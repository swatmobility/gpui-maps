//! The tables a statement reads, by name: what a dashboard says it needs
//! when the server answers "unknown table" (the Studio's missing-table
//! card). Read off the lexer, never off server text: every `FROM` and
//! `JOIN` followed by a name, `db.table` kept whole, subqueries, table
//! functions (`numbers(10)`) and the statement's own `WITH name AS (...)`
//! names left out. A best effort for a message, not a parser.

use super::lexer::{lex, Kind, Lexeme};
use super::Dialect;

/// The distinct tables `sql` reads, in order of first appearance.
pub fn referenced(sql: &str, dialect: Dialect) -> Vec<String> {
    let Ok(all) = lex(sql, dialect) else { return Vec::new() };
    let toks: Vec<Lexeme> = all.into_iter().filter(|l| !matches!(l.kind, Kind::Space | Kind::Comment)).collect();
    let word = |i: usize| toks.get(i).filter(|l| l.kind == Kind::Word).map(|l| l.text(sql));
    let is = |i: usize, kw: &str| word(i).is_some_and(|w| w.eq_ignore_ascii_case(kw));
    let sym = |i: usize, c: u8| toks.get(i).is_some_and(|l| l.kind == Kind::Sym(c));
    let name = |i: usize| -> Option<String> {
        let l = toks.get(i)?;
        let t = l.text(sql);
        match l.kind {
            Kind::Word => Some(t.to_string()),
            Kind::QuotedIdent => Some(t[1..t.len() - 1].to_string()),
            _ => None,
        }
    };
    // `WITH name AS (` and `, name AS (`: the statement's own names.
    let mut ctes = Vec::new();
    for i in 0..toks.len() {
        if is(i + 1, "AS") && sym(i + 2, b'(') {
            if let Some(n) = name(i) {
                ctes.push(n.to_ascii_lowercase());
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    for i in 0..toks.len() {
        if !(is(i, "FROM") || is(i, "JOIN")) {
            continue;
        }
        let Some(first) = name(i + 1) else { continue };
        let keyword = toks[i + 1].kind == Kind::Word && is_keyword(&first);
        if keyword {
            continue;
        }
        let (full, after) = if sym(i + 2, b'.') {
            match name(i + 3) {
                Some(second) => (format!("{first}.{second}"), i + 4),
                None => continue,
            }
        } else {
            (first, i + 2)
        };
        // A table function, not a table.
        if sym(after, b'(') {
            continue;
        }
        if ctes.contains(&full.to_ascii_lowercase()) || full.starts_with(char::is_numeric) {
            continue;
        }
        if !out.contains(&full) {
            out.push(full);
        }
    }
    out
}

/// `sql` with every UNqualified table it reads (the names [`referenced`]
/// finds) that `databases` maps qualified as `` `db`.`table` ``: a page
/// written against bare names, run on a server whose tables live in
/// per-app databases. Already-qualified names,
/// CTEs, subqueries, table functions and every other token are left as
/// they are. The rewrite is token-exact, so the result still passes the
/// read guard the original did.
pub fn qualify(sql: &str, dialect: Dialect, databases: &std::collections::BTreeMap<String, String>) -> String {
    let Ok(all) = lex(sql, dialect) else { return sql.to_string() };
    let toks: Vec<Lexeme> = all.into_iter().filter(|l| !matches!(l.kind, Kind::Space | Kind::Comment)).collect();
    let is = |i: usize, kw: &str| toks.get(i).filter(|l| l.kind == Kind::Word).is_some_and(|l| l.text(sql).eq_ignore_ascii_case(kw));
    let sym = |i: usize, c: u8| toks.get(i).is_some_and(|l| l.kind == Kind::Sym(c));
    let name = |i: usize| -> Option<String> {
        let l = toks.get(i)?;
        let t = l.text(sql);
        match l.kind {
            Kind::Word => Some(t.to_string()),
            Kind::QuotedIdent => Some(t[1..t.len() - 1].to_string()),
            _ => None,
        }
    };
    let mut ctes = Vec::new();
    for i in 0..toks.len() {
        if is(i + 1, "AS") && sym(i + 2, b'(') {
            if let Some(n) = name(i) {
                ctes.push(n.to_ascii_lowercase());
            }
        }
    }
    // (byte range of the name token, replacement)
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for i in 0..toks.len() {
        if !(is(i, "FROM") || is(i, "JOIN")) {
            continue;
        }
        let Some(table) = name(i + 1) else { continue };
        if toks[i + 1].kind == Kind::Word && is_keyword(&table) {
            continue;
        }
        if sym(i + 2, b'.') || sym(i + 2, b'(') || ctes.contains(&table.to_ascii_lowercase()) {
            continue;
        }
        let Some(db) = databases.get(&table) else { continue };
        let q = |s: &str| format!("`{}`", s.replace('\\', "\\\\").replace('`', "\\`"));
        edits.push((toks[i + 1].start, toks[i + 1].end, format!("{}.{}", q(db), q(&table))));
    }
    let mut out = sql.to_string();
    for (start, end, text) in edits.into_iter().rev() {
        out.replace_range(start..end, &text);
    }
    out
}

fn is_keyword(w: &str) -> bool {
    ["SELECT", "LATERAL", "ONLY", "UNNEST"].iter().any(|k| w.eq_ignore_ascii_case(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_qualified_quoted_and_joined_tables_are_found() {
        let sql = "SELECT a FROM bus_passenger_counts WHERE x = 1";
        assert_eq!(referenced(sql, Dialect::ClickHouse), vec!["bus_passenger_counts"]);
        let sql = "SELECT * FROM analytics.trips t JOIN `stops` s ON t.s = s.id LEFT JOIN db.stops USING (id)";
        assert_eq!(referenced(sql, Dialect::ClickHouse), vec!["analytics.trips", "stops", "db.stops"]);
    }

    #[test]
    fn subqueries_table_functions_and_ctes_are_left_out() {
        let sql = "WITH top AS (SELECT * FROM od_matrix) SELECT * FROM (SELECT 1 FROM numbers(10)) JOIN top ON 1 FROM -- from nothing\n peopleflow_single_mesh_japan";
        assert_eq!(referenced(sql, Dialect::ClickHouse), vec!["od_matrix", "peopleflow_single_mesh_japan"]);
        assert!(referenced("SELECT 1", Dialect::ClickHouse).is_empty());
        assert!(referenced("SELECT 'unterminated", Dialect::ClickHouse).is_empty());
    }

    #[test]
    fn qualify_rewrites_only_the_mapped_bare_tables() {
        let map: std::collections::BTreeMap<String, String> =
            [("a".to_string(), "db_a".to_string()), ("top".to_string(), "never".to_string()), ("c".to_string(), "db`c".to_string())].into_iter().collect();
        let sql = "WITH top AS (SELECT * FROM a) SELECT 'FROM a' FROM top JOIN x.a ON 1 JOIN a USING (k) JOIN numbers(3) ON 1 LEFT JOIN c ON 1 JOIN b ON 1";
        assert_eq!(
            qualify(sql, Dialect::ClickHouse, &map),
            "WITH top AS (SELECT * FROM `db_a`.`a`) SELECT 'FROM a' FROM top JOIN x.a ON 1 JOIN `db_a`.`a` USING (k) JOIN numbers(3) ON 1 LEFT JOIN `db\\`c`.`c` ON 1 JOIN b ON 1"
        );
        assert_eq!(qualify("SELECT 'unterminated", Dialect::ClickHouse, &map), "SELECT 'unterminated");
    }

    #[test]
    fn a_table_named_twice_is_listed_once() {
        let sql = "SELECT * FROM a UNION ALL SELECT * FROM a";
        assert_eq!(referenced(sql, Dialect::Postgres), vec!["a"]);
    }
}
