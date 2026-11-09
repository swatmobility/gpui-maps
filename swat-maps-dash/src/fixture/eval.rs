//! A tree-walking evaluator for the SQL subset fixture pages compile to.
//!
//! It is not a SQL engine. It runs exactly this shape (keywords in any case):
//!
//! ```text
//! SELECT [DISTINCT] item, … FROM table | (subquery) [AS] alias
//!   [WHERE e] [GROUP BY e, …] [HAVING e] [ORDER BY e [ASC|DESC], …]
//!   [LIMIT n [OFFSET m] | LIMIT m, n]
//! ```
//!
//! with ClickHouse parameters `{name:Type}`, the scalar functions and the
//! aggregates listed on [`execute`], and ClickHouse's NULL and integer
//! rules. `JOIN`, `UNION`, `WITH`, window functions and `LIMIT BY` are
//! refused with [`EvalError::Unsupported`] naming the construct.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use super::db::{cmp_vals, column_kind, time_text, Database, Table, Val, NULL};
use super::parse::{parse, Ast, BinOp, CmpOp, FromClause, Query, SelectItem};
use crate::sql::{Param, ParamValue};

/// Why a fixture query could not run.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EvalError {
    #[error("parse error at byte {at}: {message}")]
    Parse { at: usize, message: String },
    #[error("unsupported in fixture SQL: {0}")]
    Unsupported(String),
    #[error("unknown table {0}")]
    UnknownTable(String),
    #[error("unknown column {0}")]
    UnknownColumn(String),
    #[error("unsupported function {0}()")]
    UnsupportedFunction(String),
    #[error("unknown parameter {{{0}}}")]
    UnknownParameter(String),
    #[error("invalid: {0}")]
    Invalid(String),
}

/// A query's result: `(name, ClickHouse type)` columns and rows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultSet {
    pub columns: Vec<(String, String)>,
    pub rows: Vec<Vec<Val>>,
}

impl ResultSet {
    /// The position of the column `name`.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|(n, _)| n == name)
    }
}

/// Runs one statement of the subset against `db`, reading `{name:Type}`
/// placeholders from `params` (an `Array` binds as a list).
///
/// Scalar functions: `if`, `multiIf`, `round(x[, n])` (half to even, as
/// ClickHouse rounds floats), `floor`, `ceil`, `abs`, `sqrt`, `intDiv`,
/// `greatest`, `least`, `toString`, `toInt8/16/32/64`, `toUInt8/16/32/64`,
/// `toFloat32/64`, `toDate`, `toYear`, `toMonth`, `concat`, `lower`,
/// `upper`, `length` (bytes), `substring(s, start[, len])` (characters),
/// `position(hay, needle)` (1-based character index, 0 when absent),
/// `normalizeUTF8NFKC`, `coalesce`, `ifNull`.
///
/// Aggregates: `count()`, `count(x)`, `countIf`, `sum`, `sumIf`, `avg`,
/// `min`, `max`, `any`, `argMax`, `argMin`, `uniq`, `uniqExact`, `median`,
/// `quantile(p)(x)` and `quantileExact(p)(x)`. All three quantiles
/// interpolate linearly between order statistics (deterministic, unlike
/// ClickHouse's sampled `quantile`).
pub fn execute(db: &Database, sql: &str, params: &[Param]) -> Result<ResultSet, EvalError> {
    let q = parse(sql)?;
    run(db, &q, params)
}

/// A hashable stand-in for a value: grouping, `DISTINCT` and `uniq` keys.
/// An integral float keys as the integer, so `1` and `1.0` are one group.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Null,
    I(i64),
    F(u64),
    S(String),
    L(Vec<Key>),
}

impl Key {
    fn of(v: &Val) -> Key {
        match v {
            Val::Null => Key::Null,
            Val::Int(i) => Key::I(*i),
            Val::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Key::I(*f as i64),
            Val::Float(f) => Key::F(f.to_bits()),
            Val::Str(s) => Key::S(s.clone()),
            Val::List(xs) => Key::L(xs.iter().map(Key::of).collect()),
        }
    }
}

/// A resolved expression: columns are indices, aliases are inlined,
/// parameters are literals, aggregates are slots.
#[derive(Debug, Clone)]
enum Node {
    Lit(Val),
    Col(usize),
    Agg(usize),
    Neg(Box<Node>),
    Not(Box<Node>),
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Bin(BinOp, Box<Node>, Box<Node>),
    Cmp(CmpOp, Box<Node>, Box<Node>),
    In { e: Box<Node>, list: Vec<Node>, neg: bool },
    Between { e: Box<Node>, lo: Box<Node>, hi: Box<Node>, neg: bool },
    IsNull { e: Box<Node>, neg: bool },
    Tuple(Vec<Node>),
    Func(Func, Vec<Node>),
}

