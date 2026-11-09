//! The read guard (C1, explorer-sql D1 and D5), generalised from
//! `src/overlay/dotgrid/sql_guard.rs` (which stays where it is).
//!
//! Every statement the crate hands to a transport passes [`check`] first,
//! through [`super::CompiledQuery::guarded`]. It accepts exactly one
//! `SELECT` or `WITH` statement (or `DESCRIBE`/`EXPLAIN` of one, which the
//! connector itself builds), and refuses:
//!
//! - a `;` outside strings and comments (one statement only);
//! - a write or DDL keyword in statement position (start, or right after a
//!   bracket, which is how a data-modifying CTE begins);
//! - a top-level `INTO` (`SELECT … INTO t` in Postgres, `INTO OUTFILE` in
//!   ClickHouse);
//! - `FOR UPDATE` / `FOR SHARE` (Postgres row locks);
//! - a ClickHouse table function that reaches another host or the server's
//!   files, unless the connection allows it by name (D5);
//! - `EXPLAIN ANALYZE`, which executes its statement;
//! - a template placeholder that was never compiled.
//!
//! It is a tripwire, not a parser. The server-side layers (ClickHouse
//! `readonly=2` or a read-only profile; a Postgres `READ ONLY` transaction)
//! stand behind it.

use super::lexer::{lex, Kind, Lexeme};
use super::Dialect;

/// Why the guard refused a statement. Every variant is a closed code; none
/// carries text from the statement, so a refusal can be shown anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardCode {
    /// The text does not lex (a string or comment never closes).
    Unterminated,
    /// A `;` outside strings and comments.
    MultipleStatements,
    /// The statement is not a `SELECT`/`WITH` read.
    NotRead,
    /// A write or DDL keyword in statement position.
    WriteKeyword,
    /// A data-modifying CTE (`WITH x AS (DELETE … RETURNING …)`).
    DataModifyingCte,
    /// A top-level `INTO`.
    SelectInto,
    /// `FOR UPDATE` / `FOR SHARE`.
    RowLock,
    /// `EXPLAIN ANALYZE` runs its statement.
    ExplainAnalyze,
    /// A denied ClickHouse table function, named from the fixed list.
    TableFunction(&'static str),
    /// A `${…}` that never compiled.
    UncompiledPlaceholder,
    /// The query was compiled for another dialect than the connector speaks.
    DialectMismatch,
}

impl GuardCode {
    /// The diagnostic code.
    pub fn code(self) -> &'static str {
        match self {
            GuardCode::TableFunction(_) => "table_function_not_allowed",
            GuardCode::DataModifyingCte => "data_modifying_cte",
            GuardCode::SelectInto => "select_into",
            GuardCode::RowLock => "row_lock",
            GuardCode::DialectMismatch => "dialect_mismatch",
            _ => "statement_not_read",
        }
    }
}

/// ClickHouse table functions that read from or reach other hosts, or the
/// server's own files and processes, using the server's network position
/// (D5). Refused unless the connection allows the name.
pub const CLICKHOUSE_DENIED_TABLE_FUNCTIONS: [&str; 15] = [
    "url",
    "s3",
    "s3Cluster",
    "file",
    "remote",
    "remoteSecure",
    "cluster",
    "mysql",
    "postgresql",
    "jdbc",
    "odbc",
    "hdfs",
    "azureBlobStorage",
    "executable",
    "input",
];

const WRITE_KEYWORDS: [&str; 20] = [
    "INSERT", "UPDATE", "DELETE", "MERGE", "UPSERT", "ALTER", "DROP", "CREATE", "TRUNCATE", "GRANT", "REVOKE", "ATTACH",
    "DETACH", "RENAME", "OPTIMIZE", "KILL", "COPY", "CALL", "EXCHANGE", "UNDROP",
];

/// Per-connection guard settings: the table functions this connection
/// allows despite the denylist (a non-secret setting, D5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuardOptions {
    pub allowed_table_functions: Vec<String>,
}

