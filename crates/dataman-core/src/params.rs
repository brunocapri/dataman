// Variables are rewritten to `$n` so every value is bound through the extended protocol, never
// spliced into the SQL.

use crate::schema::SchemaSnapshot;
use crate::sql::lexer::{Token, TokenKind, tokenize, tokenize_in};
use crate::sql::{Dialect, scope};

fn ident_value(sql: &str, t: &Token, d: Dialect) -> String {
    crate::sql::ident_value_in(sql, t, d)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub key: String,
    pub index: u32,
    pub hint: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    pub sql: String,
    pub params: Vec<Param>,
}

pub fn detect_in(sql: &str, dialect: Dialect) -> Option<Prepared> {
    if dialect == Dialect::TSql {
        return detect_tsql(sql);
    }
    if matches!(dialect, Dialect::Sqlite | Dialect::DuckDb) {
        return detect_numbered(sql, dialect);
    }
    if dialect != Dialect::MySql {
        return detect(sql);
    }
    let sig: Vec<Token> = tokenize_in(sql, dialect).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let first = sig.first()?.text(sql).to_ascii_uppercase();
    if matches!(first.as_str(), "PREPARE" | "CREATE" | "DECLARE" | "EXECUTE" | "DEALLOCATE") {
        return None;
    }
    let n = sig.iter().filter(|t| t.kind == TokenKind::Param).count() as u32;
    (n > 0).then(|| Prepared { sql: sql.to_string(), params: (1..=n).map(|i| Param { key: format!("?{i}"), index: i, hint: None }).collect() })
}

/// SQLite (`?`, `?N`, `:x`, `@x`, `$x`) and DuckDB (`?`, `$N`), numbered as the engine does: a
/// bare `?` or a new name takes one more than the largest number so far. Every one is rewritten
/// to the dialect's `?N`/`$N`, so values bind by position alone.
fn detect_numbered(sql: &str, d: Dialect) -> Option<Prepared> {
    let sig: Vec<Token> = tokenize_in(sql, d).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let first = sig.first()?.text(sql).to_ascii_uppercase();
    if matches!(first.as_str(), "PREPARE" | "CREATE" | "EXECUTE" | "DEALLOCATE") {
        return None;
    }
    let mut max = 0u32;
    let mut named: Vec<(String, u32)> = Vec::new();
    let mut at: Vec<(std::ops::Range<usize>, u32)> = Vec::new();
    for t in sig.iter().filter(|t| t.kind == TokenKind::Param) {
        let s = t.text(sql);
        let n = if s == "?" {
            max + 1
        } else if let Some(n) = s.strip_prefix(['?', '$']).and_then(|n| n.parse::<u32>().ok()).filter(|n| *n > 0 && s.as_bytes()[1].is_ascii_digit()) {
            n
        } else {
            match named.iter().find(|(k, _)| k == s) {
                Some((_, n)) => *n,
                None => {
                    named.push((s.to_string(), max + 1));
                    max + 1
                }
            }
        };
        max = max.max(n);
        at.push((t.start..t.end, n));
    }
    if at.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(sql.len());
    let mut pos = 0;
    for (r, n) in &at {
        out.push_str(&sql[pos..r.start]);
        out.push_str(&d.placeholder(*n as usize));
        pos = r.end;
    }
    out.push_str(&sql[pos..]);
    let key = |n: u32| named.iter().find(|(_, k)| *k == n).map_or_else(|| d.placeholder(n as usize), |(k, _)| k.clone());
    Some(Prepared { sql: out, params: (1..=max).map(|n| Param { key: key(n), index: n, hint: None }).collect() })
}

const TSQL_STATEMENT_WORDS: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "SET", "IF", "WHILE", "EXEC", "EXECUTE", "RETURN", "PRINT", "BEGIN", "END",
    "OPEN", "FETCH", "CLOSE", "DEALLOCATE", "WITH", "TRUNCATE", "DROP", "CREATE", "ALTER", "WAITFOR", "THROW", "RAISERROR",
];

