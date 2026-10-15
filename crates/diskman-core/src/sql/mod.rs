pub mod dialect;
pub mod lexer;
pub mod scope;
pub mod split;

pub use dialect::Dialect;
pub use lexer::{Token, TokenKind, tokenize, tokenize_in};
pub use split::{GoBatch, Statement, batches_in, go_batches, split, split_in, statement_at, statement_at_in};

/// Quotes an identifier when PostgreSQL would fold or reject it unquoted (the lexer's keywords;
/// SQL Diskman generates quotes through [`Dialect::quote_ident`], which knows every reserved word).
pub fn quote_ident(name: &str) -> String {
    let plain = !name.is_empty()
        && name.bytes().next().is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
        && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        && !lexer::is_keyword(name);
    if plain { name.to_string() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
}

/// The identifier a token denotes: unquoted names fold to lower case, quoted ones are taken
/// verbatim.
pub fn ident_value(src: &str, t: &Token) -> String {
    ident_value_in(src, t, Dialect::Postgres)
}

/// [`ident_value`] in `dialect`: `` `quoted` `` in MySQL, and unquoted names kept as written
/// where the engine does not fold them.
pub fn ident_value_in(src: &str, t: &Token, dialect: Dialect) -> String {
    let s = t.text(src);
    if t.kind == TokenKind::QuotedIdent {
        let (open, close) = match s.chars().next() {
            Some('`') => ('`', '`'),
            Some('[') => ('[', ']'),
            _ => ('"', '"'),
        };
        let inner = s.strip_prefix(open).unwrap_or(s);
        let inner = inner.strip_suffix(close).unwrap_or(inner);
        inner.replace(&format!("{close}{close}"), &close.to_string())
    } else if dialect.folds_to_lower() {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// The table a simple query reads first (`FROM [schema.]table`), or `None` (`FROM f(…)`, a
/// subquery, no `FROM`).
pub fn first_table(sql: &str) -> Option<String> {
    // T-SQL's `[names]` (nowhere else does a table name start with `[`).
    let d = if sql.contains('[') { Dialect::TSql } else { Dialect::Postgres };
    let toks: Vec<_> = tokenize_in(sql, d).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let i = toks.iter().position(|t| t.kind == TokenKind::Keyword && t.text(sql).eq_ignore_ascii_case("FROM"))?;
    let n = toks.get(i + 1)?;
    if !matches!(n.kind, TokenKind::Ident | TokenKind::QuotedIdent) {
        return None;
    }
    let mut name = ident_value_in(sql, n, d);
    if toks.get(i + 2).is_some_and(|t| t.text(sql) == ".")
        && let Some(n2) = toks.get(i + 3)
    {
        name = ident_value_in(sql, n2, d);
    }
    if toks.get(i + 2).is_some_and(|t| t.text(sql) == "(") {
        return None;
    }
    Some(name)
}

/// `UPDATE`, `CREATE TABLE`: the command of a statement (two words for DDL), upper case.
pub fn statement_verb(sql: &str) -> String {
    // `CREATE TEMP TABLE`, `CREATE OR REPLACE VIEW`: the object, not its modifiers.
    const MODIFIERS: &[&str] = &["TEMP", "TEMPORARY", "UNLOGGED", "UNIQUE", "OR", "REPLACE", "GLOBAL", "LOCAL", "IF", "RECURSIVE", "TRUSTED", "PROCEDURAL"];
    let mut words = sql
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("--"))
        .flat_map(|l| l.split_whitespace())
        .map(|w| w.trim_end_matches(';').to_ascii_uppercase());
    let Some(first) = words.next() else { return "DONE".into() };
    match first.as_str() {
        "CREATE" | "DROP" | "ALTER" | "TRUNCATE" => match words.find(|w| !MODIFIERS.contains(&w.as_str())) {
            Some(obj) => format!("{first} {obj}"),
            None => first,
        },
        _ => first,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_and_verbs() {
        assert_eq!(first_table("select * from public.\"Orders\" o").as_deref(), Some("Orders"));
        assert_eq!(first_table("select 1"), None);
        assert_eq!(first_table("select * from generate_series(1, 3)"), None);
        assert_eq!(statement_verb("-- c\ncreate table t (x int);"), "CREATE TABLE");
        assert_eq!(statement_verb("update t set x = 1"), "UPDATE");
        assert_eq!(statement_verb("CREATE TEMP TABLE IF NOT EXISTS t AS SELECT 1"), "CREATE TABLE");
        assert_eq!(statement_verb("create or replace view v as select 1"), "CREATE VIEW");
        assert_eq!(statement_verb("CREATE UNIQUE INDEX i ON t (x)"), "CREATE INDEX");
        assert_eq!(statement_verb("   "), "DONE");
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("orders"), "orders");
        assert_eq!(quote_ident("Orders"), "\"Orders\"");
        assert_eq!(quote_ident("select"), "\"select\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident("1x"), "\"1x\"");
    }
}
