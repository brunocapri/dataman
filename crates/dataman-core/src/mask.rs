//! Passwords written into statements (`CREATE USER … PASSWORD '…'`, `AUTH …`, `pwd: "…"`)
//! never reach the history or the audit log: they are kept as `••••••`.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

use crate::config::Engine;
use crate::sql::{Dialect, TokenKind, tokenize_in};

pub const MASK: &str = "••••••";

/// SQL words after which the next value is a secret.
const ARMS: &[&str] = &["PASSWORD", "OLD_PASSWORD", "IDENTIFIED", "SECRET", "PASSPHRASE", "PASSWD"];

pub fn statement(engine: Engine, text: &str) -> Cow<'_, str> {
    let masked = match engine {
        Engine::Redis => redis(text),
        e if e.is_sql() => sql(text, e.dialect()),
        _ => Cow::Borrowed(text),
    };
    match inline(&masked) {
        Cow::Borrowed(_) => masked,
        Cow::Owned(s) => Cow::Owned(s),
    }
}

/// Whatever the engine: every engine's masking (for text whose engine is not known).
pub fn any(text: &str) -> Cow<'_, str> {
    let s = sql(text, Dialect::Postgres).into_owned();
    let s = redis(&s).into_owned();
    match inline(&s) {
        Cow::Borrowed(_) if s == text => Cow::Borrowed(text),
        c => Cow::Owned(c.into_owned()),
    }
}

fn sql(text: &str, dialect: Dialect) -> Cow<'_, str> {
    let lower = text.to_ascii_lowercase();
    if !["password", "identified", "secret", "passphrase", "passwd"].iter().any(|w| lower.contains(w)) {
        return Cow::Borrowed(text);
    }
    let toks: Vec<_> = tokenize_in(text, dialect).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let mut cuts = Vec::new();
    let mut armed = false;
    for (i, t) in toks.iter().enumerate() {
        let word = t.text(text);
        match t.kind {
            TokenKind::Keyword | TokenKind::Ident | TokenKind::QuotedIdent if ARMS.iter().any(|a| word.trim_matches(['"', '`', '[', ']']).eq_ignore_ascii_case(a)) => armed = true,
            TokenKind::Punct if word == ";" => armed = false,
            TokenKind::String | TokenKind::DollarString if armed => {
                let prev = i.checked_sub(1).map(|p| toks[p].text(text).to_ascii_uppercase()).unwrap_or_default();
                if ARMS.contains(&prev.trim_matches(['"', '`', '[', ']'])) || matches!(prev.as_str(), "=" | "(" | "BY" | "N") {
                    cuts.push(t.start..t.end);
                    armed = false;
                }
            }
            _ => {}
        }
    }
    if cuts.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for r in cuts {
        out.push_str(&text[at..r.start]);
        let lit = &text[r.clone()];
        let q = if lit.starts_with('$') { "$$" } else { "'" };
        let lead = if lit.starts_with(['N', 'n', 'E', 'e']) && lit.len() > 1 && lit.as_bytes()[1] == b'\'' { &lit[..1] } else { "" };
        out.push_str(&format!("{lead}{q}{MASK}{q}"));
        at = r.end;
    }
    out.push_str(&text[at..]);
    Cow::Owned(out)
}

