//! Pure string checks for the SQL the ClickHouse population adapter and its
//! loader send. Two rules from the design (§7.2, and the global ClickHouse
//! constraints): no statement ever selects `*` or `alias.*` (a view over
//! `MATERIALIZED` columns silently loses them), and no statement names a
//! database other than [`DOTS_DB`] (the loader owns that one database and
//! nothing else on the server). Tests in the adapter and in
//! `tools/dotgrid-build` run every SQL template through these.

/// The one database the dot-density loader creates and writes.
pub const DOTS_DB: &str = "gpui_map_dots";

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A bare or quoted (`` `x` ``, `"x"`) identifier, keyword or number.
    Word(String),
    Sym(char),
}

impl Tok {
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self, Tok::Word(w) if w.eq_ignore_ascii_case(kw))
    }
}

/// Splits `sql` into words and symbols, dropping whitespace, string
/// literals (`'…'` with `\'` and `''` escapes) and comments (`--`, `#`,
/// `/* */`), so neither check can be fooled by (or trip over) text that is
/// not SQL.
fn tokens(sql: &str) -> Vec<Tok> {
    let b: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && b.get(i + 1) == Some(&'-') || c == '#' {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i < b.len() && !(b[i] == '*' && b.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if c == '\'' {
            i += 1;
            while i < b.len() {
                match b[i] {
                    '\\' => i += 2,
                    '\'' if b.get(i + 1) == Some(&'\'') => i += 2,
                    '\'' => break,
                    _ => i += 1,
                }
            }
            i += 1;
        } else if c == '`' || c == '"' {
            let start = i + 1;
            i = start;
            while i < b.len() && b[i] != c {
                i += 1;
            }
            out.push(Tok::Word(b[start..i.min(b.len())].iter().collect()));
            i += 1;
        } else if c.is_alphanumeric() || c == '_' {
            let start = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                i += 1;
            }
            out.push(Tok::Word(b[start..i].iter().collect()));
        } else {
            out.push(Tok::Sym(c));
            i += 1;
        }
    }
    out
}

/// True when `sql` selects `*` or `alias.*` anywhere (case-insensitive, ignores string literals and comments).
pub fn star_select_in(sql: &str) -> bool {
    let t = tokens(sql);
    t.iter().enumerate().any(|(i, tok)| {
        *tok == Tok::Sym('*')
            && i > 0
            && match &t[i - 1] {
                // `alias.*`, or `*` opening/continuing a select list. An
                // operand before it (`a * b`, `2 * x`, `(a) * b`) or a
                // bracket (`count(*)`) is not a star select.
                Tok::Sym('.') | Tok::Sym(',') => true,
                prev => prev.is_kw("SELECT") || prev.is_kw("DISTINCT") || prev.is_kw("ALL"),
            }
    })
}