pub fn detect_tsql(sql: &str) -> Option<Prepared> {
    let sig: Vec<Token> = tokenize_in(sql, Dialect::TSql).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let word = |i: usize| sig.get(i).filter(|t| matches!(t.kind, TokenKind::Keyword | TokenKind::Ident)).map(|t| t.text(sql).to_ascii_uppercase());
    let is_var = |t: &Token| t.kind == TokenKind::Ident && t.text(sql).starts_with('@') && !t.text(sql).starts_with("@@") && t.text(sql).len() > 1;
    for i in 0..sig.len() {
        if matches!(word(i).as_deref(), Some("CREATE" | "ALTER")) {
            let j = if word(i + 1).as_deref() == Some("OR") { i + 3 } else { i + 1 };
            if matches!(word(j).as_deref(), Some("PROC" | "PROCEDURE" | "FUNCTION" | "TRIGGER")) {
                return None;
            }
        }
    }
    let mut declared: Vec<String> = Vec::new();
    let mut in_declare = false;
    for (i, t) in sig.iter().enumerate() {
        match word(i).as_deref() {
            Some("DECLARE") => {
                in_declare = true;
                continue;
            }
            Some(x) if TSQL_STATEMENT_WORDS.contains(&x) => in_declare = false,
            _ => {}
        }
        if t.kind == TokenKind::Punct && t.text(sql) == ";" {
            in_declare = false;
        }
        let prev = i.checked_sub(1).and_then(|p| sig.get(p));
        let after_declare = prev.is_some_and(|p| p.text(sql) == "," || p.text(sql).eq_ignore_ascii_case("DECLARE"));
        if in_declare && is_var(t) && after_declare {
            declared.push(t.text(sql).to_lowercase());
        }
    }
    let mut named: Vec<String> = Vec::new();
    let mut at: Vec<(std::ops::Range<usize>, usize)> = Vec::new();
    let mut in_exec = false;
    for (i, t) in sig.iter().enumerate() {
        match word(i).as_deref() {
            Some(x) if TSQL_STATEMENT_WORDS.contains(&x) => in_exec = matches!(x, "EXEC" | "EXECUTE"),
            _ => {}
        }
        if !is_var(t) {
            continue;
        }
        let name = t.text(sql).to_lowercase();
        if declared.contains(&name) {
            continue;
        }
        let next_eq = sig.get(i + 1).is_some_and(|n| n.text(sql) == "=");
        let prev = i.checked_sub(1).and_then(|p| sig.get(p)).map(|p| p.text(sql).to_ascii_uppercase());
        if next_eq && (in_exec || matches!(prev.as_deref(), Some("SELECT" | "SET" | ","))) {
            continue;
        }
        let k = match named.iter().position(|n| *n == name) {
            Some(k) => k,
            None => {
                named.push(name);
                named.len() - 1
            }
        };
        at.push((t.start..t.end, k));
    }
    if named.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(sql.len());
    let mut pos = 0;
    for (r, k) in &at {
        out.push_str(&sql[pos..r.start]);
        out.push_str(&format!("@P{}", k + 1));
        pos = r.end;
    }
    out.push_str(&sql[pos..]);
    let keys: Vec<String> = named
        .iter()
        .map(|n| at.iter().find(|(_, k)| named[*k] == *n).map(|(r, _)| sql[r.clone()].to_string()).unwrap_or_else(|| n.clone()))
        .collect();
    Some(Prepared { sql: out, params: keys.into_iter().enumerate().map(|(k, key)| Param { key, index: k as u32 + 1, hint: None }).collect() })
}

