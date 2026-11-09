//! Compiling a template to `(sql, params)` (§4.2.2–§4.2.4, §4.2.7), and the
//! one way to read the result's text (explorer-sql D2).

use std::collections::BTreeMap;

use super::guard::{self, GuardCode, GuardOptions};
use super::template::{Part, Piece, Slot, SqlIssue, Template};
use super::value::{Scalar, VarValue};
use super::Dialect;
use crate::format::nfkc;
use crate::hook::Refusal;
use crate::spec::{is_column_identifier, Clause, FilterBinding, Op, Page, QueryDecl, Required, Variable};

/// The connection's default per-request row cap (C5, D3).
pub const DEFAULT_MAX_ROWS: u64 = 100_000;

/// A bound parameter value.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    One(Scalar),
    Array(Vec<Scalar>),
}

impl ParamValue {
    /// The text ClickHouse reads for `param_<name>` (§8.2). Values are
    /// parsed in ClickHouse's escaped (TSV) form: a backslash, tab and
    /// newline are escaped; an array is `['a','b']` with `\'` and `\\`.
    /// *Uncertain:* the escaping follows ClickHouse's documented
    /// `Escaped` format and is pinned by goldens, not by a live server.
    pub fn clickhouse_text(&self) -> String {
        fn esc_tsv(s: &str) -> String {
            s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n")
        }
        fn elem(v: &Scalar) -> String {
            match v {
                Scalar::Str(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
                other => other.to_text(),
            }
        }
        match self {
            ParamValue::One(Scalar::Str(s)) => esc_tsv(s),
            ParamValue::One(v) => v.to_text(),
            ParamValue::Array(a) => esc_tsv(&format!("[{}]", a.iter().map(elem).collect::<Vec<_>>().join(","))),
        }
    }
}

/// One parameter: its name (`p_day_type`; for Postgres, also its position
/// in [`CompiledQuery::params`]), its type in the query's dialect and its value.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: String,
    pub value: ParamValue,
}

/// A compiled statement. Its SQL text is private: it can be read only
/// through [`CompiledQuery::guarded`], which runs the read guard (D2), so a
/// connector outside this crate cannot send unguarded SQL.
///
/// `compiled_sql_is_not_readable_outside_the_crate`:
///
/// ```compile_fail
/// fn text(q: &swat_maps_dash::sql::CompiledQuery) -> &str {
///     &q.sql
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledQuery {
    sql: String,
    params: Vec<Param>,
    dialect: Dialect,
    max_rows: u64,
    deps: Vec<String>,
}

impl CompiledQuery {
    pub(crate) fn new(sql: String, params: Vec<Param>, dialect: Dialect, max_rows: u64, deps: Vec<String>) -> Self {
        CompiledQuery { sql, params, dialect, max_rows, deps }
    }

