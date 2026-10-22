pub mod dialect;
pub mod lexer;
pub mod scope;
pub mod split;

pub use dialect::Dialect;
pub use lexer::{Token, TokenKind, tokenize, tokenize_in};
pub use split::is_exec_comment;
pub use split::{GoBatch, Statement, batches_in, go_batches, split, split_in, statement_at, statement_at_in};

pub fn quote_ident(name: &str) -> String {
    let plain = !name.is_empty()
        && name.bytes().next().is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
        && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        && !lexer::is_keyword(name);
    if plain { name.to_string() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
}

pub fn ident_value(src: &str, t: &Token) -> String {
    ident_value_in(src, t, Dialect::Postgres)
}

pub fn ident_value_in(src: &str, t: &Token, dialect: Dialect) -> String {
    let s = t.text(src);
    if t.kind == TokenKind::QuotedIdent {
        if let Some(u) = s.strip_prefix("U&").or_else(|| s.strip_prefix("u&"))
            && u.starts_with('"')
        {
            return unicode_escapes(u[1..].strip_suffix('"').unwrap_or(&u[1..])).replace("\"\"", "\"");
        }
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

fn unicode_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('\\') {
        out.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        let (digits, n) = match rest.strip_prefix('+') {
            Some(r) => (r, 6),
            None => (rest, 4),
        };
        if rest.starts_with('\\') {
            out.push('\\');
            rest = &rest[1..];
        } else if let Some(c) = digits.get(..n).and_then(|h| u32::from_str_radix(h, 16).ok()).and_then(char::from_u32) {
            out.push(c);
            rest = &digits[n..];
        } else {
            out.push('\\');
        }
    }
    out.push_str(rest);
    out
}

/// The dialect is a guess: a bracketed name after FROM (`[dbo].[t]`) is read as T-SQL's, while
/// other brackets (DuckDB's `[1, 2]`, PostgreSQL's `a[1]`) leave PostgreSQL's rules.
pub fn first_table(sql: &str) -> Option<String> {
    first_table_in(sql, Dialect::Postgres).or_else(|| if sql.contains('[') { first_table_in(sql, Dialect::TSql) } else { None })
}

pub fn first_table_in(sql: &str, d: Dialect) -> Option<String> {
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

pub fn statement_verb(sql: &str) -> String {
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
        assert_eq!(first_table("SELECT * FROM [dbo].[Orders]").as_deref(), Some("Orders"));
        assert_eq!(first_table("SELECT [1, 2] AS l FROM t").as_deref(), Some("t"), "a DuckDB list is no T-SQL name");
        assert_eq!(first_table_in("SELECT * FROM `a b`", Dialect::Sqlite).as_deref(), Some("a b"));
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

    #[test]
    fn unicode_ident() {
        let src = "U&\"d\\0061t\\+000061\"";
        let t = tokenize(src)[0];
        assert_eq!(ident_value(src, &t), "data");
    }
}

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

pub fn read_script(path: &std::path::Path) -> std::io::Result<String> {
    std::fs::read(path).map(|b| decode_script(&b))
}

/// Their delimiters are blanked, not removed, so every byte offset stays.
pub fn expand_exec_comments(sql: &str, dialect: Dialect) -> std::borrow::Cow<'_, str> {
    open_exec_comments(sql, dialect, false)
}

/// The texts a MySQL server may run for `sql`: every `/*! … */` opened and, when a comment runs
/// on some servers only (`/*!80000 …`, MariaDB's `/*M! …`), also with those skipped; which one
/// runs is the server's to decide, so the agent layers read both. Offsets stay in each.
pub fn exec_readings(sql: &str, dialect: Dialect) -> Vec<std::borrow::Cow<'_, str>> {
    let open = open_exec_comments(sql, dialect, false);
    let skipped = open_exec_comments(sql, dialect, true);
    if skipped == open { vec![open] } else { vec![open, skipped] }
}

fn open_exec_comments(sql: &str, dialect: Dialect, skip_versioned: bool) -> std::borrow::Cow<'_, str> {
    if !dialect.script().exec_comments || !sql.contains("/*!") && !sql.contains("/*M!") {
        return std::borrow::Cow::Borrowed(sql);
    }
    let mut out = sql.to_string();
    for t in tokenize_in(sql, dialect) {
        let text = t.text(sql);
        if t.kind != TokenKind::BlockComment || !is_exec_comment(text) {
            continue;
        }
        let head = if text.starts_with("/*!") { 3 } else { 4 };
        let digits = text.as_bytes()[head..].iter().take_while(|b| b.is_ascii_digit()).count();
        let blank = |out: &mut String, from: usize, to: usize| out.replace_range(from..to, &" ".repeat(to - from));
        if skip_versioned && (head == 4 || digits > 0) {
            blank(&mut out, t.start, t.end);
            continue;
        }
        blank(&mut out, t.start, t.start + head + digits);
        // One left open runs to the end of the text, as far as Dataman can tell.
        if text.len() >= head + digits + 2 && text.ends_with("*/") {
            blank(&mut out, t.end - 2, t.end);
        }
    }
    std::borrow::Cow::Owned(out)
}
