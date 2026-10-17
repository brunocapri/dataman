pub mod dialect;
pub mod lexer;
pub mod scope;
pub mod split;

pub use dialect::Dialect;
pub use lexer::{Token, TokenKind, tokenize, tokenize_in};
pub use split::is_exec_comment;
pub use split::{GoBatch, Statement, batches_in, go_batches, split, split_in, statement_at, statement_at_in};

/// Quotes an identifier when PostgreSQL would fold or reject it unquoted (the lexer's keywords;
/// SQL Dataman generates quotes through [`Dialect::quote_ident`], which knows every reserved word).
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
    fn script_files_decode() {
        assert_eq!(decode_script(b"SELECT 1"), "SELECT 1");
        assert_eq!(decode_script(b"\xef\xbb\xbfUSE [x]\nGO"), "USE [x]\nGO");
        let u16le: Vec<u8> = [0xff, 0xfe].into_iter().chain("SELECT 'é'".encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(decode_script(&u16le), "SELECT 'é'");
        let u16be: Vec<u8> = [0xfe, 0xff].into_iter().chain("SELECT 'é'".encode_utf16().flat_map(u16::to_be_bytes)).collect();
        assert_eq!(decode_script(&u16be), "SELECT 'é'");
        assert_eq!(decode_script(b"SELECT 'caf\xe9'"), "SELECT 'café'");
    }

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

/// A SQL file's text: UTF-8 without or with its signature, UTF-16 (what SSMS and Windows tools
/// save), else the bytes read as Latin-1 (a legacy code page); never the byte order mark.
pub fn decode_script(bytes: &[u8]) -> String {
    let utf16 = |b: &[u8], le: bool| {
        let units: Vec<u16> = b.chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xff, 0xfe, rest @ ..] => utf16(rest, true),
        [0xfe, 0xff, rest @ ..] => utf16(rest, false),
        [0xef, 0xbb, 0xbf, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(_) => bytes.iter().map(|&b| b as char).collect(),
        },
    }
}

/// Reads a SQL file ([`decode_script`]).
pub fn read_script(path: &std::path::Path) -> std::io::Result<String> {
    std::fs::read(path).map(|b| decode_script(&b))
}

/// MySQL's `/*! … */` (and `/*M! … */`) comments hold code the server runs: `sql` with them
/// opened (their delimiters blanked, so every byte offset stays), for reading what a statement
/// does. Other dialects, and texts without such a comment, come back as they are.
pub fn expand_exec_comments(sql: &str, dialect: Dialect) -> std::borrow::Cow<'_, str> {
    if !dialect.script().exec_comments || !sql.contains("/*!") && !sql.contains("/*M!") {
        return std::borrow::Cow::Borrowed(sql);
    }
    let mut out = sql.to_string();
    for t in tokenize_in(sql, dialect) {
        let text = t.text(sql);
        if t.kind != TokenKind::BlockComment || !is_exec_comment(text) || !text.ends_with("*/") || text.len() < 5 {
            continue;
        }
        // `/*!` or `/*M!`, then the version digits.
        let head = if text.starts_with("/*!") { 3 } else { 4 };
        let digits = text.as_bytes()[head..].iter().take_while(|b| b.is_ascii_digit()).count();
        let blank = |out: &mut String, from: usize, to: usize| out.replace_range(from..to, &" ".repeat(to - from));
        blank(&mut out, t.start, t.start + head + digits);
        blank(&mut out, t.end - 2, t.end);
    }
    std::borrow::Cow::Owned(out)
}