    /// The statement, checked by the read guard for `dialect` (the dialect
    /// the connector speaks). The only way to read the SQL text (D2).
    pub fn guarded(&self, dialect: Dialect) -> Result<GuardedSql<'_>, Refusal> {
        self.guarded_with(dialect, &GuardOptions::default())
    }

    /// [`CompiledQuery::guarded`] with a connection's guard settings (the
    /// table functions it allows, D5).
    pub fn guarded_with(&self, dialect: Dialect, opts: &GuardOptions) -> Result<GuardedSql<'_>, Refusal> {
        if dialect != self.dialect {
            return Err(Refusal::Guard { code: GuardCode::DialectMismatch });
        }
        guard::check(&self.sql, dialect, opts).map_err(|code| Refusal::Guard { code })?;
        Ok(GuardedSql { sql: &self.sql, params: &self.params, dialect })
    }

    /// The bound parameters, in order (`$1` is the first for Postgres).
    pub fn params(&self) -> &[Param] {
        &self.params
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// The per-request row cap (D3): the source's `max_rows`, else the
    /// connection's cap. A connector reads at most this many rows and
    /// reports `truncated` when there were more.
    pub fn max_rows(&self) -> u64 {
        self.max_rows
    }

    /// The tables the statement reads (`tables::referenced`), for a
    /// connector that places them in databases.
    pub fn tables(&self) -> Vec<String> {
        super::tables::referenced(&self.sql, self.dialect)
    }

    /// The same statement with its bare tables placed in `databases`
    /// (`tables::qualify`): table names only, params and caps unchanged.
    /// The text still passes the read guard on its way out.
    pub fn qualified(&self, databases: &BTreeMap<String, String>) -> CompiledQuery {
        CompiledQuery { sql: super::tables::qualify(&self.sql, self.dialect, databases), ..self.clone() }
    }

    /// Asks for another cap; the connector refuses one above the
    /// connection's ceiling.
    pub fn with_max_rows(mut self, max_rows: u64) -> Self {
        self.max_rows = max_rows;
        self
    }

    /// The variables this statement read: placeholders and every filter
    /// that reached a slot (§4.2.5).
    pub fn deps(&self) -> &[String] {
        &self.deps
    }

    /// An Explorer dataset's SQL as the user wrote it (explorer-sql P3):
    /// read by the template reader (so the lexer diagnostics and the
    /// guard's rules apply with their offsets), with no placeholder and no
    /// slot. A template (`${…}`) waits for P4 and is refused here with
    /// `template_needs_parameters`. Trailing whitespace and one trailing
    /// `;` are dropped (a habit, not a second statement). Warnings come
    /// back with the query; its text stays readable only through
    /// [`CompiledQuery::guarded`].
    pub fn plain(sql: &str, dialect: Dialect, guard: &GuardOptions, max_rows: u64) -> Result<(CompiledQuery, Vec<SqlIssue>), Vec<SqlIssue>> {
        let text = sql.trim_end();
        let text = text.strip_suffix(';').unwrap_or(text).trim_end();
        if text.is_empty() {
            return Err(vec![SqlIssue { offset: 0, severity: crate::validate::Severity::Error, code: "sql_empty", message: "Write a SELECT to run.".into() }]);
        }
        let no_vars = |_: &str| None;
        let cx = super::TemplateContext { dialect, control_of: &no_vars, privacy: crate::spec::Privacy::Open, guard };
        let template = || {
            vec![SqlIssue {
                offset: text.find("${").unwrap_or(0),
                severity: crate::validate::Severity::Error,
                code: "template_needs_parameters",
                message: "${…} placeholders and slots arrive with Explorer parameters; write the values into the SQL for now.".into(),
            }]
        };
        let (t, warnings) = Template::parse(text, &cx).map_err(|issues| if issues.iter().any(|i| i.code == "unknown_variable") { template() } else { issues })?;
        if !t.placeholder_vars().is_empty() || t.slots().next().is_some() {
            return Err(template());
        }
        guard::check(text, dialect, guard).map_err(|code| {
            vec![SqlIssue { offset: 0, severity: crate::validate::Severity::Error, code: code.code(), message: "Only one SELECT (or WITH … SELECT) can run here.".into() }]
        })?;
        Ok((CompiledQuery::new(text.to_string(), Vec::new(), dialect, max_rows, Vec::new()), warnings))
    }

    /// A statement whose values are all bound ClickHouse query parameters
    /// (`{name:Type}` in the text, `param_<name>` on the request): the
    /// Explorer's live tiles, whose SQL is generated around a user
    /// template. The read guard runs here as well as on the way out, and
    /// every `{name:Type}` in the text must have a value in `params` (a
    /// placeholder never bound would reach the server as an error). The
    /// values are never spliced into the text.
    pub fn bound(sql: &str, params: Vec<Param>, dialect: Dialect, guard: &GuardOptions, max_rows: u64) -> Result<CompiledQuery, GuardCode> {
        guard::check(sql, dialect, guard)?;
        let lexemes = super::lexer::lex(sql, dialect).map_err(|_| GuardCode::Unterminated)?;
        let t: Vec<_> = lexemes.into_iter().filter(super::lexer::Lexeme::significant).collect();
        for (i, l) in t.iter().enumerate() {
            if l.kind != super::lexer::Kind::Sym(b'{') {
                continue;
            }
            let name = t.get(i + 1).filter(|n| n.kind == super::lexer::Kind::Word).map(|n| n.text(sql));
            let typed = t.get(i + 2).is_some_and(|c| c.kind == super::lexer::Kind::Sym(b':'));
            match name {
                Some(n) if typed && params.iter().any(|p| p.name == n) => {}
                _ => return Err(GuardCode::UncompiledPlaceholder),
            }
        }
        Ok(CompiledQuery::new(sql.to_string(), params, dialect, max_rows, Vec::new()))
    }

    /// Wraps this query as `DESCRIBE (query)` for the shape probe (§8.2).
    pub(crate) fn describe(&self) -> CompiledQuery {
        CompiledQuery { sql: format!("DESCRIBE ({})", self.sql), ..self.clone() }
    }

    /// A statement the crate itself builds (the read-only probe). It still
    /// passes the guard on its way out.
    pub(crate) fn internal(sql: &str, dialect: Dialect) -> CompiledQuery {
        CompiledQuery { sql: sql.to_string(), params: Vec::new(), dialect, max_rows: DEFAULT_MAX_ROWS, deps: Vec::new() }
    }

    /// Test-only: any text, as if a bug in generation had produced it.
    #[cfg(test)]
    pub(crate) fn forced_for_test(sql: &str, dialect: Dialect) -> CompiledQuery {
        CompiledQuery::internal(sql, dialect)
    }
}

