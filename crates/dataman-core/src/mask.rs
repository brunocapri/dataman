//! Passwords written into statements (`CREATE USER … PASSWORD '…'`, `AUTH …`, `pwd: "…"`,
//! `remote(…, 'pw')`) never reach the history or the audit log: they are kept as `••••••`.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

use crate::config::Engine;
use crate::sql::{Dialect, ident_value_in, tokenize_in, TokenKind};

pub const MASK: &str = "••••••";

/// SQL words after which the next value is a secret.
const ARMS: &[&str] = &["PASSWORD", "OLD_PASSWORD", "IDENTIFIED", "SECRET", "PASSPHRASE", "PASSWD"];

/// ClickHouse's table functions and engines that take credentials among their plain arguments
/// (lower case), with the first argument that can hold one: every string from there on is masked,
/// inside calls too (`headers('Authorization' = '…')`), since the positions move from one form of
/// a call to another (`remote('h', db.t, 'u', 'pw')`).
const CLICKHOUSE_CREDENTIALS: &[(&str, usize)] = &[
    ("aes_decrypt_mysql", 2),
    ("aes_encrypt_mysql", 2),
    ("arrowflight", 1),
    ("azureblobstorage", 0),
    ("azureblobstoragecluster", 1),
    ("azurequeue", 0),
    ("clickhouse", 2),
    ("cos", 1),
    ("cosn", 1),
    ("decrypt", 2),
    ("deltalake", 1),
    ("deltalakeazure", 3),
    ("deltalakes3", 1),
    ("encrypt", 2),
    ("gcs", 1),
    ("headers", 0),
    ("hudi", 1),
    ("iceberg", 1),
    ("icebergazure", 3),
    ("icebergs3", 1),
    ("materializedmysql", 3),
    ("materializedpostgresql", 3),
    ("mongodb", 3),
    ("mysql", 3),
    ("oss", 1),
    ("postgresql", 3),
    ("redis", 3),
    ("remote", 2),
    ("remotesecure", 2),
    ("s3", 1),
    ("s3cluster", 2),
    ("s3queue", 1),
    ("trydecrypt", 2),
    ("ytsaurus", 2),
];

pub fn statement(engine: Engine, text: &str) -> Cow<'_, str> {
    let masked = match engine {
        Engine::Redis => redis(text),
        e if e.is_sql() => sql(text, e.dialect()),
        _ => Cow::Borrowed(text),
    };
    let masked = if engine == Engine::ClickHouse {
        match clickhouse_arguments(&masked) {
            Cow::Owned(s) => Cow::Owned(s),
            Cow::Borrowed(_) => masked,
        }
    } else {
        masked
    };
    match inline(&masked) {
        Cow::Borrowed(_) => masked,
        Cow::Owned(s) => Cow::Owned(s),
    }
}

/// Whatever the engine: every engine's masking (for text whose engine is not known).
pub fn any(text: &str) -> Cow<'_, str> {
    let s = sql(text, Dialect::Postgres).into_owned();
    let s = clickhouse_arguments(&s).into_owned();
    let s = redis(&s).into_owned();
    match inline(&s) {
        Cow::Borrowed(_) if s == text => Cow::Borrowed(text),
        c => Cow::Owned(c.into_owned()),
    }
}

/// A word after which the next value is a secret: `SECRET`, or settings' `kafka_sasl_password`.
fn arms(word: &str) -> bool {
    let w = word.trim_matches(['"', '`', '[', ']']);
    ARMS.iter().any(|a| w.eq_ignore_ascii_case(a)) || ["_password", "_secret", "_passwd", "secret_access_key"].iter().any(|s| w.len() >= s.len() && w.is_char_boundary(w.len() - s.len()) && w[w.len() - s.len()..].eq_ignore_ascii_case(s))
}

/// The string arguments of ClickHouse's calls that can hold credentials.
fn clickhouse_arguments(text: &str) -> Cow<'_, str> {
    let d = Dialect::ClickHouse;
    let toks: Vec<_> = tokenize_in(text, d).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let mut cuts: Vec<std::ops::Range<usize>> = Vec::new();
    let mut i = 0;
    while i + 1 < toks.len() {
        let open = toks[i + 1].kind == TokenKind::Punct && toks[i + 1].text(text) == "(";
        let from = matches!(toks[i].kind, TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword).then(|| ident_value_in(text, &toks[i], d).to_ascii_lowercase()).and_then(|n| CLICKHOUSE_CREDENTIALS.iter().find(|(c, _)| *c == n)).map(|(_, f)| *f);
        let (Some(from), true) = (from, open) else {
            i += 1;
            continue;
        };
        let (mut depth, mut arg) = (0usize, 0usize);
        let mut k = i + 1;
        while let Some(t) = toks.get(k) {
            match (t.kind, t.text(text)) {
                (TokenKind::Punct, "(") => depth += 1,
                (TokenKind::Punct, ")") => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                (TokenKind::Punct, ",") if depth == 1 => arg += 1,
                (TokenKind::String, _) if arg >= from => cuts.push(t.start..t.end),
                _ => {}
            }
            k += 1;
        }
        // Past the call: a call inside it has had its strings counted with it.
        i = k + 1;
    }
    if cuts.is_empty() { Cow::Borrowed(text) } else { Cow::Owned(with_masks(text, cuts)) }
}