fn redis(text: &str) -> Cow<'_, str> {
    let Ok(args) = crate::nosql::redis::parse(text) else { return Cow::Borrowed(text) };
    let up = |i: usize| args.get(i).map(|a| a.to_ascii_uppercase()).unwrap_or_default();
    let mut hide = vec![false; args.len()];
    match up(0).as_str() {
        "AUTH" => hide.iter_mut().skip(1).for_each(|h| *h = true),
        "HELLO" | "MIGRATE" => {
            for i in 1..args.len() {
                match up(i).as_str() {
                    "AUTH" if up(0) == "HELLO" => hide.iter_mut().skip(i + 1).take(2).for_each(|h| *h = true),
                    "AUTH" => hide.iter_mut().skip(i + 1).take(1).for_each(|h| *h = true),
                    "AUTH2" => hide.iter_mut().skip(i + 1).take(2).for_each(|h| *h = true),
                    _ => {}
                }
            }
        }
        "ACL" if up(1) == "SETUSER" => {
            for (i, a) in args.iter().enumerate().skip(3) {
                hide[i] = a.starts_with(['>', '<', '#', '!']);
            }
        }
        "CONFIG" if up(1) == "SET" => {
            let mut i = 2;
            while i + 1 < args.len() {
                if matches!(up(i).as_str(), "REQUIREPASS" | "MASTERAUTH" | "MASTERUSER") {
                    hide[i + 1] = true;
                }
                i += 2;
            }
        }
        _ => {}
    }
    if !hide.iter().any(|h| *h) {
        return Cow::Borrowed(text);
    }
    let shown: Vec<String> = args
        .iter()
        .zip(&hide)
        .map(|(a, h)| match (h, a.chars().next()) {
            (true, Some(c @ ('>' | '<' | '#' | '!'))) if up(0) == "ACL" => format!("{c}{MASK}"),
            (true, _) => MASK.to_string(),
            (false, _) => crate::nosql::redis::quote(a),
        })
        .collect();
    Cow::Owned(shown.join(" "))
}