fn has_agg(n: &Node) -> bool {
    match n {
        Node::Agg(_) => true,
        Node::Lit(_) | Node::Col(_) => false,
        Node::Neg(e) | Node::Not(e) | Node::IsNull { e, .. } => has_agg(e),
        Node::And(a, b) | Node::Or(a, b) | Node::Bin(_, a, b) | Node::Cmp(_, a, b) => has_agg(a) || has_agg(b),
        Node::In { e, list, .. } => has_agg(e) || list.iter().any(has_agg),
        Node::Between { e, lo, hi, .. } => has_agg(e) || has_agg(lo) || has_agg(hi),
        Node::Tuple(xs) | Node::Func(_, xs) => xs.iter().any(has_agg),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Func {
    If,
    MultiIf,
    Round,
    Floor,
    Ceil,
    Abs,
    Sqrt,
    IntDiv,
    Greatest,
    Least,
    ToString,
    /// Bits and signedness.
    ToInt(u8, bool),
    ToFloat,
    ToDate,
    ToYear,
    ToMonth,
    Concat,
    Lower,
    Upper,
    Length,
    Substring,
    Position,
    Nfkc,
    Coalesce,
    IfNull,
}

/// A scalar function by lower-cased name: the function and its arity.
fn func_of(name: &str) -> Option<(Func, usize, usize)> {
    const ANY: usize = usize::MAX;
    Some(match name {
        "if" => (Func::If, 3, 3),
        "multiif" => (Func::MultiIf, 3, ANY),
        "round" => (Func::Round, 1, 2),
        "floor" => (Func::Floor, 1, 1),
        "ceil" | "ceiling" => (Func::Ceil, 1, 1),
        "abs" => (Func::Abs, 1, 1),
        "sqrt" => (Func::Sqrt, 1, 1),
        "intdiv" => (Func::IntDiv, 2, 2),
        "greatest" => (Func::Greatest, 1, ANY),
        "least" => (Func::Least, 1, ANY),
        "tostring" => (Func::ToString, 1, 1),
        "toint8" => (Func::ToInt(8, true), 1, 1),
        "toint16" => (Func::ToInt(16, true), 1, 1),
        "toint32" => (Func::ToInt(32, true), 1, 1),
        "toint64" => (Func::ToInt(64, true), 1, 1),
        "touint8" => (Func::ToInt(8, false), 1, 1),
        "touint16" => (Func::ToInt(16, false), 1, 1),
        "touint32" => (Func::ToInt(32, false), 1, 1),
        "touint64" => (Func::ToInt(64, false), 1, 1),
        "tofloat32" | "tofloat64" => (Func::ToFloat, 1, 1),
        "todate" => (Func::ToDate, 1, 1),
        "toyear" => (Func::ToYear, 1, 1),
        "tomonth" => (Func::ToMonth, 1, 1),
        "concat" => (Func::Concat, 1, ANY),
        "lower" => (Func::Lower, 1, 1),
        "upper" => (Func::Upper, 1, 1),
        "length" => (Func::Length, 1, 1),
        "substring" | "substr" => (Func::Substring, 2, 3),
        "position" => (Func::Position, 2, 2),
        "normalizeutf8nfkc" => (Func::Nfkc, 1, 1),
        "coalesce" => (Func::Coalesce, 1, ANY),
        "ifnull" => (Func::IfNull, 2, 2),
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum AggKind {
    Count,
    CountIf,
    Sum,
    SumIf,
    Avg,
    Min,
    Max,
    Any,
    ArgMax,
    ArgMin,
    Uniq,
    /// The level, `0..=1`.
    Quantile(f64),
}

/// An aggregate by lower-cased name: the kind, its arity, and whether it
/// takes a level (`quantile(p)(x)`).
fn agg_of(name: &str) -> Option<(AggKind, usize, usize, bool)> {
    Some(match name {
        "count" => (AggKind::Count, 0, 1, false),
        "countif" => (AggKind::CountIf, 1, 1, false),
        "sum" => (AggKind::Sum, 1, 1, false),
        "sumif" => (AggKind::SumIf, 2, 2, false),
        "avg" => (AggKind::Avg, 1, 1, false),
        "min" => (AggKind::Min, 1, 1, false),
        "max" => (AggKind::Max, 1, 1, false),
        "any" => (AggKind::Any, 1, 1, false),
        "argmax" => (AggKind::ArgMax, 2, 2, false),
        "argmin" => (AggKind::ArgMin, 2, 2, false),
        "uniq" | "uniqexact" => (AggKind::Uniq, 1, 1, false),
        "median" => (AggKind::Quantile(0.5), 1, 1, false),
        "quantile" | "quantileexact" => (AggKind::Quantile(0.5), 1, 1, true),
        _ => return None,
    })
}

#[derive(Debug, Clone)]
struct AggSpec {
    kind: AggKind,
    args: Vec<Node>,
}

/// Where an identifier looks first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// `WHERE`, `GROUP BY`, aggregate arguments: a column, then a
    /// non-aggregate alias.
    Row,
    /// `SELECT` items: a column, then an alias.
    Select,
    /// `HAVING`, `ORDER BY`: an alias, then a column.
    AliasFirst,
}

/// Whether an aggregate may appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Allowed,
    InsideAgg,
    Forbidden(&'static str),
}

impl Mode {
    fn refuse(self, what: &str) -> Result<(), EvalError> {
        match self {
            Mode::Allowed => Ok(()),
            Mode::InsideAgg => Err(EvalError::Unsupported(format!("{what} inside an aggregate"))),
            Mode::Forbidden(place) => Err(EvalError::Unsupported(format!("{what} in {place}"))),
        }
    }
}

struct ItemAst {
    name: String,
    alias: Option<String>,
    expr: Ast,
}

struct Resolver<'q> {
    cols: &'q [String],
    items: &'q [ItemAst],
    params: &'q [Param],
    done: Vec<Option<Node>>,
    busy: Vec<bool>,
    aggs: Vec<AggSpec>,
}

impl Resolver<'_> {
    fn col_index(&self, name: &str) -> Option<usize> {
        self.cols.iter().position(|c| c == name).or_else(|| {
            let (_, last) = name.rsplit_once('.')?;
            self.cols.iter().position(|c| c == last)
        })
    }

    fn alias_index(&self, name: &str) -> Option<usize> {
        self.items.iter().position(|i| i.alias.as_deref() == Some(name))
    }

    /// Item `i`'s expression, resolved once in `SELECT` scope.
    fn item(&mut self, i: usize) -> Result<Node, EvalError> {
        if let Some(n) = self.done.get(i).and_then(Option::as_ref) {
            return Ok(n.clone());
        }
        let items = self.items;
        let Some(it) = items.get(i) else {
            return Err(EvalError::Invalid(format!("there is no SELECT item {}", i + 1)));
        };
        if self.busy[i] {
            return Err(EvalError::Unsupported(format!("the alias {} refers to itself", it.name)));
        }
        self.busy[i] = true;
        let n = self.resolve(&it.expr, Scope::Select, Mode::Allowed);
        self.busy[i] = false;
        let n = n?;
        self.done[i] = Some(n.clone());
        Ok(n)
    }

    fn ident(&mut self, name: &str, scope: Scope, mode: Mode) -> Result<Node, EvalError> {
        let col = self.col_index(name);
        let alias = self.alias_index(name).filter(|i| !self.busy[*i] || col.is_none());
        let pick = match (scope, col, alias) {
            (_, _, None) => None,
            (Scope::AliasFirst, _, Some(i)) => Some(i),
            (_, None, Some(i)) => Some(i),
            _ => None,
        };
        if let Some(i) = pick {
            let n = self.item(i)?;
            if has_agg(&n) {
                mode.refuse(&format!("the aggregate alias {name}"))?;
            }
            return Ok(n);
        }
        col.map(Node::Col).ok_or_else(|| EvalError::UnknownColumn(name.to_string()))
    }

    fn param(&self, name: &str) -> Result<Node, EvalError> {
        let p = self.params.iter().find(|p| p.name == name).ok_or_else(|| EvalError::UnknownParameter(name.to_string()))?;
        Ok(Node::Lit(match &p.value {
            ParamValue::One(s) => Val::from_scalar(s),
            ParamValue::Array(a) => Val::List(a.iter().map(Val::from_scalar).collect()),
        }))
    }

    fn boxed(&mut self, a: &Ast, scope: Scope, mode: Mode) -> Result<Box<Node>, EvalError> {
        self.resolve(a, scope, mode).map(Box::new)
    }

    fn resolve(&mut self, a: &Ast, scope: Scope, mode: Mode) -> Result<Node, EvalError> {
        Ok(match a {
            Ast::Lit(v) => Node::Lit(v.clone()),
            Ast::Ident(name) => self.ident(name, scope, mode)?,
            Ast::Param(name) => self.param(name)?,
            Ast::Neg(e) => Node::Neg(self.boxed(e, scope, mode)?),
            Ast::Not(e) => Node::Not(self.boxed(e, scope, mode)?),
            Ast::And(x, y) => Node::And(self.boxed(x, scope, mode)?, self.boxed(y, scope, mode)?),
            Ast::Or(x, y) => Node::Or(self.boxed(x, scope, mode)?, self.boxed(y, scope, mode)?),
            Ast::Bin(op, x, y) => Node::Bin(*op, self.boxed(x, scope, mode)?, self.boxed(y, scope, mode)?),
            Ast::Cmp(op, x, y) => Node::Cmp(*op, self.boxed(x, scope, mode)?, self.boxed(y, scope, mode)?),
            Ast::In { e, list, neg } => Node::In {
                e: self.boxed(e, scope, mode)?,
                list: list.iter().map(|x| self.resolve(x, scope, mode)).collect::<Result<_, _>>()?,
                neg: *neg,
            },
            Ast::Between { e, lo, hi, neg } => Node::Between {
                e: self.boxed(e, scope, mode)?,
                lo: self.boxed(lo, scope, mode)?,
                hi: self.boxed(hi, scope, mode)?,
                neg: *neg,
            },
            Ast::IsNull { e, neg } => Node::IsNull { e: self.boxed(e, scope, mode)?, neg: *neg },
            Ast::Tuple(xs) => Node::Tuple(xs.iter().map(|x| self.resolve(x, scope, mode)).collect::<Result<_, _>>()?),
            Ast::Call { name, params, args, star } => self.call(name, params.as_deref(), args, *star, scope, mode)?,
        })
    }

    fn call(
        &mut self,
        name: &str,
        params: Option<&[Ast]>,
        args: &[Ast],
        star: bool,
        scope: Scope,
        mode: Mode,
    ) -> Result<Node, EvalError> {
        let lname = name.to_ascii_lowercase();
        let arity = |min: usize, max: usize| {
            if args.len() < min || args.len() > max {
                let want = if min == max { min.to_string() } else if max == usize::MAX { format!("at least {min}") } else { format!("{min} to {max}") };
                Err(EvalError::Invalid(format!("{name}() takes {want} arguments, got {}", args.len())))
            } else {
                Ok(())
            }
        };
        if let Some((kind, min, max, levelled)) = agg_of(&lname) {
            mode.refuse(&format!("the aggregate {name}()"))?;
            if star && kind != AggKind::Count {
                return Err(EvalError::Invalid(format!("{name}(*) is not supported")));
            }
            arity(min, max)?;
            let kind = match (levelled, params) {
                (true, Some([p])) => AggKind::Quantile(level(p)?),
                (true, None) => kind,
                (true, Some(_)) => return Err(EvalError::Invalid(format!("{name}() takes one level"))),
                (false, Some(_)) => return Err(EvalError::Unsupported(format!("the parametric aggregate {name}(…)(…)"))),
                (false, None) => kind,
            };
            let args = args.iter().map(|x| self.resolve(x, Scope::Row, Mode::InsideAgg)).collect::<Result<Vec<_>, _>>()?;
            self.aggs.push(AggSpec { kind, args });
            return Ok(Node::Agg(self.aggs.len() - 1));
        }
        let Some((f, min, max)) = func_of(&lname) else {
            return Err(EvalError::UnsupportedFunction(name.to_string()));
        };
        if params.is_some() || star {
            return Err(EvalError::Unsupported(format!("{name}() with parameters or *")));
        }
        arity(min, max)?;
        if f == Func::MultiIf && args.len().is_multiple_of(2) {
            return Err(EvalError::Invalid("multiIf() takes condition/value pairs and an else value".into()));
        }
        let args = args.iter().map(|x| self.resolve(x, scope, mode)).collect::<Result<Vec<_>, _>>()?;
        Ok(Node::Func(f, args))
    }
}

/// A quantile level: a numeric literal in `0..=1`.
fn level(p: &Ast) -> Result<f64, EvalError> {
    let v = match p {
        Ast::Lit(Val::Int(i)) => *i as f64,
        Ast::Lit(Val::Float(f)) => *f,
        _ => return Err(EvalError::Unsupported("a quantile level that is not a number literal".into())),
    };
    if (0.0..=1.0).contains(&v) {
        Ok(v)
    } else {
        Err(EvalError::Invalid(format!("the quantile level {v} is outside 0..1")))
    }
}

/// The input of one query: the column names and types, and the rows.
struct Input<'a> {
    names: Vec<String>,
    types: Vec<String>,
    rows: &'a [Vec<Val>],
}

fn table<'a>(db: &'a Database, name: &str) -> Result<&'a Table, EvalError> {
    db.table(name)
        .or_else(|| name.rsplit_once('.').and_then(|(_, last)| db.table(last)))
        .ok_or_else(|| EvalError::UnknownTable(name.to_string()))
}