/// SQL text that passed the read guard. It has no public constructor:
/// [`CompiledQuery::guarded`] is the only way to get one.
///
/// `guarded_sql_cannot_be_constructed`:
///
/// ```compile_fail
/// let g = swat_maps_dash::sql::GuardedSql { sql: "DROP TABLE t", params: &[], dialect: swat_maps_dash::sql::Dialect::ClickHouse };
/// ```
///
/// And the positive control, so the cases above fail for the right reason:
///
/// ```
/// use swat_maps_dash::sql::{CompiledQuery, Dialect, GuardedSql};
/// fn read(q: &CompiledQuery) -> Option<&str> {
///     q.guarded(Dialect::ClickHouse).ok().map(|g: GuardedSql<'_>| g.sql())
/// }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct GuardedSql<'a> {
    sql: &'a str,
    params: &'a [Param],
    dialect: Dialect,
}

impl<'a> GuardedSql<'a> {
    pub fn sql(&self) -> &'a str {
        self.sql
    }
    pub fn params(&self) -> &'a [Param] {
        self.params
    }
    pub fn dialect(&self) -> Dialect {
        self.dialect
    }
}

/// Why a query cannot run yet. Not an error: the widget waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// A required variable is unset (or `"all"`, or an empty set).
    ChooseOne { var: String },
    /// A required variable holds more than one value ("Choose one {label}").
    TooMany { var: String },
    /// A placeholder's variable has no value to bind.
    Unbound { var: String },
}

/// The inputs a compile reads: the page, the query's source, the committed
/// values and the types inferred from options queries.
pub struct CompileInput<'a> {
    pub page: &'a Page,
    pub source: &'a str,
    pub values: &'a BTreeMap<String, VarValue>,
    /// Types inferred from each variable's options query, used as returned
    /// (§3.5), for variables that declare no `type`.
    pub inferred_types: &'a BTreeMap<String, String>,
    pub max_rows: u64,
}

impl<'a> CompileInput<'a> {
    fn value(&self, key: &str) -> &VarValue {
        static UNSET: VarValue = VarValue::Unset;
        self.values.get(key).unwrap_or(&UNSET)
    }