pub fn detect(sql: &str) -> Option<Prepared> {
    let tokens = tokenize(sql);
    let sig: Vec<Token> = tokens.iter().copied().filter(|t| !t.kind.is_trivia()).collect();
    let first = sig.first()?.text(sql).to_ascii_uppercase();
    if matches!(first.as_str(), "PREPARE" | "CREATE" | "DO" | "DECLARE" | "EXECUTE" | "DEALLOCATE") {
        return None;
    }
    let mut max = 0u32;
    let mut named: Vec<String> = Vec::new();
    let mut vars: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let mut brackets = 0i32;
    for (i, t) in sig.iter().enumerate() {
        match (t.kind, t.text(sql)) {
            (TokenKind::Punct, "[") => brackets += 1,
            (TokenKind::Punct, "]") => brackets -= 1,
            (TokenKind::Param, s) => {
                if let Ok(n) = s[1..].parse::<u32>()
                    && n > 0
                {
                    max = max.max(n);
                }
            }
            (TokenKind::Punct, ":") if brackets <= 0 => {
                let Some(next) = sig.get(i + 1) else { continue };
                if next.start != t.end {
                    continue;
                }
                let name = match next.kind {
                    TokenKind::Ident | TokenKind::Keyword => next.text(sql).to_string(),
                    TokenKind::String if next.text(sql).starts_with('\'') && next.text(sql).len() >= 3 && next.text(sql).ends_with('\'') => {
                        let s = next.text(sql);
                        s[1..s.len() - 1].to_string()
                    }
                    _ => continue,
                };
                if !name.chars().all(|c| c.is_alphanumeric() || c == '_') || name.is_empty() {
                    continue;
                }
                if !named.contains(&name) {
                    named.push(name.clone());
                }
                vars.push((t.start..next.end, name));
            }
            _ => {}
        }
    }
    if max == 0 && named.is_empty() {
        return None;
    }
    let mut params: Vec<Param> = (1..=max).map(|n| Param { key: format!("${n}"), index: n, hint: None }).collect();
    for (k, name) in named.iter().enumerate() {
        params.push(Param { key: format!(":{name}"), index: max + 1 + k as u32, hint: None });
    }
    let mut out = String::with_capacity(sql.len());
    let mut pos = 0;
    for (r, name) in &vars {
        out.push_str(&sql[pos..r.start]);
        let ix = max + 1 + named.iter().position(|n| n == name).unwrap_or(0) as u32;
        out.push_str(&format!("${ix}"));
        pos = r.end;
    }
    out.push_str(&sql[pos..]);
    Some(Prepared { sql: out, params })
}