fn run(db: &Database, q: &Query, params: &[Param]) -> Result<ResultSet, EvalError> {
    let sub: ResultSet;
    let input = match &q.from {
        FromClause::Table(name) => {
            let t = table(db, name)?;
            Input {
                names: t.columns.iter().map(|(n, _)| n.clone()).collect(),
                types: t.columns.iter().map(|(_, ty)| ty.clone()).collect(),
                rows: &t.rows,
            }
        }
        FromClause::Sub(s) => {
            sub = run(db, s, params)?;
            Input {
                names: sub.columns.iter().map(|(n, _)| n.clone()).collect(),
                types: sub.columns.iter().map(|(_, ty)| ty.clone()).collect(),
                rows: &sub.rows,
            }
        }
    };
    let rows = input.rows;

    // `*` expands to every input column.
    let mut items = Vec::new();
    for it in &q.items {
        match it {
            SelectItem::Star => {
                items.extend(input.names.iter().map(|n| ItemAst { name: n.clone(), alias: None, expr: Ast::Ident(n.clone()) }))
            }
            SelectItem::Expr { expr, alias, name } => items.push(ItemAst { name: name.clone(), alias: alias.clone(), expr: expr.clone() }),
        }
    }

    let mut r = Resolver {
        cols: &input.names,
        items: &items,
        params,
        done: vec![None; items.len()],
        busy: vec![false; items.len()],
        aggs: Vec::new(),
    };
    let positional = |a: &Ast| match a {
        Ast::Lit(Val::Int(k)) if *k >= 1 => usize::try_from(*k - 1).ok(),
        _ => None,
    };
    let select = (0..items.len()).map(|i| r.item(i)).collect::<Result<Vec<_>, _>>()?;
    let where_ = q.where_.as_ref().map(|w| r.resolve(w, Scope::Row, Mode::Forbidden("WHERE"))).transpose()?;
    let mut group = Vec::new();
    for g in &q.group_by {
        group.push(match positional(g) {
            Some(i) => r.item(i)?,
            None => r.resolve(g, Scope::Row, Mode::Forbidden("GROUP BY"))?,
        });
    }
    let having = q.having.as_ref().map(|h| r.resolve(h, Scope::AliasFirst, Mode::Allowed)).transpose()?;
    let mut order = Vec::new();
    for (o, desc) in &q.order_by {
        let n = match positional(o) {
            Some(i) => r.item(i)?,
            None => r.resolve(o, Scope::AliasFirst, Mode::Allowed)?,
        };
        order.push((n, *desc));
    }
    let aggs = std::mem::take(&mut r.aggs);
    let grouped = !group.is_empty() || !aggs.is_empty();

    let mut kept = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        if let Some(w) = &where_ {
            if !truthy(&*eval(w, &Ctx { row: Some(row.as_slice()), aggs: &[] })?) {
                continue;
            }
        }
        kept.push(i);
    }

    // (output row, ORDER BY keys)
    let mut out: Vec<(Vec<Val>, Vec<Val>)> = Vec::new();
    let mut emit = |cx: &Ctx<'_>| -> Result<(), EvalError> {
        if let Some(h) = &having {
            if !truthy(&*eval(h, cx)?) {
                return Ok(());
            }
        }
        let vals = select.iter().map(|n| eval(n, cx).map(Cow::into_owned)).collect::<Result<Vec<_>, _>>()?;
        let keys = order.iter().map(|(n, _)| eval(n, cx).map(Cow::into_owned)).collect::<Result<Vec<_>, _>>()?;
        out.push((vals, keys));
        Ok(())
    };

    if grouped {
        let mut index: HashMap<Vec<Key>, usize> = HashMap::new();
        let mut groups: Vec<(Option<usize>, Vec<AggState>)> = Vec::new();
        for &ri in &kept {
            let cx = Ctx { row: Some(rows[ri].as_slice()), aggs: &[] };
            let gi = if group.is_empty() {
                if groups.is_empty() {
                    groups.push((Some(ri), aggs.iter().map(AggState::new).collect()));
                }
                0
            } else {
                let key = group.iter().map(|g| eval(g, &cx).map(|v| Key::of(&v))).collect::<Result<Vec<_>, _>>()?;
                match index.get(&key) {
                    Some(&g) => g,
                    None => {
                        groups.push((Some(ri), aggs.iter().map(AggState::new).collect()));
                        index.insert(key, groups.len() - 1);
                        groups.len() - 1
                    }
                }
            };
            for (spec, st) in aggs.iter().zip(groups[gi].1.iter_mut()) {
                st.update(spec, &cx)?;
            }
        }
        // Aggregates without GROUP BY give one row, even over no rows.
        if group.is_empty() && groups.is_empty() {
            groups.push((None, aggs.iter().map(AggState::new).collect()));
        }
        for (first, states) in groups {
            let vals: Vec<Val> = aggs.iter().zip(states).map(|(spec, st)| st.finish(spec.kind)).collect();
            emit(&Ctx { row: first.map(|i| rows[i].as_slice()), aggs: &vals })?;
        }
    } else {
        for &ri in &kept {
            emit(&Ctx { row: Some(rows[ri].as_slice()), aggs: &[] })?;
        }
    }

    if q.distinct {
        let mut seen: HashSet<Vec<Key>> = HashSet::new();
        out.retain(|(vals, _)| seen.insert(vals.iter().map(Key::of).collect()));
    }
    if !order.is_empty() {
        out.sort_by(|(_, a), (_, b)| {
            for ((x, y), (_, desc)) in a.iter().zip(b).zip(&order) {
                let o = match (x.is_null(), y.is_null()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) if *desc => cmp_vals(y, x),
                    (false, false) => cmp_vals(x, y),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        });
    }
    let rows_out: Vec<Vec<Val>> = out.into_iter().skip(q.offset).take(q.limit.unwrap_or(usize::MAX)).map(|(v, _)| v).collect();

    let columns = items
        .iter()
        .zip(&select)
        .enumerate()
        .map(|(c, (it, n))| {
            let ty = match n {
                Node::Col(i) => input.types.get(*i).cloned(),
                _ => None,
            };
            let ty = ty.unwrap_or_else(|| {
                let kind = column_kind(rows_out.iter().map(|r| r.get(c).unwrap_or(&NULL)));
                let elems = rows_out.iter().filter_map(|r| match r.get(c) {
                    Some(Val::List(xs)) => Some(xs.iter()),
                    _ => None,
                });
                kind.sql_type(column_kind(elems.flatten()).base)
            });
            (it.name.clone(), ty)
        })
        .collect();
    Ok(ResultSet { columns, rows: rows_out })
}