    fn ty(&self, v: &Variable) -> String {
        v.ty.clone().or_else(|| self.inferred_types.get(&v.key).cloned()).unwrap_or_else(|| "String".to_string())
    }

    /// The filters that apply to this query's source.
    fn filters(&self) -> impl Iterator<Item = &'a FilterBinding> + '_ {
        let page_source = self.page.source.as_deref();
        self.page
            .filters
            .iter()
            .filter(move |f| f.column.is_some() && f.source.as_deref().or(page_source) == Some(self.source))
    }
}

struct Binder {
    dialect: Dialect,
    params: Vec<Param>,
}

impl Binder {
    fn bind(&mut self, name: String, ty: String, value: ParamValue) -> String {
        let at = match self.params.iter().position(|p| p.name == name) {
            Some(i) => i,
            None => {
                self.params.push(Param { name: name.clone(), ty: ty.clone(), value });
                self.params.len() - 1
            }
        };
        match self.dialect {
            Dialect::ClickHouse => format!("{{{name}:{ty}}}"),
            Dialect::Postgres => format!("${}", at + 1),
        }
    }
}

impl Template {
    /// Compiles the template with the committed values. Variables bind as
    /// parameters and never appear in the SQL text.
    pub fn compile(&self, input: &CompileInput<'_>) -> Result<CompiledQuery, Pending> {
        let page = input.page;
        let d = self.dialect;
        let mut deps: Vec<String> = Vec::new();
        let add_dep = |k: &str, deps: &mut Vec<String>| {
            if !deps.iter().any(|d| d == k) {
                deps.push(k.to_string());
            }
        };
        for k in self.placeholder_vars() {
            add_dep(k, &mut deps);
        }
        for (slot, exclude) in self.slots() {
            for f in input.filters() {
                if reaches(f, slot) && !exclude.contains(&f.var) {
                    add_dep(&f.var, &mut deps);
                }
            }
        }
        // Required gating (§4.2.4), before anything binds.
        for k in &deps {
            if let Some(v) = page.variable(k) {
                gate(v, input.value(k))?;
            }
        }

        let mut b = Binder { dialect: d, params: Vec::new() };
        let mut sql = String::new();
        for p in &self.pieces {
            match p {
                Piece::Text(t) => sql.push_str(t),
                Piece::Var { key, part } => {
                    let var = page.variable(key).ok_or_else(|| Pending::Unbound { var: key.clone() })?;
                    let ty = input.ty(var);
                    let unbound = || Pending::Unbound { var: key.clone() };
                    let text = match (part, input.value(key)) {
                        (Part::Whole, VarValue::One(s)) => b.bind(format!("p_{key}"), d.param_type(&ty), ParamValue::One(s.clone())),
                        (Part::Whole, VarValue::Many(a)) if !a.is_empty() => {
                            b.bind(format!("p_{key}"), d.array_type(&ty), ParamValue::Array(a.clone()))
                        }
                        (Part::Lo, VarValue::Range { lo: Some(s), .. }) => {
                            b.bind(format!("p_{key}_lo"), d.param_type(&ty), ParamValue::One(s.clone()))
                        }
                        (Part::Hi, VarValue::Range { hi: Some(s), .. }) => {
                            b.bind(format!("p_{key}_hi"), d.param_type(&ty), ParamValue::One(s.clone()))
                        }
                        _ => return Err(unbound()),
                    };
                    sql.push_str(&text);
                }
                Piece::Slot { slot, exclude } => {
                    let mut preds = Vec::new();
                    if *slot != Slot::Viewport {
                        for f in input.filters() {
                            if !reaches(f, *slot) || exclude.contains(&f.var) {
                                continue;
                            }
                            let Some(var) = page.variable(&f.var) else { continue };
                            if let Some(p) = predicate(f, var, input.value(&f.var), &input.ty(var), &mut b) {
                                preds.push(p);
                            }
                        }
                    }
                    if !preds.is_empty() {
                        let head = match slot {
                            Slot::Where => "WHERE ",
                            Slot::Having => "HAVING ",
                            _ => "AND ",
                        };
                        sql.push_str(head);
                        sql.push_str(&preds.join(" AND "));
                    }
                }
            }
        }
        Ok(CompiledQuery::new(sql, b.params, d, input.max_rows, deps))
    }
}