pub fn infer(p: &mut Prepared, snap: &SchemaSnapshot) {
    let sql = &p.sql;
    let tokens = tokenize_in(sql, snap.dialect);
    let sig: Vec<Token> = tokens.iter().copied().filter(|t| !t.kind.is_trivia()).collect();
    let a = scope::analyze_in(sql, &sig, snap.dialect);
    let text = |i: usize| sig.get(i).map_or("", |t| t.text(sql));
    let word = |i: usize, w: &str| sig.get(i).is_some_and(|t| matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) && t.text(sql).eq_ignore_ascii_case(w));
    let is_name = |i: usize| sig.get(i).is_some_and(|t| matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent));
    let col_type = |end: usize, at: usize| -> Option<String> {
        if !is_name(end) {
            return None;
        }
        let name = ident_value(sql, &sig[end], snap.dialect);
        let q = (end >= 2 && text(end - 1) == "." && is_name(end - 2)).then(|| ident_value(sql, &sig[end - 2], snap.dialect));
        let scope = a.scope_at(at)?;
        a.ref_type(scope, q.as_deref(), &name, snap)
    };
    let col_type_from = |start: usize, at: usize| -> Option<String> {
        if is_name(start) && text(start + 1) == "." && is_name(start + 2) { col_type(start + 2, at) } else { col_type(start, at) }
    };
    const CMP: &[&str] = &["=", "<>", "!=", "<", ">", "<=", ">="];
    let values_row = sig.iter().position(|t| t.text(sql).eq_ignore_ascii_case("VALUES"));
    let mut nth = 0u32;
    for (i, t) in sig.iter().enumerate() {
        let n = match (t.kind, t.text(sql)) {
            (TokenKind::Param, "?") => {
                nth += 1;
                nth
            }
            (TokenKind::Param, s) => {
                let Ok(n) = s[1..].parse::<u32>() else { continue };
                n
            }
            (TokenKind::Ident, s) if snap.dialect == Dialect::TSql && s.starts_with("@P") => {
                let Ok(n) = s[2..].parse::<u32>() else { continue };
                n
            }
            _ => continue,
        };
        let Some(param) = p.params.iter_mut().find(|x| x.index == n) else { continue };
        if param.hint.is_some() {
            continue;
        }
        let at = t.start;
        let hint = if text(i + 1) == "::" {
            Some(text(i + 2).to_string())
        } else if word(i + 1, "AS") && i >= 2 && text(i - 1) == "(" && word(i - 2, "CAST") {
            Some(text(i + 2).to_string())
        } else if i >= 1 && (word(i - 1, "LIMIT") || word(i - 1, "OFFSET") || word(i - 1, "NEXT")) || (i >= 2 && text(i - 1) == "(" && word(i - 2, "TOP")) {
            Some("bigint".into())
        } else if i >= 2 && (CMP.contains(&text(i - 1)) || word(i - 1, "LIKE") || word(i - 1, "ILIKE")) {
            col_type(i - 2, at)
        } else if CMP.contains(&text(i + 1)) {
            col_type_from(i + 2, at)
        } else {
            let mut k = i;
            while k >= 2 && text(k - 1) == "," {
                k -= 2;
            }
            if k >= 3 && text(k - 1) == "(" && word(k - 2, "IN") {
                col_type(k - 3, at)
            } else if let Some(v) = values_row
                && i > v
            {
                let open = (v..i).rev().find(|&j| text(j) == "(");
                open.and_then(|o| {
                    let pos = (o + 1..i).filter(|&j| text(j) == ",").count();
                    let tgt: Vec<&scope::TargetCol> = a.targets.iter().filter(|x| x.scope == 0).collect();
                    let target = tgt.get(pos)?;
                    let src = a.scopes.first()?.sources.get(target.source)?;
                    a.source_columns(0, src, snap)?.into_iter().find(|(c, _)| *c == target.name).map(|(_, t)| t)
                })
            } else {
                None
            }
        };
        param.hint = hint.filter(|h| !h.is_empty());
    }
}