/// What an expression reads: the current row (the group's first row when
/// grouped; none for an empty aggregate) and the group's aggregate values.
struct Ctx<'a> {
    row: Option<&'a [Val]>,
    aggs: &'a [Val],
}

fn int(b: bool) -> Val {
    Val::Int(i64::from(b))
}

/// ClickHouse truthiness: a non-zero number, a non-empty list; `NULL` is false.
fn truthy(v: &Val) -> bool {
    match v {
        Val::Null => false,
        Val::Int(i) => *i != 0,
        Val::Float(f) => *f != 0.0,
        Val::Str(s) => !(s.is_empty() || s == "0" || s.eq_ignore_ascii_case("false")),
        Val::List(xs) => !xs.is_empty(),
    }
}

#[derive(Debug, Clone, Copy)]
enum Num {
    I(i64),
    F(f64),
}

impl Num {
    fn f(self) -> f64 {
        match self {
            Num::I(i) => i as f64,
            Num::F(f) => f,
        }
    }
}

/// A value as a number: `None` for `Null`; an error for text that is not one.
fn num(v: &Val, what: &str) -> Result<Option<Num>, EvalError> {
    match v {
        Val::Null => Ok(None),
        Val::Int(i) => Ok(Some(Num::I(*i))),
        Val::Float(f) => Ok(Some(Num::F(*f))),
        Val::Str(s) => {
            let t = s.trim();
            if let Ok(i) = t.parse::<i64>() {
                Ok(Some(Num::I(i)))
            } else if let Ok(f) = t.parse::<f64>() {
                Ok(Some(Num::F(f)))
            } else {
                Err(EvalError::Invalid(format!("{what} needs a number, got '{s}'")))
            }
        }
        Val::List(_) => Err(EvalError::Invalid(format!("{what} needs a number, got a list"))),
    }
}