fn reaches(f: &FilterBinding, slot: Slot) -> bool {
    match f.clause {
        Clause::Where => matches!(slot, Slot::Where | Slot::AndWhere),
        Clause::Having => slot.is_having(),
    }
}

fn gate(v: &Variable, value: &VarValue) -> Result<(), Pending> {
    let choose = || Pending::ChooseOne { var: v.key.clone() };
    let too_many = || Pending::TooMany { var: v.key.clone() };
    match (v.required, value) {
        (Required::No, _) => Ok(()),
        (_, VarValue::One(_)) => Ok(()),
        (Required::OneValue, VarValue::Many(a)) if a.len() == 1 => Ok(()),
        (_, VarValue::Many(a)) if a.len() > 1 => Err(too_many()),
        // Strict: a one-element multi still waits (the od_matrix_by_trip contract).
        (Required::Strict, VarValue::Many(a)) if a.len() == 1 => Err(choose()),
        _ => Err(choose()),
    }
}

/// One filter's predicate (§4.2.3), or `None` when its value is unset.
fn predicate(f: &FilterBinding, var: &Variable, value: &VarValue, ty: &str, b: &mut Binder) -> Option<String> {
    if value.is_unset() {
        return None;
    }
    let d = b.dialect;
    let col = f.column.as_deref()?;
    let op = f.op.unwrap_or(var.control.default_op());
    let k = &var.key;
    let one = |v: &VarValue| match v {
        VarValue::One(s) => Some(s.clone()),
        VarValue::Many(a) if a.len() == 1 => a.first().cloned(),
        _ => None,
    };
    Some(match op {
        Op::Eq | Op::Ge | Op::Le => {
            let s = one(value)?;
            let sym = match op {
                Op::Eq => "=",
                Op::Ge => ">=",
                _ => "<=",
            };
            format!("{col} {sym} {}", b.bind(format!("f_{k}"), d.param_type(ty), ParamValue::One(s)))
        }
        Op::In => {
            let items = match value {
                VarValue::Many(a) => a.clone(),
                VarValue::One(s) => vec![s.clone()],
                _ => return None,
            };
            let p = b.bind(format!("f_{k}"), d.array_type(ty), ParamValue::Array(items));
            d.in_array(col, &p)
        }
        Op::Between | Op::Overlaps => {
            let VarValue::Range { lo, hi } = value else { return None };
            // Bind in the order the text reads, so `$n` numbers ascend.
            let bind_lo = |b: &mut Binder| lo.clone().map(|s| b.bind(format!("f_{k}_lo"), d.param_type(ty), ParamValue::One(s)));
            let bind_hi = |b: &mut Binder| hi.clone().map(|s| b.bind(format!("f_{k}_hi"), d.param_type(ty), ParamValue::One(s)));
            let (lo, hi) = if op == Op::Overlaps {
                let hi = bind_hi(b);
                (bind_lo(b), hi)
            } else {
                let lo = bind_lo(b);
                (lo, bind_hi(b))
            };
            if op == Op::Overlaps {
                let mut parts = Vec::new();
                if let Some(hi) = hi {
                    parts.push(format!("{col}_lo <= {hi}"));
                }
                if let Some(lo) = lo {
                    parts.push(format!("{col}_hi >= {lo}"));
                }
                parts.join(" AND ")
            } else {
                match (lo, hi) {
                    (Some(lo), Some(hi)) => format!("{col} BETWEEN {lo} AND {hi}"),
                    (Some(lo), None) => format!("{col} >= {lo}"),
                    (None, Some(hi)) => format!("{col} <= {hi}"),
                    (None, None) => return None,
                }
            }
        }
        Op::Contains => {
            let s = one(value)?;
            let needle = nfkc(&s.to_text());
            if needle.is_empty() {
                return None;
            }
            let p = b.bind(format!("f_{k}"), d.param_type("String"), ParamValue::One(Scalar::Str(needle)));
            let col_expr = if f.normalized { col.to_string() } else { d.nfkc_column(col) };
            d.contains(&col_expr, &p)
        }
    })
}