static KEYED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(["']?\b(?:pwd|password|passwd|secret|requirepass|masterauth)["']?\s*:\s*)("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')"#).unwrap()
});
static CHANGE_PW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(changeUserPassword\s*\(\s*(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')\s*,\s*)("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')"#).unwrap());
static KV: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)(\b(?:password|pwd)\s*=\s*)([^\s;'"&,)]+)"#).unwrap());
static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)(\b[a-z][a-z0-9+.-]*://[^:/@\s'"]+:)([^@\s'"/]+)(@)"#).unwrap());

/// Secrets in documents and strings: `pwd: "…"`, `"password": "…"`, `password=…`,
/// `scheme://user:pass@`.
fn inline(text: &str) -> Cow<'_, str> {
    let quoted = |c: &regex::Captures| {
        let v = &c[2];
        let q = &v[..1];
        format!("{}{q}{MASK}{q}", &c[1])
    };
    let s = KEYED.replace_all(text, quoted);
    let s = match CHANGE_PW.replace_all(&s, quoted) {
        Cow::Borrowed(_) => s,
        Cow::Owned(o) => Cow::Owned(o),
    };
    let src = s.clone();
    let s = match KV.replace_all(&src, |c: &regex::Captures| {
        // `PASSWORD = N'…'`: a string prefix, not the value (masked above).
        let whole = c.get(0).unwrap();
        if c[2].len() == 1 && "NnEeBbXxUu".contains(&c[2]) && src[whole.end()..].starts_with('\'') {
            return whole.as_str().to_string();
        }
        format!("{}{MASK}", &c[1])
    }) {
        Cow::Borrowed(_) => s,
        Cow::Owned(o) => Cow::Owned(o),
    };
    match URL.replace_all(&s, |c: &regex::Captures| format!("{}{MASK}{}", &c[1], &c[3])) {
        Cow::Borrowed(_) => s,
        Cow::Owned(o) => Cow::Owned(o),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Engine::*;

    #[test]
    fn sql_passwords() {
        let m = |e: Engine, s: &str| statement(e, s).into_owned();
        for (e, sql, want) in [
            (Postgres, "CREATE USER bob PASSWORD 'hunter2'", "CREATE USER bob PASSWORD '••••••'"),
            (Postgres, "ALTER ROLE bob WITH ENCRYPTED PASSWORD 'p''w' VALID UNTIL '2030-01-01'", "ALTER ROLE bob WITH ENCRYPTED PASSWORD '••••••' VALID UNTIL '2030-01-01'"),
            (Postgres, "create role x login password $$s3cr$t$$", "create role x login password $$••••••$$"),
            (Postgres, "CREATE USER MAPPING FOR bob SERVER s OPTIONS (user 'bob', password 'pw1')", "CREATE USER MAPPING FOR bob SERVER s OPTIONS (user 'bob', password '••••••')"),
            (Postgres, "SELECT dblink_connect('host=db user=u password=pw2 dbname=x')", "SELECT dblink_connect('host=db user=u password=•••••• dbname=x')"),
            (Postgres, "SELECT * FROM users WHERE password = 'not a secret? still masked'", "SELECT * FROM users WHERE password = '••••••'"),
            (MySql, "CREATE USER 'app'@'%' IDENTIFIED BY 'pw3'", "CREATE USER 'app'@'%' IDENTIFIED BY '••••••'"),
            (MySql, "ALTER USER 'app'@'%' IDENTIFIED WITH caching_sha2_password BY 'p\\'w4'", "ALTER USER 'app'@'%' IDENTIFIED WITH caching_sha2_password BY '••••••'"),
            (MySql, "SET PASSWORD FOR 'app'@'%' = 'pw5'", "SET PASSWORD FOR 'app'@'%' = '••••••'"),
            (SqlServer, "CREATE LOGIN app WITH PASSWORD = N'pw6', CHECK_POLICY = OFF", "CREATE LOGIN app WITH PASSWORD = N'••••••', CHECK_POLICY = OFF"),
            (SqlServer, "ALTER LOGIN app WITH PASSWORD = 'new7' OLD_PASSWORD = 'old7'", "ALTER LOGIN app WITH PASSWORD = '••••••' OLD_PASSWORD = '••••••'"),
            (SqlServer, "CREATE MASTER KEY ENCRYPTION BY PASSWORD = 'k8'", "CREATE MASTER KEY ENCRYPTION BY PASSWORD = '••••••'"),
            (Postgres, "SELECT 'postgres://bob:pw9@db/app'", "SELECT 'postgres://bob:••••••@db/app'"),
        ] {
            assert_eq!(m(e, sql), want, "{sql}");
        }
        for s in ["SELECT * FROM t WHERE name = 'password'", "SELECT 1", "UPDATE users SET name = 'x' WHERE id = 1"] {
            assert_eq!(m(Postgres, s), s);
        }
    }

    #[test]
    fn nosql_passwords() {
        let m = |e: Engine, s: &str| statement(e, s).into_owned();
        assert_eq!(m(Redis, "AUTH hunter2"), "AUTH ••••••");
        assert_eq!(m(Redis, "auth default \"p w\""), "auth •••••• ••••••");
        assert_eq!(m(Redis, "HELLO 3 AUTH bob pw SETNAME x"), "HELLO 3 AUTH •••••• •••••• SETNAME x");
        assert_eq!(m(Redis, "CONFIG SET requirepass pw maxmemory 1mb"), "CONFIG SET requirepass •••••• maxmemory 1mb");
        assert_eq!(m(Redis, "ACL SETUSER bob on >pw ~* +@all"), "ACL SETUSER bob on >•••••• ~* +@all");
        assert_eq!(m(Redis, "MIGRATE h 6379 k 0 5000 AUTH2 bob pw"), "MIGRATE h 6379 k 0 5000 AUTH2 •••••• ••••••");
        assert_eq!(m(Redis, "GET password"), "GET password");
        assert_eq!(m(MongoDb, "db.createUser({user: 'bob', pwd: 'pw', roles: []})"), "db.createUser({user: 'bob', pwd: '••••••', roles: []})");
        assert_eq!(m(MongoDb, r#"db.changeUserPassword("bob", "pw")"#), r#"db.changeUserPassword("bob", "••••••")"#);
        assert_eq!(m(Elasticsearch, "PUT _security/user/bob\n{\"password\": \"pw\", \"roles\": []}"), "PUT _security/user/bob\n{\"password\": \"••••••\", \"roles\": []}");
        assert_eq!(any("ALTER USER x PASSWORD 'pw'"), "ALTER USER x PASSWORD '••••••'");
        assert_eq!(any("SELECT 1"), "SELECT 1");
    }
}