/// A float, or `NULL` when it is infinite or NaN.
fn float(f: f64) -> Val {
    if f.is_finite() {
        Val::Float(f)
    } else {
        Val::Null
    }
}

fn arith(op: BinOp, a: &Val, b: &Val) -> Result<Val, EvalError> {
    if a.is_null() || b.is_null() {
        return Ok(Val::Null);
    }
    if op == BinOp::Concat {
        return Ok(Val::Str(a.text() + &b.text()));
    }
    let (Some(x), Some(y)) = (num(a, "arithmetic")?, num(b, "arithmetic")?) else {
        return Ok(Val::Null);
    };
    Ok(match (op, x, y) {
        (BinOp::Add, Num::I(x), Num::I(y)) => Val::Int(x.wrapping_add(y)),
        (BinOp::Sub, Num::I(x), Num::I(y)) => Val::Int(x.wrapping_sub(y)),
        (BinOp::Mul, Num::I(x), Num::I(y)) => Val::Int(x.wrapping_mul(y)),
        (BinOp::Mod, Num::I(_), Num::I(0)) => Val::Null,
        (BinOp::Mod, Num::I(x), Num::I(y)) => Val::Int(x.wrapping_rem(y)),
        (BinOp::Div, _, _) if y.f() == 0.0 => Val::Null,
        (BinOp::Add, _, _) => float(x.f() + y.f()),
        (BinOp::Sub, _, _) => float(x.f() - y.f()),
        (BinOp::Mul, _, _) => float(x.f() * y.f()),
        (BinOp::Div, _, _) => float(x.f() / y.f()),
        (BinOp::Mod, _, _) => float(x.f() % y.f()),
        (BinOp::Concat, _, _) => Val::Null,
    })
}

/// Two non-null values are equal under [`cmp_vals`].
fn same(a: &Val, b: &Val) -> bool {
    !a.is_null() && !b.is_null() && cmp_vals(a, b) == Ordering::Equal
}

fn eval<'a>(n: &'a Node, cx: &Ctx<'a>) -> Result<Cow<'a, Val>, EvalError> {
    Ok(match n {
        Node::Lit(v) => Cow::Borrowed(v),
        Node::Col(i) => Cow::Borrowed(cx.row.and_then(|r| r.get(*i)).unwrap_or(&NULL)),
        Node::Agg(i) => Cow::Borrowed(cx.aggs.get(*i).unwrap_or(&NULL)),
        Node::Neg(e) => Cow::Owned(match num(&*eval(e, cx)?, "unary minus")? {
            None => Val::Null,
            Some(Num::I(i)) => Val::Int(i.wrapping_neg()),
            Some(Num::F(f)) => Val::Float(-f),
        }),
        Node::Not(e) => {
            let v = eval(e, cx)?;
            Cow::Owned(if v.is_null() { Val::Null } else { int(!truthy(&v)) })
        }
        Node::And(a, b) => {
            let x = eval(a, cx)?;
            if !x.is_null() && !truthy(&x) {
                return Ok(Cow::Owned(Val::Int(0)));
            }
            let y = eval(b, cx)?;
            Cow::Owned(if !y.is_null() && !truthy(&y) {
                Val::Int(0)
            } else if x.is_null() || y.is_null() {
                Val::Null
            } else {
                Val::Int(1)
            })
        }
        Node::Or(a, b) => {
            let x = eval(a, cx)?;
            if truthy(&x) {
                return Ok(Cow::Owned(Val::Int(1)));
            }
            let y = eval(b, cx)?;
            Cow::Owned(if truthy(&y) {
                Val::Int(1)
            } else if x.is_null() || y.is_null() {
                Val::Null
            } else {
                Val::Int(0)
            })
        }
        Node::Bin(op, a, b) => Cow::Owned(arith(*op, &*eval(a, cx)?, &*eval(b, cx)?)?),
        Node::Cmp(op, a, b) => {
            let (x, y) = (eval(a, cx)?, eval(b, cx)?);
            if x.is_null() || y.is_null() {
                return Ok(Cow::Owned(Val::Int(0)));
            }
            let o = cmp_vals(&x, &y);
            Cow::Owned(int(match op {
                CmpOp::Eq => o == Ordering::Equal,
                CmpOp::Ne => o != Ordering::Equal,
                CmpOp::Lt => o == Ordering::Less,
                CmpOp::Le => o != Ordering::Greater,
                CmpOp::Gt => o == Ordering::Greater,
                CmpOp::Ge => o != Ordering::Less,
            }))
        }
        Node::In { e, list, neg } => {
            let v = eval(e, cx)?;
            if v.is_null() {
                return Ok(Cow::Owned(Val::Int(0)));
            }
            let mut found = false;
            for item in list {
                let x = eval(item, cx)?;
                found = match &*x {
                    Val::List(xs) => xs.iter().any(|y| same(&v, y)),
                    other => same(&v, other),
                };
                if found {
                    break;
                }
            }
            Cow::Owned(int(found != *neg))
        }
        Node::Between { e, lo, hi, neg } => {
            let (v, l, h) = (eval(e, cx)?, eval(lo, cx)?, eval(hi, cx)?);
            if v.is_null() || l.is_null() || h.is_null() {
                return Ok(Cow::Owned(Val::Int(0)));
            }
            let inside = cmp_vals(&v, &l) != Ordering::Less && cmp_vals(&v, &h) != Ordering::Greater;
            Cow::Owned(int(inside != *neg))
        }
        Node::IsNull { e, neg } => Cow::Owned(int(eval(e, cx)?.is_null() != *neg)),
        Node::Tuple(xs) => Cow::Owned(Val::List(xs.iter().map(|x| eval(x, cx).map(Cow::into_owned)).collect::<Result<_, _>>()?)),
        Node::Func(f, args) => call(*f, args, cx)?,
    })
}