/// The text of a generated `group`/`agg` query (§4.2.7) as a template, so
/// it compiles through the same path and the same slots.
pub fn generated_text(q: &QueryDecl, dialect: Dialect) -> Result<String, SqlIssue> {
    let err = |code, msg: String| SqlIssue::error_at(0, code, msg);
    let table = q.table.as_deref().ok_or_else(|| err("table_required", "A generated query on a SQL source needs \"table\".".into()))?;
    if !is_column_identifier(table) {
        return Err(err("column_not_identifier", format!("The table \"{table}\" is not a plain name.")));
    }
    let mut group = Vec::new();
    for g in &q.group {
        match g.as_str() {
            Some(c) if is_column_identifier(c) => group.push(c.to_string()),
            _ => return Err(err("column_not_identifier", "A server query groups by plain column names.".into())),
        }
    }
    let mut select = group.clone();
    for (out, call) in &q.agg {
        if !is_column_identifier(out) || out.contains('.') {
            return Err(err("column_not_identifier", format!("The output name \"{out}\" is not a plain name.")));
        }
        let (name, args) = parse_call(call).ok_or_else(|| err("reducer_syntax", format!("\"{call}\" is not a reducer call.")))?;
        let expr = dialect
            .reducer(name, &args)
            .ok_or_else(|| err("reducer_unsupported_by_dialect", format!("{name}() has no SQL form in this dialect.")))?;
        select.push(format!("{expr} AS {out}"));
    }
    let mut sql = format!("SELECT {} FROM {table} ${{where}}", select.join(", "));
    if !group.is_empty() {
        sql.push_str(&format!(" GROUP BY {}", group.join(", ")));
    }
    // Always a having slot: HAVING without GROUP BY filters the one
    // aggregate row, in ClickHouse and Postgres alike.
    sql.push_str(" ${having}");
    if let Some(o) = &q.order {
        let mut w = o.split_whitespace();
        let col = w.next().unwrap_or("");
        let dir = w.next().unwrap_or("asc");
        let dir_ok = dir.eq_ignore_ascii_case("asc") || dir.eq_ignore_ascii_case("desc");
        if !is_column_identifier(col) || !dir_ok || w.next().is_some() {
            return Err(err("order_syntax", format!("\"{o}\" is not \"column\" or \"column asc|desc\".")));
        }
        sql.push_str(&format!(" ORDER BY {col} {}", dir.to_ascii_uppercase()));
    }
    if let Some(n) = q.top {
        sql.push_str(&format!(" LIMIT {n}"));
    }
    Ok(sql)
}

/// `name(a, b)` → `("name", ["a", "b"])`, each argument an identifier or a
/// number.
fn parse_call(call: &str) -> Option<(&str, Vec<&str>)> {
    let (name, rest) = call.trim().split_once('(')?;
    let inner = rest.strip_suffix(')')?;
    let args: Vec<&str> = inner.split(',').map(str::trim).filter(|a| !a.is_empty()).collect();
    let ok = args.iter().all(|a| is_column_identifier(a) || a.parse::<f64>().is_ok());
    (ok && is_column_identifier(name.trim())).then_some((name.trim(), args))
}

impl SqlIssue {
    pub(crate) fn error_at(offset: usize, code: &'static str, message: String) -> SqlIssue {
        SqlIssue { offset, severity: crate::validate::Severity::Error, code, message }
    }
}