pub fn bind_value(text: &str) -> Option<String> {
    if text.trim().eq_ignore_ascii_case("null") { None } else { Some(text.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Column, RelKind, Relation};
    use std::collections::HashMap;

    fn keys(sql: &str) -> Vec<String> {
        detect(sql).map(|p| p.params.into_iter().map(|x| x.key).collect()).unwrap_or_default()
    }

    #[test]
    fn detection_ignores_casts_strings_and_bodies() {
        assert_eq!(keys("select * from t where a = $1 and b = $2"), vec!["$1", "$2"]);
        assert_eq!(keys("select $2"), vec!["$1", "$2"], "a gap still needs a value");
        assert_eq!(keys("select a::int, b::text from t"), Vec::<String>::new());
        assert_eq!(keys("select ':x', '$1', E'\\':y' from t -- :z $3\n /* :w */"), Vec::<String>::new());
        assert_eq!(keys("select $$ :x $1 $$, $f$ $2 $f$"), Vec::<String>::new());
        assert_eq!(keys("select arr[1:n], arr[:2] from t"), Vec::<String>::new());
        assert_eq!(keys("create function f(int) returns int as $$ select $1 $$ language sql"), Vec::<String>::new());
        assert_eq!(keys("prepare p as select $1"), Vec::<String>::new());
        assert_eq!(keys("do $$ begin x := 1; end $$"), Vec::<String>::new());
        assert_eq!(keys("select a : b from t"), Vec::<String>::new(), "a colon with a space is not a variable");
    }

    #[test]
    fn mysql_question_marks() {
        let p = detect_in("SELECT * FROM t WHERE a = ? AND b = '?' AND c IN (?, ?) # ?", Dialect::MySql).unwrap();
        assert_eq!(p.params.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), vec!["?1", "?2", "?3"]);
        assert_eq!(p.sql, "SELECT * FROM t WHERE a = ? AND b = '?' AND c IN (?, ?) # ?", "sent as written");
        assert!(detect_in("SELECT @v := 1, :x", Dialect::MySql).is_none());
        assert!(detect_in("CREATE PROCEDURE p() SELECT ?", Dialect::MySql).is_none());
        let col = |n: &str, t: &str| Column { name: n.into(), type_name: t.into(), nullable: None, primary_key: false, default: None };
        let rel = |n: &str| Relation { schema: "shop".into(), name: n.into(), kind: RelKind::Table, oid: None };
        let mut cols = HashMap::new();
        cols.insert(0, vec![col("id", "int"), col("Status", "varchar(20)")]);
        let mut snap = SchemaSnapshot::new(vec!["shop".into()], vec!["shop".into()], vec![rel("orders")], cols);
        snap.dialect = Dialect::MySql;
        let mut p = detect_in("SELECT * FROM orders WHERE `Status` = ? AND id > ? LIMIT ?", Dialect::MySql).unwrap();
        infer(&mut p, &snap);
        assert_eq!(p.params.iter().map(|x| x.hint.clone().unwrap_or_default()).collect::<Vec<_>>(), vec!["varchar(20)", "int", "bigint"]);
    }

    #[test]
    fn sqlite_and_duckdb_parameters() {
        let p = detect_in("SELECT * FROM t WHERE a = ? AND b = :name AND c = ?5 AND d = ? AND e = :name AND f = '?' -- ?", Dialect::Sqlite).unwrap();
        assert_eq!(p.sql, "SELECT * FROM t WHERE a = ?1 AND b = ?2 AND c = ?5 AND d = ?6 AND e = ?2 AND f = '?' -- ?");
        assert_eq!(p.params.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), vec!["?1", ":name", "?3", "?4", "?5", "?6"]);
        let p = detect_in("SELECT @a, $b, @a", Dialect::Sqlite).unwrap();
        assert_eq!((p.sql.as_str(), p.params.len()), ("SELECT ?1, ?2, ?1", 2));
        assert!(detect_in("CREATE TRIGGER x AFTER INSERT ON t BEGIN SELECT 1; END", Dialect::Sqlite).is_none());
        assert!(detect_in("SELECT [a?], `b?`, \"c?\"", Dialect::Sqlite).is_none());
        let p = detect_in("SELECT ?, ?, [1, 2][1:2] FROM t WHERE x = ?", Dialect::DuckDb).unwrap();
        assert_eq!(p.sql, "SELECT $1, $2, [1, 2][1:2] FROM t WHERE x = $3");
        let p = detect_in("SELECT $2, $1", Dialect::DuckDb).unwrap();
        assert_eq!(p.params.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), vec!["$1", "$2"]);
        assert!(detect_in("SELECT $$ ? $$, x::INT", Dialect::DuckDb).is_none());
    }

    #[test]
    fn tsql_variables() {
        let d = Dialect::TSql;
        let p = detect_in("SELECT * FROM orders WHERE customer_id = @Customer AND status = @s OR @s IS NULL AND x = @customer -- @nope\n AND y = '@no'", d).unwrap();
        assert_eq!(p.params.iter().map(|x| (x.key.as_str(), x.index)).collect::<Vec<_>>(), vec![("@Customer", 1), ("@s", 2)]);
        assert_eq!(p.sql, "SELECT * FROM orders WHERE customer_id = @P1 AND status = @P2 OR @P2 IS NULL AND x = @P1 -- @nope\n AND y = '@no'");
        assert!(detect_in("DECLARE @a int = 1, @b int; SELECT @a + @b, @@ROWCOUNT", d).is_none());
        assert!(detect_in("DECLARE @t TABLE (a int); INSERT @t VALUES (1); SELECT * FROM @t", d).is_none());
        assert!(detect_in("EXEC dbo.p @customer = 1, @total = 2", d).is_none());
        assert!(detect_in("CREATE PROCEDURE p @a int AS SELECT @a", d).is_none());
        let p = detect_in("EXEC dbo.p @customer = @id", d).unwrap();
        assert_eq!(p.params[0].key, "@id");
        let p = detect_in("SELECT TOP (@n) * FROM t WHERE @id = id", d).unwrap();
        assert_eq!(p.params.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(), vec!["@n", "@id"]);
        let col = |n: &str, t: &str| Column { name: n.into(), type_name: t.into(), nullable: None, primary_key: false, default: None };
        let rel = |n: &str| Relation { schema: "dbo".into(), name: n.into(), kind: RelKind::Table, oid: None };
        let mut cols = HashMap::new();
        cols.insert(0, vec![col("id", "int"), col("status", "varchar(12)")]);
        let mut snap = SchemaSnapshot::new(vec!["dbo".into()], vec!["dbo".into()], vec![rel("orders")], cols);
        snap.dialect = d;
        let mut p = detect_in("SELECT TOP (@n) * FROM [orders] WHERE status = @s AND id > @id", d).unwrap();
        infer(&mut p, &snap);
        assert_eq!(p.params.iter().map(|x| x.hint.clone().unwrap_or_default()).collect::<Vec<_>>(), vec!["bigint", "varchar(12)", "int"]);
    }

    #[test]
    fn variables_become_positional() {
        let p = detect("select * from t where a = :id and b = :'name' and c = :id and d = $1").unwrap();
        assert_eq!(p.sql, "select * from t where a = $2 and b = $3 and c = $2 and d = $1");
        assert_eq!(p.params.iter().map(|x| (x.key.as_str(), x.index)).collect::<Vec<_>>(), vec![("$1", 1), (":id", 2), (":name", 3)]);
        assert_eq!(bind_value("NULL"), None);
        assert_eq!(bind_value("42"), Some("42".into()));
    }

    #[test]
    fn types_from_the_statement() {
        let col = |n: &str, t: &str| Column { name: n.into(), type_name: t.into(), nullable: None, primary_key: false, default: None };
        let rel = |n: &str| Relation { schema: "public".into(), name: n.into(), kind: RelKind::Table, oid: None };
        let mut cols = HashMap::new();
        cols.insert(0, vec![col("id", "bigint"), col("status", "text"), col("created", "timestamp with time zone")]);
        let snap = SchemaSnapshot::new(vec!["public".into()], vec!["public".into()], vec![rel("orders")], cols);
        let hints = |sql: &str| {
            let mut p = detect(sql).unwrap();
            infer(&mut p, &snap);
            p.params.into_iter().map(|x| x.hint.unwrap_or_default()).collect::<Vec<_>>()
        };
        assert_eq!(hints("select * from orders p where p.id = $1 and $2 = status limit $3"), vec!["bigint", "text", "bigint"]);
        assert_eq!(hints("select * from orders where created > :since and status in (:a, :b)"), vec!["timestamp with time zone", "text", "text"]);
        assert_eq!(hints("select $1::int, cast($2 as date)"), vec!["int", "date"]);
        assert_eq!(hints("insert into orders (status, id) values ($1, $2)"), vec!["text", "bigint"]);
        assert_eq!(hints("update orders set status = $1 where id = $2"), vec!["text", "bigint"]);
        assert_eq!(hints("select * from missing where x = $1"), vec![""]);
    }
}