fn call<'a>(f: Func, args: &'a [Node], cx: &Ctx<'a>) -> Result<Cow<'a, Val>, EvalError> {
    // The lazy ones first: they evaluate only the branch they take.
    match f {
        Func::If => {
            return if truthy(&*eval(&args[0], cx)?) { eval(&args[1], cx) } else { eval(&args[2], cx) };
        }
        Func::MultiIf => {
            let mut i = 0;
            while i + 1 < args.len() {
                if truthy(&*eval(&args[i], cx)?) {
                    return eval(&args[i + 1], cx);
                }
                i += 2;
            }
            return match args.last() {
                Some(e) => eval(e, cx),
                None => Ok(Cow::Owned(Val::Null)),
            };
        }
        Func::Coalesce | Func::IfNull => {
            for a in args {
                let v = eval(a, cx)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            return Ok(Cow::Owned(Val::Null));
        }
        _ => {}
    }
    let vals = args.iter().map(|a| eval(a, cx)).collect::<Result<Vec<_>, _>>()?;
    let a0: &Val = vals.first().map_or(&NULL, |v| &**v);
    let a1: &Val = vals.get(1).map_or(&NULL, |v| &**v);
    if a0.is_null() && !matches!(f, Func::Concat | Func::Greatest | Func::Least) {
        return Ok(Cow::Owned(Val::Null));
    }
    let name = match f {
        Func::Floor => "floor",
        Func::Ceil => "ceil",
        Func::Abs => "abs",
        Func::Sqrt => "sqrt",
        Func::ToInt(..) => "toInt",
        Func::ToFloat => "toFloat",
        Func::ToYear => "toYear",
        Func::ToMonth => "toMonth",
        _ => "a function",
    };
    Ok(Cow::Owned(match f {
        Func::Round => {
            let places = match num(a1, "round")? {
                Some(Num::I(n)) => n.clamp(-18, 18) as i32,
                Some(Num::F(n)) => n.clamp(-18.0, 18.0) as i32,
                None => 0,
            };
            match num(a0, "round")? {
                None => Val::Null,
                Some(Num::I(i)) if places >= 0 => Val::Int(i),
                Some(Num::I(i)) => {
                    let p = 10f64.powi(-places);
                    Val::Int(((i as f64 / p).round_ties_even() * p) as i64)
                }
                Some(Num::F(x)) => {
                    let m = 10f64.powi(places);
                    float((x * m).round_ties_even() / m)
                }
            }
        }
        Func::Floor | Func::Ceil | Func::Abs | Func::Sqrt => match num(a0, name)? {
            None => Val::Null,
            Some(Num::I(i)) => match f {
                Func::Abs => Val::Int(i.wrapping_abs()),
                Func::Sqrt => float((i as f64).sqrt()),
                _ => Val::Int(i),
            },
            Some(Num::F(x)) => float(match f {
                Func::Floor => x.floor(),
                Func::Ceil => x.ceil(),
                Func::Abs => x.abs(),
                _ => x.sqrt(),
            }),
        },
        Func::IntDiv => match (num(a0, "intDiv")?, num(a1, "intDiv")?) {
            (Some(_), Some(Num::I(0))) => Val::Null,
            (Some(Num::I(x)), Some(Num::I(y))) => Val::Int(x.wrapping_div(y)),
            (Some(x), Some(y)) if y.f() != 0.0 => {
                let q = (x.f() / y.f()).trunc();
                if q.is_finite() {
                    Val::Int(q as i64)
                } else {
                    Val::Null
                }
            }
            _ => Val::Null,
        },
        Func::Greatest | Func::Least => {
            if vals.iter().any(|v| v.is_null()) {
                Val::Null
            } else {
                let want = if f == Func::Greatest { Ordering::Greater } else { Ordering::Less };
                let mut best = a0;
                for v in vals[1..].iter().map(|v| &**v) {
                    if cmp_vals(v, best) == want {
                        best = v;
                    }
                }
                match best {
                    Val::Int(i) if vals.iter().any(|v| matches!(**v, Val::Float(_))) => Val::Float(*i as f64),
                    other => other.clone(),
                }
            }
        }
        Func::ToString => Val::Str(a0.text()),
        Func::ToInt(bits, signed) => {
            let i = match num(a0, name)? {
                None => return Ok(Cow::Owned(Val::Null)),
                Some(Num::I(i)) => i,
                Some(Num::F(x)) => x.trunc() as i64,
            };
            Val::Int(match (bits, signed) {
                (8, true) => i64::from(i as i8),
                (16, true) => i64::from(i as i16),
                (32, true) => i64::from(i as i32),
                (8, false) => i64::from(i as u8),
                (16, false) => i64::from(i as u16),
                (32, false) => i64::from(i as u32),
                _ => i,
            })
        }
        Func::ToFloat => match num(a0, name)? {
            None => Val::Null,
            Some(n) => float(n.f()),
        },
        Func::ToDate => match a0 {
            Val::Str(s) => Val::Str(s.get(..10).unwrap_or(s).to_string()),
            other => match num(other, "toDate")? {
                None => Val::Null,
                Some(n) => Val::Str(time_text((n.f().trunc() as i64).saturating_mul(86_400_000))),
            },
        },
        Func::ToYear | Func::ToMonth => {
            let s = a0.text();
            let part = if f == Func::ToYear { s.get(0..4) } else { s.get(5..7) };
            match part.and_then(|p| p.parse::<i64>().ok()) {
                Some(v) => Val::Int(v),
                None => return Err(EvalError::Invalid(format!("{name} needs a date 'YYYY-MM-DD', got '{s}'"))),
            }
        }
        Func::Concat => {
            if vals.iter().any(|v| v.is_null()) {
                Val::Null
            } else {
                Val::Str(vals.iter().map(|v| v.text()).collect())
            }
        }
        Func::Lower => Val::Str(a0.text().to_ascii_lowercase()),
        Func::Upper => Val::Str(a0.text().to_ascii_uppercase()),
        Func::Length => match a0 {
            Val::List(xs) => Val::Int(xs.len() as i64),
            other => Val::Int(other.text().len() as i64),
        },
        Func::Substring => {
            let s = a0.text();
            let n = s.chars().count() as i64;
            let start = match num(a1, "substring")? {
                Some(v) => v.f() as i64,
                None => return Ok(Cow::Owned(Val::Null)),
            };
            let from = match start {
                0 => n,
                k if k > 0 => k - 1,
                k => (n + k).max(0),
            };
            let len = match vals.get(2) {
                Some(v) => match num(v, "substring")? {
                    Some(l) => (l.f() as i64).max(0),
                    None => return Ok(Cow::Owned(Val::Null)),
                },
                None => i64::MAX,
            };
            let skip = usize::try_from(from).unwrap_or(usize::MAX);
            let take = usize::try_from(len).unwrap_or(usize::MAX);
            Val::Str(s.chars().skip(skip).take(take).collect())
        }
        Func::Position => {
            if a1.is_null() {
                Val::Null
            } else {
                let (hay, needle) = (a0.text(), a1.text());
                Val::Int(hay.find(&needle).map_or(0, |b| hay[..b].chars().count() as i64 + 1))
            }
        }
        Func::Nfkc => Val::Str(crate::format::nfkc(&a0.text())),
        Func::If | Func::MultiIf | Func::Coalesce | Func::IfNull => Val::Null,
    }))
}

/// Aggregate argument `i` in the row `cx`, `NULL` when absent.
fn agg_arg<'a>(spec: &'a AggSpec, i: usize, cx: &Ctx<'a>) -> Result<Cow<'a, Val>, EvalError> {
    match spec.args.get(i) {
        Some(n) => eval(n, cx),
        None => Ok(Cow::Borrowed(&NULL)),
    }
}

/// One aggregate's running state.
#[derive(Debug, Clone)]
enum AggState {
    Count(i64),
    Sum { i: i64, f: f64, float: bool },
    Avg { sum: f64, n: u64 },
    Best(Option<Val>),
    Arg { a: Val, b: Option<Val> },
    Uniq(HashSet<Key>),
    Values(Vec<f64>),
}

impl AggState {
    fn new(spec: &AggSpec) -> AggState {
        match spec.kind {
            AggKind::Count | AggKind::CountIf => AggState::Count(0),
            AggKind::Sum | AggKind::SumIf => AggState::Sum { i: 0, f: 0.0, float: false },
            AggKind::Avg => AggState::Avg { sum: 0.0, n: 0 },
            AggKind::Min | AggKind::Max | AggKind::Any => AggState::Best(None),
            AggKind::ArgMax | AggKind::ArgMin => AggState::Arg { a: Val::Null, b: None },
            AggKind::Uniq => AggState::Uniq(HashSet::new()),
            AggKind::Quantile(_) => AggState::Values(Vec::new()),
        }
    }

    fn update(&mut self, spec: &AggSpec, cx: &Ctx<'_>) -> Result<(), EvalError> {
        let arg = |i: usize| agg_arg(spec, i, cx);
        match (spec.kind, self) {
            (AggKind::Count, AggState::Count(n)) => {
                if spec.args.is_empty() || !arg(0)?.is_null() {
                    *n += 1;
                }
            }
            (AggKind::CountIf, AggState::Count(n)) => {
                if truthy(&*arg(0)?) {
                    *n += 1;
                }
            }
            (kind @ (AggKind::Sum | AggKind::SumIf), AggState::Sum { i, f, float }) => {
                if kind == AggKind::SumIf && !truthy(&*arg(1)?) {
                    return Ok(());
                }
                match num(&*arg(0)?, "sum")? {
                    None => {}
                    Some(Num::I(x)) => {
                        *i = i.wrapping_add(x);
                        *f += x as f64;
                    }
                    Some(Num::F(x)) => {
                        *float = true;
                        *f += x;
                    }
                }
            }
            (AggKind::Avg, AggState::Avg { sum, n }) => {
                if let Some(x) = num(&*arg(0)?, "avg")? {
                    *sum += x.f();
                    *n += 1;
                }
            }
            (kind @ (AggKind::Min | AggKind::Max | AggKind::Any), AggState::Best(best)) => {
                let v = arg(0)?;
                if v.is_null() {
                    return Ok(());
                }
                let replace = match (kind, best.as_ref()) {
                    (_, None) => true,
                    (AggKind::Min, Some(b)) => cmp_vals(&v, b) == Ordering::Less,
                    (AggKind::Max, Some(b)) => cmp_vals(&v, b) == Ordering::Greater,
                    _ => false,
                };
                if replace {
                    *best = Some(v.into_owned());
                }
            }
            (kind @ (AggKind::ArgMax | AggKind::ArgMin), AggState::Arg { a, b }) => {
                let key = arg(1)?;
                if key.is_null() {
                    return Ok(());
                }
                let want = if kind == AggKind::ArgMax { Ordering::Greater } else { Ordering::Less };
                if b.as_ref().is_none_or(|cur| cmp_vals(&key, cur) == want) {
                    *a = arg(0)?.into_owned();
                    *b = Some(key.into_owned());
                }
            }
            (AggKind::Uniq, AggState::Uniq(set)) => {
                let v = arg(0)?;
                if !v.is_null() {
                    set.insert(Key::of(&v));
                }
            }
            (AggKind::Quantile(_), AggState::Values(xs)) => {
                if let Some(x) = num(&*arg(0)?, "quantile")? {
                    xs.push(x.f());
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self, kind: AggKind) -> Val {
        match self {
            AggState::Count(n) => Val::Int(n),
            AggState::Sum { i, f, float } => {
                if float {
                    self::float(f)
                } else {
                    Val::Int(i)
                }
            }
            AggState::Avg { sum, n } => {
                if n == 0 {
                    Val::Null
                } else {
                    float(sum / n as f64)
                }
            }
            AggState::Best(v) => v.unwrap_or(Val::Null),
            AggState::Arg { a, .. } => a,
            AggState::Uniq(set) => Val::Int(set.len() as i64),
            AggState::Values(mut xs) => {
                let AggKind::Quantile(p) = kind else { return Val::Null };
                if xs.is_empty() {
                    return Val::Null;
                }
                xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
                let pos = p * (xs.len() - 1) as f64;
                let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
                let (a, b) = (xs[lo], xs[hi.min(xs.len() - 1)]);
                float(a + (b - a) * (pos - lo as f64))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::Scalar;

    fn s(v: &str) -> Val {
        Val::Str(v.into())
    }

    /// `t(stop, hour, n, d)`: three stops over two days.
    fn db() -> Database {
        let mut t = Table::new("t", &[("stop", "String"), ("hour", "Int8"), ("n", "UInt32"), ("d", "Date")]);
        for (stop, hour, n, d) in [
            ("Central Sta.", 7, 10, "2026-04-01"),
            ("Central Sta.", 8, 30, "2026-04-01"),
            ("Riverside", 7, 5, "2026-04-01"),
            ("Riverside", 9, 15, "2026-04-02"),
            ("Uplands", 8, 2, "2026-04-02"),
            ("Central Sta.", 9, 20, "2026-04-02"),
        ] {
            t.push(vec![s(stop), Val::Int(hour), Val::Int(n), s(d)]);
        }
        let mut db = Database::new();
        db.insert(t);
        db
    }

    fn one(name: &str, v: Scalar) -> Param {
        Param { name: name.into(), ty: "String".into(), value: ParamValue::One(v) }
    }

    fn rows(sql: &str, params: &[Param]) -> Vec<Vec<Val>> {
        execute(&db(), sql, params).unwrap_or_else(|e| panic!("{sql}: {e}")).rows
    }

    #[test]
    fn a_group_by_sum_orders_by_its_alias_and_limits() {
        let rs = execute(&db(), "SELECT stop, sum(n) AS passengers FROM t GROUP BY stop ORDER BY passengers DESC LIMIT 2", &[]).unwrap();
        assert_eq!(rs.columns, vec![("stop".to_string(), "String".to_string()), ("passengers".to_string(), "Int64".to_string())]);
        assert_eq!(rs.rows, vec![vec![s("Central Sta."), Val::Int(60)], vec![s("Riverside"), Val::Int(20)]]);
    }

    #[test]
    fn having_filters_on_an_alias() {
        let r = rows("SELECT stop, sum(n) AS p FROM t GROUP BY stop HAVING p >= 20 ORDER BY stop", &[]);
        assert_eq!(r, vec![vec![s("Central Sta."), Val::Int(60)], vec![s("Riverside"), Val::Int(20)]]);
    }

    #[test]
    fn a_where_in_param_narrows_the_groups() {
        let p = Param {
            name: "f_stop".into(),
            ty: "Array(String)".into(),
            value: ParamValue::Array(vec![Scalar::Str("Uplands".into()), Scalar::Str("Riverside".into())]),
        };
        let r = rows("SELECT stop, count() AS c FROM t WHERE stop IN {f_stop:Array(String)} GROUP BY stop ORDER BY stop", &[p]);
        assert_eq!(r, vec![vec![s("Riverside"), Val::Int(2)], vec![s("Uplands"), Val::Int(1)]]);
    }

    #[test]
    fn between_date_params_and_a_normalised_contains() {
        let params = [
            one("f_d_lo", Scalar::Str("2026-04-02".into())),
            one("f_d_hi", Scalar::Str("2026-04-30".into())),
            // Full-width "Ｓｔａ" normalises to "Sta" on both sides.
            one("f_s", Scalar::Str(crate::format::nfkc("Ｓｔａ"))),
        ];
        let r = rows(
            "SELECT stop, sum(n) AS p FROM t WHERE d BETWEEN {f_d_lo:Date} AND {f_d_hi:Date} \
             AND position(normalizeUTF8NFKC(stop), {f_s:String}) > 0 GROUP BY stop",
            &params,
        );
        assert_eq!(r, vec![vec![s("Central Sta."), Val::Int(20)]]);
    }

    #[test]
    fn an_aggregate_over_no_rows_is_one_row_of_zero() {
        let r = rows("SELECT count() AS c, sum(n) AS s, avg(n) AS a FROM t WHERE hour > 23", &[]);
        assert_eq!(r, vec![vec![Val::Int(0), Val::Int(0), Val::Null]]);
    }

    #[test]
    fn aggregates_nest_inside_scalar_expressions() {
        // 82 passengers over 2 distinct days.
        let r = rows("SELECT round(sum(n) / greatest(uniqExact(d), 1), 2) AS per_day FROM t", &[]);
        assert_eq!(r, vec![vec![Val::Float(41.0)]]);
        let r = rows("SELECT stop, round(sum(n) / 3, 2) AS x FROM t WHERE stop = 'Riverside' GROUP BY stop", &[]);
        assert_eq!(r, vec![vec![s("Riverside"), Val::Float(6.67)]]);
    }

    #[test]
    fn quantiles_interpolate_between_order_statistics() {
        // n sorted: 2 5 10 15 20 30
        let r = rows("SELECT quantile(0.5)(n), quantileExact(0.25)(n), median(n) FROM t", &[]);
        assert_eq!(r, vec![vec![Val::Float(12.5), Val::Float(6.25), Val::Float(12.5)]]);
    }

    #[test]
    fn an_if_alias_groups() {
        let r = rows(
            "SELECT if(hour < 8, 'early', 'late') AS bucket, sum(n) AS p FROM t GROUP BY bucket ORDER BY bucket",
            &[],
        );
        assert_eq!(r, vec![vec![s("early"), Val::Int(15)], vec![s("late"), Val::Int(67)]]);
    }

    #[test]
    fn a_subquery_in_from_is_a_table() {
        // Day 1: 2 stops; day 2: 3 stops.
        let r = rows("SELECT avg(u) AS a FROM (SELECT d, uniq(stop) AS u FROM t GROUP BY d)", &[]);
        assert_eq!(r, vec![vec![Val::Float(2.5)]]);
    }

    #[test]
    fn distinct_keeps_the_first_of_each_row() {
        let r = rows("SELECT DISTINCT d FROM t", &[]);
        assert_eq!(r, vec![vec![s("2026-04-01")], vec![s("2026-04-02")]]);
    }

    #[test]
    fn an_unknown_column_is_named() {
        assert_eq!(execute(&db(), "SELECT nope FROM t", &[]), Err(EvalError::UnknownColumn("nope".into())));
        assert_eq!(execute(&db(), "SELECT a FROM missing", &[]), Err(EvalError::UnknownTable("missing".into())));
        assert_eq!(execute(&db(), "SELECT frob(n) FROM t", &[]), Err(EvalError::UnsupportedFunction("frob".into())));
        assert!(matches!(execute(&db(), "SELECT n FROM t WHERE sum(n) > 1", &[]), Err(EvalError::Unsupported(_))));
    }

    #[test]
    fn nulls_sort_last_and_integer_division_is_float() {
        let mut t = Table::new("x", &[("v", "Nullable(Int64)")]);
        for v in [Val::Int(3), Val::Null, Val::Int(1)] {
            t.push(vec![v]);
        }
        let mut db = Database::new();
        db.insert(t);
        for dir in ["ASC", "DESC"] {
            let r = execute(&db, &format!("SELECT v FROM x ORDER BY v {dir}"), &[]).unwrap().rows;
            assert_eq!(r[2], vec![Val::Null], "{dir}");
        }
        let r = execute(&db, "SELECT v / 2, v % 2, v * 2, intDiv(v, 2) FROM x WHERE v IS NOT NULL ORDER BY v", &[]).unwrap().rows;
        assert_eq!(r[1], vec![Val::Float(1.5), Val::Int(1), Val::Int(6), Val::Int(1)]);
        let r = execute(&db, "SELECT count(v), count(), v / 0 FROM x", &[]).unwrap().rows;
        assert_eq!(r, vec![vec![Val::Int(2), Val::Int(3), Val::Null]]);
    }
}