fn with_masks(text: &str, cuts: Vec<std::ops::Range<usize>>) -> String {
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
    out
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
            TokenKind::Keyword | TokenKind::Ident | TokenKind::QuotedIdent if arms(word) => armed = true,
            TokenKind::Punct if word == ";" => armed = false,
            TokenKind::String | TokenKind::DollarString if armed => {
                let prev = i.checked_sub(1).map(|p| toks[p].text(text).to_ascii_uppercase()).unwrap_or_default();
                if arms(&prev) || matches!(prev.as_str(), "=" | "(" | "BY" | "N") {
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
    Cow::Owned(with_masks(text, cuts))
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
    fn clickhouse_credentials_in_arguments() {
        let m = |s: &str| statement(ClickHouse, s).into_owned();
        for (sql, want) in [
            ("SELECT * FROM remote('h:9000', 'db', 't', 'u', 'pw1')", "SELECT * FROM remote('h:9000', 'db', '••••••', '••••••', '••••••')"),
            ("SELECT * FROM remote('h:9000', db.t, 'u', 'pw2')", "SELECT * FROM remote('h:9000', db.t, '••••••', '••••••')"),
            ("SELECT * FROM `re\\x6Dote`('h', 'db', 't', 'u', 'pw3')", "SELECT * FROM `re\\x6Dote`('h', 'db', '••••••', '••••••', '••••••')"),
            ("SELECT * FROM s3('https://b/x.csv', 'AKIA', 'pw4', 'CSV')", "SELECT * FROM s3('https://b/x.csv', '••••••', '••••••', '••••••')"),
            ("CREATE TABLE t (a Int8) ENGINE = MySQL('h:3306', 'db', 't', 'u', 'pw5')", "CREATE TABLE t (a Int8) ENGINE = MySQL('h:3306', 'db', 't', '••••••', '••••••')"),
            ("CREATE TABLE t (a Int8) ENGINE = S3('https://b/x.csv', 'AKIA', 'pw6')", "CREATE TABLE t (a Int8) ENGINE = S3('https://b/x.csv', '••••••', '••••••')"),
            ("CREATE TABLE t (a Int8) ENGINE = Kafka SETTINGS kafka_sasl_password = 'pw7'", "CREATE TABLE t (a Int8) ENGINE = Kafka SETTINGS kafka_sasl_password = '••••••'"),
            ("BACKUP TABLE t TO S3('https://b/x', 'AKIA', 'pw8')", "BACKUP TABLE t TO S3('https://b/x', '••••••', '••••••')"),
            ("SELECT * FROM url('https://x', CSV, 'a Int8', headers('Authorization' = 'Bearer pw9'))", "SELECT * FROM url('https://x', CSV, 'a Int8', headers('••••••' = '••••••'))"),
            ("SELECT encrypt('aes-256-gcm', 'text', 'pw10', 'iv')", "SELECT encrypt('aes-256-gcm', 'text', '••••••', '••••••')"),
            ("SELECT * FROM s3('https://b/x.csv', access_key_id = 'AKIA', secret_access_key = 'p\\'w11')", "SELECT * FROM s3('https://b/x.csv', access_key_id = '••••••', secret_access_key = '••••••')"),
        ] {
            assert_eq!(m(sql), want, "{sql}");
            assert!(!any(sql).contains("pw"), "{sql}: {}", any(sql));
        }
        for s in ["SELECT remote FROM t WHERE mysql = 'x'", "SELECT * FROM numbers(3)", "SELECT 'remote(a, b, c, d)'"] {
            assert_eq!(m(s), s);
        }
        // Elsewhere those are other functions, and a statement's values stay.
        assert_eq!(statement(Postgres, "SELECT encrypt('a', 'b', 'c')"), "SELECT encrypt('a', 'b', 'c')");
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