/// True when `sql` names any database other than `allowed` in a `db.table` reference or USE/CREATE DATABASE.
///
/// **Keyword-scoped, not a parser.** It sees the name right after `USE`,
/// `DATABASE`, `FROM`, `JOIN`, `INTO`, `TABLE`, `VIEW`, `DICTIONARY`,
/// `TRUNCATE` and `TO`, and both operands of `EXCHANGE TABLES a AND b`. It does NOT see a database named inside function or
/// engine arguments -- `ENGINE = Merge(pop, …)`, `Buffer(pop, …)`,
/// `Distributed(…, pop, …)`, table functions such as `remote(…)` or
/// `merge(…)`, dictionary `SOURCE(…)` -- nor what `COLUMNS('.*')` expands to.
/// It is a tripwire for the loader's own templates, which use none of those
/// (the tests in `tools/dotgrid-build/src/sql.rs` pin that), not a sandbox.
///
/// Conservative on purpose: `system.*` reads are allowed, but an UNQUALIFIED
/// table after `FROM`/`JOIN`/`INTO`/`TABLE`/… counts as foreign too, because
/// the server resolves it in the session's default database, which is not
/// `allowed` (a `WITH name AS (…)` CTE name is not a table and is exempt).
/// `INSERT INTO FUNCTION` counts as foreign (it writes outside any table).
pub fn names_foreign_database(sql: &str, allowed: &str) -> bool {
    let t = tokens(sql);
    let word = |i: usize| match t.get(i) {
        Some(Tok::Word(w)) => Some(w.as_str()),
        _ => None,
    };
    let ctes: Vec<&str> = (0..t.len())
        .filter(|&i| t[i + 1..].first().is_some_and(|n| n.is_kw("AS")) && t.get(i + 2) == Some(&Tok::Sym('(')))
        .filter_map(word)
        .collect();
    // Skips `IF [NOT] EXISTS` after a DDL keyword.
    let skip_if = |mut i: usize| {
        if t.get(i).is_some_and(|x| x.is_kw("IF")) {
            i += 1;
            if t.get(i).is_some_and(|x| x.is_kw("NOT")) {
                i += 1;
            }
            if t.get(i).is_some_and(|x| x.is_kw("EXISTS")) {
                i += 1;
            }
        }
        i
    };
    for (i, tok) in t.iter().enumerate() {
        if tok.is_kw("USE") || tok.is_kw("DATABASE") {
            let j = if tok.is_kw("USE") { i + 1 } else { skip_if(i + 1) };
            if word(j).is_some_and(|db| db != allowed) {
                return true;
            }
            continue;
        }
        // `EXCHANGE TABLES a AND b` (and `DICTIONARIES`): both operands must
        // be `allowed.<name>`; an unqualified or missing one counts as foreign.
        if tok.is_kw("EXCHANGE") && t.get(i + 1).is_some_and(|x| x.is_kw("TABLES") || x.is_kw("DICTIONARIES")) {
            let and = (i + 2..t.len()).find(|&k| t[k].is_kw("AND"));
            for j in [Some(i + 2), and.map(|a| a + 1)] {
                let ours = j.is_some_and(|j| {
                    word(j) == Some(allowed) && t.get(j + 1) == Some(&Tok::Sym('.')) && word(j + 2).is_some()
                });
                if !ours {
                    return true;
                }
            }
            continue;
        }
        let names_a_table = ["FROM", "JOIN", "INTO", "TABLE", "VIEW", "DICTIONARY", "TRUNCATE", "TO"]
            .iter()
            .any(|kw| tok.is_kw(kw));
        if !names_a_table {
            continue;
        }
        let j = skip_if(i + 1);
        if t.get(j).is_some_and(|x| x.is_kw("FUNCTION")) {
            return true;
        }
        let Some(first) = word(j) else { continue }; // `FROM (subquery)`
        if t.get(j + 1) == Some(&Tok::Sym('(')) {
            continue; // a table function such as `numbers(10)`
        }
        if t.get(j + 1) == Some(&Tok::Sym('.')) && word(j + 2).is_some() {
            if first != allowed && first != "system" {
                return true;
            }
        } else if !ctes.contains(&first) && !(tok.is_kw("TRUNCATE") && first.eq_ignore_ascii_case("TABLE")) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_star_select_is_caught() {
        assert!(star_select_in("SELECT * FROM t"));
        assert!(star_select_in("select\n  *\nfrom t"));
        assert!(star_select_in("SELECT a.* FROM t AS a"));
        assert!(!star_select_in("SELECT count() FROM t"));
        assert!(!star_select_in("SELECT row, col FROM t -- not SELECT *"));
        assert!(!star_select_in("SELECT '*' AS s FROM t"));
        assert!(!star_select_in("SELECT a * b FROM t"));
    }

    #[test]
    fn a_foreign_database_is_caught() {
        assert!(names_foreign_database("INSERT INTO pop.ghs_cells VALUES", DOTS_DB));
        assert!(names_foreign_database("CREATE DATABASE other", DOTS_DB));
        assert!(!names_foreign_database("INSERT INTO gpui_map_dots.ghs_cells_30ss VALUES", DOTS_DB));
        assert!(!names_foreign_database("SELECT name FROM system.databases WHERE name = 'gpui_map_dots'", DOTS_DB)); // system reads allowed
    }

    #[test]
    fn literals_and_comments_neither_hide_nor_fake_a_star() {
        assert!(star_select_in("SELECT /* note */ * FROM t"));
        assert!(star_select_in("SELECT DISTINCT * FROM t"));
        assert!(star_select_in("SELECT row, * FROM t"));
        assert!(star_select_in("SELECT 'it''s', t.* FROM t"));
        assert!(!star_select_in("SELECT count(*) FROM t"));
        assert!(!star_select_in("SELECT 'a, *' FROM t # SELECT *"));
        assert!(!star_select_in("SELECT (row + 0.5) * {arc:Float64} AS lat FROM t"));
    }

    #[test]
    fn keyword_scoped_references_to_another_database_are_caught() {
        for sql in [
            "USE default",
            "DROP DATABASE IF EXISTS pop",
            "CREATE DATABASE IF NOT EXISTS gpui_map_dots_x",
            "CREATE TABLE IF NOT EXISTS pop.t (a UInt8) ENGINE = Memory",
            "DROP TABLE default.t",
            "TRUNCATE TABLE pop.t",
            "ALTER TABLE `pop`.t DELETE WHERE 1",
            "SELECT a FROM gpui_map_dots.t JOIN pop.u USING (a)",
            "INSERT INTO ghs_cells VALUES", // unqualified: the session default database
            "INSERT INTO FUNCTION file('x') VALUES",
            "RENAME TABLE gpui_map_dots.t TO pop.t",
            "EXCHANGE TABLES pop.t AND gpui_map_dots.t",
            "EXCHANGE TABLES gpui_map_dots.t AND pop.t",
            "exchange tables gpui_map_dots.t and `default`.t",
            "EXCHANGE TABLES gpui_map_dots.t AND t", // unqualified: the session default database
            "EXCHANGE TABLES system.t AND gpui_map_dots.t",
            "EXCHANGE DICTIONARIES gpui_map_dots.d AND pop.d",
        ] {
            assert!(names_foreign_database(sql, DOTS_DB), "{sql}");
        }
        for sql in [
            "CREATE DATABASE IF NOT EXISTS gpui_map_dots",
            "DROP TABLE IF EXISTS gpui_map_dots.t_stage",
            "SELECT count() FROM (SELECT row FROM gpui_map_dots.t) WHERE row > 0",
            "SELECT number FROM numbers(10)",
            "WITH c AS (SELECT row FROM gpui_map_dots.t) SELECT row FROM c",
            "SELECT 'INSERT INTO pop.t' AS s FROM system.one -- DROP DATABASE pop",
            "EXCHANGE TABLES gpui_map_dots.race_parts_2020_load AND gpui_map_dots.race_parts_2020",
        ] {
            assert!(!names_foreign_database(sql, DOTS_DB), "{sql}");
        }
    }

    #[test]
    fn names_inside_function_and_engine_arguments_are_not_seen() {
        // The documented blind spot: these reach `pop` and pass. The loader's
        // templates never use engine or table-function arguments.
        for sql in [
            "CREATE TABLE gpui_map_dots.m (a UInt8) ENGINE = Merge(pop, '^t')",
            "SELECT a FROM gpui_map_dots.t WHERE a IN (SELECT a FROM remote('h', pop, t))",
        ] {
            assert!(!names_foreign_database(sql, DOTS_DB), "now caught -- update the doc: {sql}");
        }
    }
}