/// Checks one statement. `Ok` means it may be sent.
pub fn check(sql: &str, dialect: Dialect, opts: &GuardOptions) -> Result<(), GuardCode> {
    let lexemes = lex(sql, dialect).map_err(|_| GuardCode::Unterminated)?;
    let t: Vec<Lexeme> = lexemes.into_iter().filter(Lexeme::significant).collect();
    let word = |i: usize| t.get(i).filter(|l| l.kind == Kind::Word).map(|l| l.text(sql));
    let is_kw = |i: usize, kw: &str| word(i).is_some_and(|w| w.eq_ignore_ascii_case(kw));

    if t.iter().any(|l| l.kind == Kind::Sym(b';')) {
        return Err(GuardCode::MultipleStatements);
    }
    if t.iter().any(|l| l.kind == Kind::Template) {
        return Err(GuardCode::UncompiledPlaceholder);
    }
    // The opening: SELECT/WITH, or DESCRIBE/EXPLAIN of one.
    let mut first = 0;
    if is_kw(0, "DESCRIBE") || is_kw(0, "DESC") || is_kw(0, "EXPLAIN") {
        if is_kw(0, "EXPLAIN") && (is_kw(1, "ANALYZE") || is_kw(1, "ANALYSE") || t.get(1).is_some_and(|l| l.kind == Kind::Sym(b'('))) {
            // `EXPLAIN (ANALYZE) …` and `EXPLAIN ANALYZE …` execute.
            return Err(GuardCode::ExplainAnalyze);
        }
        first = 1;
        // `EXPLAIN ESTIMATE` (ClickHouse) reads the parts it would touch
        // and runs nothing: the live tiles' cost preview.
        if is_kw(1, "TABLE") || (dialect == Dialect::ClickHouse && is_kw(0, "EXPLAIN") && is_kw(1, "ESTIMATE")) {
            first = 2;
        }
        while t.get(first).is_some_and(|l| l.kind == Kind::Sym(b'(')) {
            first += 1;
        }
    }
    if !(is_kw(first, "SELECT") || is_kw(first, "WITH")) {
        return Err(if word(first).is_some_and(|w| WRITE_KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(w))) {
            GuardCode::WriteKeyword
        } else {
            GuardCode::NotRead
        });
    }

    let mut depth: i32 = 0;
    for (i, l) in t.iter().enumerate() {
        match l.kind {
            Kind::Sym(b'(') => depth += 1,
            Kind::Sym(b')') => depth -= 1,
            Kind::Word => {
                let w = l.text(sql);
                let prev = i.checked_sub(1).and_then(|p| t.get(p)).map(|p| p.kind);
                let next = t.get(i + 1).map(|n| n.kind);
                let statement_position = matches!(prev, None | Some(Kind::Sym(b'(')) | Some(Kind::Sym(b')')));
                if statement_position && next != Some(Kind::Sym(b'(')) && WRITE_KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(w)) {
                    return Err(if prev == Some(Kind::Sym(b'(')) { GuardCode::DataModifyingCte } else { GuardCode::WriteKeyword });
                }
                if depth == 0 && w.eq_ignore_ascii_case("INTO") {
                    return Err(GuardCode::SelectInto);
                }
                if dialect == Dialect::Postgres
                    && w.eq_ignore_ascii_case("FOR")
                    && ["UPDATE", "SHARE", "NO", "KEY"].iter().any(|k| is_kw(i + 1, k))
                {
                    return Err(GuardCode::RowLock);
                }
                if dialect == Dialect::ClickHouse && next == Some(Kind::Sym(b'(')) && prev != Some(Kind::Sym(b'.')) {
                    if let Some(f) = CLICKHOUSE_DENIED_TABLE_FUNCTIONS.iter().find(|f| f.eq_ignore_ascii_case(w)) {
                        if !opts.allowed_table_functions.iter().any(|a| a.eq_ignore_ascii_case(f)) {
                            return Err(GuardCode::TableFunction(f));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(s: &str) -> Result<(), GuardCode> {
        check(s, Dialect::ClickHouse, &GuardOptions::default())
    }
    fn pg(s: &str) -> Result<(), GuardCode> {
        check(s, Dialect::Postgres, &GuardOptions::default())
    }

    #[test]
    fn reads_pass() {
        for d in [Dialect::ClickHouse, Dialect::Postgres] {
            let ok = GuardOptions::default();
            assert_eq!(check("SELECT a FROM t WHERE b = 'x;y'", d, &ok), Ok(()));
            assert_eq!(check("WITH x AS (SELECT 1 AS a) SELECT a FROM x", d, &ok), Ok(()));
            assert_eq!(check("-- c\nSELECT replace(a, 'b', 'c') FROM t", d, &ok), Ok(()));
            assert_eq!(check("DESCRIBE (SELECT a FROM t)", d, &ok), Ok(()));
        }
    }

    #[test]
    fn writes_and_second_statements_are_refused() {
        assert_eq!(ch("INSERT INTO t VALUES (1)"), Err(GuardCode::WriteKeyword));
        assert_eq!(ch("DROP TABLE t"), Err(GuardCode::WriteKeyword));
        assert_eq!(ch("SELECT 1; DROP TABLE t"), Err(GuardCode::MultipleStatements));
        assert_eq!(ch("/*x*/DROP TABLE t"), Err(GuardCode::WriteKeyword));
        assert_eq!(ch("SHOW TABLES"), Err(GuardCode::NotRead));
        assert_eq!(ch("SELECT 1 INTO OUTFILE 'x'"), Err(GuardCode::SelectInto));
        assert_eq!(ch("SELECT '${x}'"), Ok(()), "a placeholder inside a string is text");
        assert_eq!(ch("SELECT ${x}"), Err(GuardCode::UncompiledPlaceholder));
        assert_eq!(pg("EXPLAIN ANALYZE SELECT 1"), Err(GuardCode::ExplainAnalyze));
    }

    #[test]
    fn explain_estimate_passes_for_clickhouse_only_and_still_needs_a_read() {
        assert_eq!(ch("EXPLAIN ESTIMATE SELECT a FROM t WHERE b > 1"), Ok(()));
        assert!(ch("EXPLAIN ESTIMATE DROP TABLE t").is_err());
        assert!(ch("EXPLAIN ESTIMATE SHOW TABLES").is_err());
        assert!(pg("EXPLAIN ESTIMATE SELECT 1").is_err());
    }

    #[test]
    fn dollar_quoted_semicolon_is_not_a_second_statement() {
        assert_eq!(pg("SELECT $x$;DROP TABLE t$x$ AS a"), Ok(()));
        assert_eq!(pg("SELECT $$;$$ AS a"), Ok(()));
        assert_eq!(pg("SELECT E'\\';' AS a"), Ok(()));
        assert_eq!(pg("SELECT '\\'; DROP TABLE t; --'"), Err(GuardCode::MultipleStatements));
        assert_eq!(pg("SELECT 1 /* /* nested */ ; */"), Ok(()));
    }

    #[test]
    fn select_into_is_refused() {
        assert_eq!(pg("SELECT a INTO new_t FROM t"), Err(GuardCode::SelectInto));
        assert_eq!(pg("select a into temporary x from t"), Err(GuardCode::SelectInto));
    }

    #[test]
    fn a_deleting_cte_is_refused() {
        assert_eq!(pg("WITH gone AS (DELETE FROM t RETURNING id) SELECT id FROM gone"), Err(GuardCode::DataModifyingCte));
        assert_eq!(pg("WITH a AS (SELECT 1), b AS (UPDATE t SET x = 1 RETURNING x) SELECT x FROM b"), Err(GuardCode::DataModifyingCte));
        assert_eq!(pg("WITH a AS (SELECT 1 AS x) INSERT INTO t SELECT x FROM a"), Err(GuardCode::WriteKeyword));
    }

    #[test]
    fn for_update_is_refused() {
        assert_eq!(pg("SELECT a FROM t FOR UPDATE"), Err(GuardCode::RowLock));
        assert_eq!(pg("SELECT a FROM t for share"), Err(GuardCode::RowLock));
        assert_eq!(pg("SELECT a FROM t FOR NO KEY UPDATE"), Err(GuardCode::RowLock));
        assert_eq!(pg("SELECT a FROM t FOR KEY SHARE"), Err(GuardCode::RowLock));
    }

    #[test]
    fn every_denied_table_function_is_refused() {
        for f in CLICKHOUSE_DENIED_TABLE_FUNCTIONS {
            let lower = f.to_ascii_lowercase();
            let upper = f.to_ascii_uppercase();
            for name in [f.to_string(), lower, upper] {
                assert_eq!(ch(&format!("SELECT a FROM {name}('x')")), Err(GuardCode::TableFunction(f)), "{name}");
                assert_eq!(ch(&format!("SELECT a FROM {name} \n ('x')")), Err(GuardCode::TableFunction(f)), "{name} with whitespace");
                assert_eq!(ch(&format!("SELECT a FROM t WHERE b IN (SELECT c FROM {name}/**/('x'))")), Err(GuardCode::TableFunction(f)));
            }
        }
    }

    #[test]
    fn an_allowed_function_passes() {
        let opts = GuardOptions { allowed_table_functions: vec!["URL".into()] };
        assert_eq!(check("SELECT a FROM url('http://127.0.0.1/x.csv')", Dialect::ClickHouse, &opts), Ok(()));
        assert_eq!(check("SELECT a FROM s3('x')", Dialect::ClickHouse, &opts), Err(GuardCode::TableFunction("s3")));
    }

    #[test]
    fn a_table_function_in_a_string_or_a_column_is_ignored() {
        assert_eq!(ch("SELECT 'url(x)' AS a, `file` FROM t"), Ok(()));
        assert_eq!(ch("SELECT a FROM t -- remote('x')"), Ok(()));
        assert_eq!(ch("SELECT t.file, url FROM t"), Ok(()));
        // Postgres has no table-function denylist; `file(` is just a function there.
        assert_eq!(pg("SELECT file('x')"), Ok(()));
    }

    /// Cases ported from clickpad's guard (`ch-agent/src/sql.rs`
    /// `is_read_only_query` tests) and scanner (`ch-core/src/sql_scan.rs`).
    /// Two deliberate differences, both from the dash spec (§4.2.1, C1):
    /// any `;` refuses (clickpad accepts one trailing `;`), and only
    /// `SELECT`/`WITH` or `DESCRIBE`/`EXPLAIN` *of a query* pass (clickpad
    /// also accepts `SHOW` and `DESCRIBE TABLE`).
    #[test]
    fn clickpad_cases() {
        for sql in [
            "SELECT country, count() AS n FROM visits GROUP BY country",
            "WITH t AS (SELECT 1 AS a) SELECT a FROM t",
            "EXPLAIN SELECT 1",
            "SELECT 1 -- what; ever\nFROM system.one",
            "SELECT 'a;b' AS s",
            "SELECT 'it\\'s; fine' AS s",
            "SELECT 'it''s; fine' AS s",
            "SELECT `a;b` FROM t",
            "SELECT \"a;b\" FROM t",
            "SELECT 1 -- ; DROP TABLE events",
            "SELECT 1 /* ; DROP TABLE events */",
            "SELECT 1 /* ;\n multi line ; */ + 2 -- ;\n",
            "/* the point */ SELECT 1",
            "-- the point\nSELECT 1",
            "\n\t  SELECT 1",
            "SELECT count() FROM selection",
            "select country from visits",
        ] {
            assert_eq!(ch(sql), Ok(()), "{sql:?} must pass");
        }
        for sql in [
            "DROP TABLE events",
            "drop table events",
            "INSERT INTO events VALUES (1)",
            "ALTER TABLE events DELETE WHERE 1",
            "TRUNCATE TABLE events",
            "CREATE TABLE t (a UInt8) ENGINE = Memory",
            "OPTIMIZE TABLE events FINAL",
            "SYSTEM SHUTDOWN",
            "KILL QUERY WHERE 1",
            "ATTACH TABLE events",
            "RENAME TABLE a TO b",
            "GRANT SELECT ON *.* TO someone",
            "SET max_threads = 1",
            "USE analytics",
            "UNDROP TABLE events",
            "INSERT INTO selection VALUES (1)",
            "SELECTED a FROM t",
            "WITHDRAW 5",
            "DESCRIBED",
            "SELECT$1 FROM t",
            "SELECT 1; DROP TABLE events",
            "SELECT 1;DROP TABLE events",
            "SELECT 1;\nDROP TABLE events",
            "SELECT 1; SELECT 2",
            "SELECT 1;;",
            "SELECT 1;",
            "SELECT 1; -- all done",
            "SELECT 'a;b'; DROP TABLE events",
            "SELECT 1 -- fine\n; DROP TABLE events",
            "/*x*/DROP TABLE events",
            "-- innocent\nDROP TABLE events",
            "  /* a */ /* b */ INSERT INTO t VALUES",
            "",
            "   \n\t ",
            "-- just a thought",
            "/* just a thought */",
            ";",
            "SELECT 'unterminated ; DROP TABLE t",
            "SELECT `unterminated",
            "SELECT 1 /* unterminated ; DROP",
            "/* SELECT 1",
            "SHOW TABLES",
            "DESCRIBE TABLE events",
        ] {
            assert!(ch(sql).is_err(), "{sql:?} must be refused");
        }
    }
}
