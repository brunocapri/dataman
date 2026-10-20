use crate::config::{ConnectionProfile, Engine, SslMode};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnSpec {
    pub engine: Engine,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub database: Option<String>,
    pub password: Option<String>,
    pub sslmode: Option<SslMode>,
    pub service: Option<String>,
    pub encrypt: Option<String>,
    pub trust_cert: Option<bool>,
    pub options: std::collections::BTreeMap<String, String>,
}

impl ConnSpec {
    pub fn set_mssql(&mut self, key: &str, value: &str) {
        let v = value.trim();
        let yes = || matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1");
        match key {
            "server" | "data source" | "address" | "addr" | "network address" | "host" => {
                let v = v.strip_prefix("tcp:").unwrap_or(v);
                let (h, p) = match v.rsplit_once(',') {
                    Some((h, p)) => (h, p.trim().parse().ok()),
                    None => (v, None),
                };
                let (h, instance) = match h.trim().split_once('\\') {
                    Some((h, i)) => (h, Some(i)),
                    None => (h.trim(), None),
                };
                let h = if matches!(h, "." | "(local)") { "localhost" } else { h };
                let h = match instance {
                    Some(i) => format!("{h}\\{i}"),
                    None => h.to_string(),
                };
                self.host = Some(h).filter(|h| !h.is_empty());
                self.port = p.or(self.port);
            }
            "port" | "portnumber" => self.port = v.parse().ok(),
            "database" | "databasename" | "initial catalog" | "dbname" => self.database = Some(v.to_string()),
            "user" | "user id" | "userid" | "uid" | "username" | "user name" => self.user = Some(v.to_string()),
            "password" | "pwd" => self.password = Some(v.to_string()),
            "encrypt" => self.encrypt = Some(v.to_ascii_lowercase()),
            "trustservercertificate" | "trust server certificate" => self.trust_cert = Some(yes()),
            _ => {}
        }
    }

    fn mssql_sslmode(&self) -> Option<SslMode> {
        let trust = self.trust_cert == Some(true);
        Some(match self.encrypt.as_deref() {
            Some("false" | "no" | "0") => SslMode::Disable,
            Some("optional") => SslMode::Prefer,
            Some("true" | "yes" | "1" | "mandatory" | "strict") => {
                if trust {
                    SslMode::Require
                } else {
                    SslMode::VerifyFull
                }
            }
            _ if trust => SslMode::Require,
            _ => return None,
        })
    }
    pub fn set(&mut self, key: &str, value: &str) {
        let first = || value.split(',').next().unwrap_or("").trim().to_string();
        match key {
            "host" => self.host = Some(first()).filter(|h| !h.is_empty()),
            "hostaddr" if self.host.is_none() => self.host = Some(first()).filter(|h| !h.is_empty()),
            "port" => self.port = first().parse().ok(),
            "user" => self.user = Some(value.to_string()),
            "dbname" => self.database = Some(value.to_string()),
            "password" => self.password = Some(value.to_string()),
            "sslmode" => self.sslmode = sslmode(value),
            "ssl-mode" | "ssl_mode" | "sslMode" => self.sslmode = sslmode(&value.to_ascii_lowercase()),
            "ssl" if value == "true" || value == "1" => self.sslmode = Some(SslMode::Require),
            "useSSL" => self.sslmode = Some(if value == "true" { SslMode::Require } else { SslMode::Disable }),
            "service" => self.service = Some(value.to_string()),
            _ => {}
        }
    }

    pub fn set_mongo(&mut self, key: &str, value: &str) {
        let yes = matches!(value.to_ascii_lowercase().as_str(), "true" | "1" | "yes");
        match key {
            "tls" | "ssl" => {
                self.sslmode = Some(if !yes {
                    SslMode::Disable
                } else if self.sslmode == Some(SslMode::Require) {
                    SslMode::Require
                } else {
                    SslMode::VerifyFull
                })
            }
            "tlsAllowInvalidCertificates" | "tlsInsecure" if yes => self.sslmode = Some(SslMode::Require),
            "authSource" | "replicaSet" | "authMechanism" | "directConnection" | "srv" => {
                self.options.insert(key.to_string(), value.to_string());
            }
            _ => {}
        }
    }

    pub fn or(mut self, other: ConnSpec) -> ConnSpec {
        self.host = self.host.or(other.host);
        self.port = self.port.or(other.port);
        self.user = self.user.or(other.user);
        self.database = self.database.or(other.database);
        self.password = self.password.or(other.password);
        self.sslmode = self.sslmode.or(other.sslmode);
        self
    }

    pub fn into_profile(self) -> (ConnectionProfile, Option<String>) {
        let mssql_mode = self.mssql_sslmode();
        let mut p = ConnectionProfile::new("");
        p.engine = self.engine;
        p.options = self.options.clone();
        p.host = self.host.unwrap_or_else(|| "localhost".into());
        p.port = self.port.unwrap_or(self.engine.default_port());
        match self.engine {
            Engine::Postgres => {
                p.user = self.user.filter(|u| !u.is_empty()).unwrap_or_else(local_user);
                p.database = self.database.filter(|d| !d.is_empty()).unwrap_or_else(|| p.user.clone());
            }
            _ => {
                let user = self.engine.default_user();
                p.user = self.user.filter(|u| !u.is_empty()).unwrap_or_else(|| user.into());
                p.database = self.database.unwrap_or_default();
            }
        }
        if let Some(m) = self.sslmode.or(mssql_mode.filter(|_| self.engine == Engine::SqlServer)) {
            p.sslmode = m;
        }
        p.name = match &self.service {
            Some(s) => s.clone(),
            None if p.database.is_empty() => format!("{}@{}", p.user, p.host),
            None => format!("{}@{}", p.database, p.host),
        };
        (p, self.password.filter(|p| !p.is_empty()))
    }
}

pub(crate) fn local_user() -> String {
    std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_else(|_| "postgres".into())
}

/// `allow` reads as `prefer` and `verify-ca` as `verify-full`: the closest modes Dataman has,
/// never a weaker one.
pub fn sslmode(v: &str) -> Option<SslMode> {
    Some(match v {
        "disable" | "disabled" => SslMode::Disable,
        "allow" | "prefer" | "preferred" => SslMode::Prefer,
        "require" | "required" => SslMode::Require,
        "verify-ca" | "verify-full" | "verify_ca" | "verify_identity" => SslMode::VerifyFull,
        _ => return None,
    })
}

pub fn parse(s: &str) -> Option<ConnSpec> {
    let s = s.trim();
    let s = match s.strip_prefix("jdbc:") {
        Some(rest) if !rest.starts_with("sqlserver://") => rest,
        _ => s,
    };
    if ["postgres://", "postgresql://", "mysql://", "mariadb://", "mongodb://", "mongodb+srv://", "redis://", "rediss://"].iter().any(|p| s.starts_with(p)) {
        parse_url(s)
    } else if let Some(rest) = s.strip_prefix("sqlserver://").or_else(|| s.strip_prefix("jdbc:sqlserver://")) {
        parse_sqlserver(rest)
    } else if s.contains(';')
        && let Some(spec) = parse_ado(s)
    {
        Some(spec)
    } else {
        parse_kv(s)
    }
}

fn parse_sqlserver(rest: &str) -> Option<ConnSpec> {
    let mut spec = ConnSpec { engine: Engine::SqlServer, ..Default::default() };
    let jdbc = rest.contains(';') || !(rest.contains('@') || rest.contains('/') || rest.contains('?'));
    if jdbc {
        let (head, props) = rest.split_once(';').unwrap_or((rest, ""));
        let (host, port) = match head.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (head, None),
        };
        spec.set_mssql("server", host);
        if let Some(p) = port.filter(|p| !p.is_empty()) {
            spec.port = Some(p.parse().ok()?);
        }
        for kv in props.split(';').filter(|kv| !kv.trim().is_empty()) {
            let (k, v) = kv.split_once('=')?;
            spec.set_mssql(&k.trim().to_ascii_lowercase(), v);
        }
        return Some(spec);
    }
    let url = parse_url(&format!("postgresql://{rest}"))?;
    let query = rest.split_once('?').map_or("", |(_, q)| q);
    spec.host = url.host;
    spec.port = url.port;
    spec.user = url.user;
    spec.password = url.password;
    spec.database = url.database;
    for kv in query.split('&').filter(|kv| !kv.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        spec.set_mssql(&decode(k)?.to_ascii_lowercase(), &decode(v)?);
    }
    Some(spec)
}

pub fn parse_ado(s: &str) -> Option<ConnSpec> {
    let mut spec = ConnSpec { engine: Engine::SqlServer, ..Default::default() };
    let mut server = false;
    for (k, v) in ado_pairs(s)? {
        server |= matches!(k.as_str(), "server" | "data source" | "address" | "addr" | "network address");
        spec.set_mssql(&k, &v);
    }
    server.then_some(spec)
}

fn ado_pairs(s: &str) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            chars.next();
            if c == '=' {
                break;
            }
            if c == ';' {
                if key.trim().is_empty() {
                    key.clear();
                    continue;
                }
                return None;
            }
            key.push(c);
        }
        if key.trim().is_empty() {
            return Some(out);
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        match chars.peek().copied() {
            Some(q @ ('\'' | '"')) => {
                chars.next();
                loop {
                    match chars.next() {
                        Some(c) if c == q && chars.peek() == Some(&q) => {
                            chars.next();
                            value.push(q);
                        }
                        Some(c) if c == q => break,
                        Some(c) => value.push(c),
                        None => return None,
                    }
                }
                while chars.peek().is_some_and(|&c| c != ';') {
                    chars.next();
                }
                chars.next();
            }
            _ => {
                for c in chars.by_ref() {
                    if c == ';' {
                        break;
                    }
                    value.push(c);
                }
            }
        }
        out.push((key.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
}

pub fn parse_profile(s: &str) -> Option<(ConnectionProfile, Option<String>)> {
    let spec = parse(s)?;
    let spec = match &spec.service {
        Some(name) => {
            let svc = crate::pgservice::lookup(name).unwrap_or_default();
            spec.or(svc)
        }
        None => spec,
    };
    Some(spec.into_profile())
}

pub fn parse_url(s: &str) -> Option<ConnSpec> {
    let (engine, rest) = if let Some(r) = s.strip_prefix("mongodb+srv://").or_else(|| s.strip_prefix("mongodb://")) {
        (Engine::MongoDb, r)
    } else if let Some(r) = s.strip_prefix("rediss://").or_else(|| s.strip_prefix("redis://")) {
        (Engine::Redis, r)
    } else {
        match s.strip_prefix("mysql://").or_else(|| s.strip_prefix("mariadb://")) {
            Some(r) => (Engine::MySql, r),
            None => (Engine::Postgres, s.strip_prefix("postgresql://").or_else(|| s.strip_prefix("postgres://"))?),
        }
    };
    let (userinfo, rest) = match rest.find(['/', '?']).filter(|&i| !rest[..i].contains('@')).and(rest.rfind('@')) {
        Some(at) => match rest[..at].split_once(':') {
            Some((_, pw)) if !pw.split(['/', '?']).next().is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) => (Some(&rest[..at]), &rest[at + 1..]),
            _ => (None, rest),
        },
        None => (None, rest),
    };
    let (main, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = match main.find('/') {
        Some(i) => (&main[..i], &main[i + 1..]),
        None => (main, ""),
    };
    let (auth, hosts) = match (userinfo, authority.rfind('@')) {
        (Some(u), _) => (Some(u), authority),
        (None, Some(i)) => (Some(&authority[..i]), &authority[i + 1..]),
        (None, None) => (None, authority),
    };
    let mut spec = ConnSpec { engine, ..Default::default() };
    if s.starts_with("mongodb+srv://") {
        spec.options.insert("srv".into(), "true".into());
        spec.sslmode = Some(SslMode::VerifyFull);
    }
    if s.starts_with("rediss://") {
        spec.sslmode = Some(SslMode::VerifyFull);
    }
    if let Some(a) = auth {
        let (u, pw) = match a.split_once(':') {
            Some((u, pw)) => (u, Some(pw)),
            None => (a, None),
        };
        if !u.is_empty() {
            spec.user = Some(decode(u)?);
        }
        if let Some(pw) = pw {
            spec.password = Some(decode(pw)?);
        }
    }
    let hostport = hosts.split(',').next().unwrap_or("");
    if engine == Engine::MongoDb && hosts.contains(',') {
        spec.host = Some(decode(hosts)?);
        for kv in query.split('&').filter(|kv| !kv.is_empty()) {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            spec.set_mongo(&decode(k)?, &decode(v)?);
        }
        if !path.is_empty() {
            spec.database = Some(decode(path)?);
        }
        return Some(spec);
    }
    let (host, port) = if let Some(r) = hostport.strip_prefix('[') {
        let (h, after) = r.split_once(']')?;
        (h.to_string(), after.strip_prefix(':'))
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (decode(h)?, Some(p)),
            None => (decode(hostport)?, None),
        }
    };
    if !host.is_empty() {
        spec.host = Some(host);
    }
    if let Some(p) = port.filter(|p| !p.is_empty()) {
        spec.port = Some(p.parse().ok()?);
    }
    if !path.is_empty() {
        spec.database = Some(decode(path)?);
    }
    for kv in query.split('&').filter(|kv| !kv.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        if engine == Engine::MongoDb {
            spec.set_mongo(&decode(k)?, &decode(v)?);
        } else {
            spec.set(&decode(k)?, &decode(v)?);
        }
    }
    Some(spec)
}

pub fn parse_kv(s: &str) -> Option<ConnSpec> {
    const KEYS: [&str; 9] = ["host", "hostaddr", "port", "user", "dbname", "password", "sslmode", "service", "ssl"];
    let mut spec = ConnSpec::default();
    let mut known = 0;
    let mut chars = s.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' || c.is_whitespace() {
                break;
            }
            key.push(c);
            chars.next();
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.next() != Some('=') || key.is_empty() {
            return None;
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut value = String::new();
        if chars.peek() == Some(&'\'') {
            chars.next();
            let mut closed = false;
            while let Some(c) = chars.next() {
                match c {
                    '\\' => value.extend(chars.next()),
                    '\'' => {
                        closed = true;
                        break;
                    }
                    c => value.push(c),
                }
            }
            if !closed {
                return None;
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                chars.next();
                if c == '\\' {
                    value.extend(chars.next());
                } else {
                    value.push(c);
                }
            }
        }
        if KEYS.contains(&key.as_str()) {
            known += 1;
        }
        spec.set(&key, &value);
    }
    (known > 0).then_some(spec)
}

fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> ConnSpec {
        parse(s).unwrap_or_else(|| panic!("{s}"))
    }

    #[test]
    fn urls() {
        let s = url("postgres://bruno:s%40cret@db.prod.internal:6543/app_db?sslmode=require");
        assert_eq!(s.user.as_deref(), Some("bruno"));
        assert_eq!(s.password.as_deref(), Some("s@cret"));
        assert_eq!((s.host.as_deref(), s.port, s.database.as_deref()), (Some("db.prod.internal"), Some(6543), Some("app_db")));
        assert_eq!(s.sslmode, Some(SslMode::Require));

        let s = url("postgresql://localhost");
        assert_eq!((s.host.as_deref(), s.port, s.user, s.password), (Some("localhost"), None, None, None));

        let s = url("postgresql://app:a:b@[::1]:5433,other:5434/db?application_name=x&connect_timeout=3");
        assert_eq!((s.host.as_deref(), s.port, s.password.as_deref()), (Some("::1"), Some(5433), Some("a:b")));

        let s = url("postgresql:///mydb?host=/var/run/postgresql&port=5499&user=u&sslmode=verify-ca");
        assert_eq!((s.host.as_deref(), s.port, s.user.as_deref()), (Some("/var/run/postgresql"), Some(5499), Some("u")));
        assert_eq!((s.database.as_deref(), s.sslmode), (Some("mydb"), Some(SslMode::VerifyFull)));

        let s = url("postgresql://%2Ftmp:5432/db");
        assert_eq!(s.host.as_deref(), Some("/tmp"));

        let s = url("postgres://me@h/d?ssl=true");
        assert_eq!((s.user.as_deref(), s.password, s.sslmode), (Some("me"), None, Some(SslMode::Require)));
        let s = url("postgres://me:p@ss@h/d");
        assert_eq!((s.user.as_deref(), s.password.as_deref(), s.host.as_deref()), (Some("me"), Some("p@ss"), Some("h")));

        let s = url("postgres://me:a/b?c@h:5433/d");
        assert_eq!((s.password.as_deref(), s.host.as_deref(), s.port, s.database.as_deref()), (Some("a/b?c"), Some("h"), Some(5433), Some("d")));
        let s = url("postgres://h:5432/db?application_name=a@b");
        assert_eq!((s.user, s.host.as_deref(), s.database.as_deref()), (None, Some("h"), Some("db")));

        assert!(parse("postgres://h:notaport/db").is_none());
        assert_eq!(url("postgres://h").engine, Engine::Postgres);
    }

    #[test]
    fn mysql_urls() {
        let s = url("mysql://app:p%40ss@db.internal:3307/shop?ssl-mode=REQUIRED");
        assert_eq!((s.engine, s.user.as_deref(), s.password.as_deref()), (Engine::MySql, Some("app"), Some("p@ss")));
        assert_eq!((s.host.as_deref(), s.port, s.database.as_deref(), s.sslmode), (Some("db.internal"), Some(3307), Some("shop"), Some(SslMode::Require)));
        let (p, _) = parse("mariadb://h").unwrap().into_profile();
        assert_eq!((p.engine, p.port, p.user.as_str(), p.database.as_str(), p.name.as_str()), (Engine::MySql, 3306, "root", "", "root@h"));
        assert_eq!(url("mysql://h/x?sslMode=VERIFY_IDENTITY").sslmode, Some(SslMode::VerifyFull));
        let mut q = ConnectionProfile::new("x");
        (q.engine, q.host, q.port, q.user, q.database, q.sslmode) = (Engine::MySql, "db".into(), 3310, "a@b".into(), "shop".into(), SslMode::Disable);
        assert_eq!(q.url(), "mysql://a%40b@db:3310/shop?ssl-mode=DISABLED");
        let (r, _) = parse(&q.url()).unwrap().into_profile();
        assert_eq!((r.engine, r.host, r.port, r.user, r.database, r.sslmode), (q.engine, q.host, q.port, q.user, q.database, q.sslmode));
    }

    #[test]
    fn key_values() {
        assert!(parse("host=db.internal password='it''s' sslmode=disable").is_none());
        let s = url(r"host=db.internal port = 6543 dbname='my db' user=app password='it\'s \\ ok' sslmode=disable");
        assert_eq!((s.host.as_deref(), s.port, s.database.as_deref()), (Some("db.internal"), Some(6543), Some("my db")));
        assert_eq!((s.user.as_deref(), s.password.as_deref(), s.sslmode), (Some("app"), Some(r"it's \ ok"), Some(SslMode::Disable)));
        let s = url("host=/tmp dbname=x");
        assert_eq!(s.host.as_deref(), Some("/tmp"));
        let s = url("service=prod");
        assert_eq!(s.service.as_deref(), Some("prod"));
        assert!(parse("hello world").is_none());
        assert!(parse("a=b").is_none());
        assert!(parse("my-database").is_none());
        assert!(parse("host='unterminated").is_none());
    }

    #[test]
    fn sql_server_strings() {
        let p = |s: &str| parse(s).unwrap_or_else(|| panic!("{s}")).into_profile();
        let (a, pw) = p("Server=tcp:db.example.com,14330;Initial Catalog=shop;User ID=app;Password='p;w';Encrypt=True;TrustServerCertificate=True");
        assert_eq!((a.engine, a.host.as_str(), a.port, a.database.as_str(), a.user.as_str()), (Engine::SqlServer, "db.example.com", 14330, "shop", "app"));
        assert_eq!((a.sslmode, pw.as_deref()), (SslMode::Require, Some("p;w")), "a quoted ; stays in its value");
        let (a, _) = p("Data Source=.\\SQLEXPRESS;Database=x;Encrypt=false");
        assert_eq!((a.host.as_str(), a.port, a.user.as_str(), a.sslmode), ("localhost\\SQLEXPRESS", 1433, "sa", SslMode::Disable));
        let (a, _) = p("server=h;encrypt=yes");
        assert_eq!(a.sslmode, SslMode::VerifyFull, "encrypted and the certificate checked");
        let (a, pw) = p("jdbc:sqlserver://h:1500;databaseName=d;user=u;password=x;encrypt=true;trustServerCertificate=true");
        assert_eq!((a.host.as_str(), a.port, a.database.as_str(), a.user.as_str(), a.sslmode, pw.as_deref()), ("h", 1500, "d", "u", SslMode::Require, Some("x")));
        let (a, pw) = p("sqlserver://u%40x:s3cret@h:1433/d?encrypt=false");
        assert_eq!((a.host.as_str(), a.user.as_str(), a.database.as_str(), a.sslmode, pw.as_deref()), ("h", "u@x", "d", SslMode::Disable, Some("s3cret")));
        assert_eq!(p("sqlserver://h").0.port, 1433);
        let mut q = ConnectionProfile::new("x");
        (q.engine, q.host, q.port, q.user, q.database, q.sslmode) = (Engine::SqlServer, "db".into(), 14330, "sa".into(), "shop".into(), SslMode::Require);
        assert_eq!(q.url(), "sqlserver://db:14330;database=shop;user=sa;encrypt=true;trustServerCertificate=true");
        for mode in SslMode::ALL {
            q.sslmode = mode;
            let (r, _) = p(&q.url());
            assert_eq!((r.engine, r.host.as_str(), r.port, r.user.as_str(), r.database.as_str(), r.sslmode), (q.engine, "db", 14330, "sa", "shop", mode));
        }
        assert_eq!(parse("host=h dbname=d").unwrap().engine, Engine::Postgres);
        assert!(parse("a=1;b=2").is_none());
    }

    #[test]
    fn one_string_per_engine() {
        let cases: [(&str, Engine, &str, u16, &str, &str, SslMode, Option<&str>); 10] = [
            ("postgresql://app:pw@db.internal:6543/shop?sslmode=require", Engine::Postgres, "db.internal", 6543, "app", "shop", SslMode::Require, Some("pw")),
            ("host=db.internal port=6543 dbname=shop user=app password=pw sslmode=disable", Engine::Postgres, "db.internal", 6543, "app", "shop", SslMode::Disable, Some("pw")),
            ("jdbc:postgresql://db.internal/shop?user=app&password=pw&ssl=true", Engine::Postgres, "db.internal", 5432, "app", "shop", SslMode::Require, Some("pw")),
            ("mysql://app:pw@db.internal:3307/shop?ssl-mode=DISABLED", Engine::MySql, "db.internal", 3307, "app", "shop", SslMode::Disable, Some("pw")),
            ("mariadb://app@db.internal/shop", Engine::MySql, "db.internal", 3306, "app", "shop", SslMode::Prefer, None),
            ("jdbc:mysql://db.internal:3307/shop?user=app&password=pw&useSSL=false", Engine::MySql, "db.internal", 3307, "app", "shop", SslMode::Disable, Some("pw")),
            ("sqlserver://app:pw@db.internal:14330/shop?encrypt=false", Engine::SqlServer, "db.internal", 14330, "app", "shop", SslMode::Disable, Some("pw")),
            ("jdbc:sqlserver://db.internal:14330;databaseName=shop;user=app;password=pw;encrypt=true", Engine::SqlServer, "db.internal", 14330, "app", "shop", SslMode::VerifyFull, Some("pw")),
            ("Server=tcp:db.internal,14330;Database=shop;User Id=app;Password=pw;TrustServerCertificate=True", Engine::SqlServer, "db.internal", 14330, "app", "shop", SslMode::Require, Some("pw")),
            ("  Server=db.internal;Initial Catalog=shop;UID=app  ", Engine::SqlServer, "db.internal", 1433, "app", "shop", SslMode::Prefer, None),
        ];
        for (s, engine, host, port, user, db, ssl, pw) in cases {
            let (p, got_pw) = parse_profile(s).unwrap_or_else(|| panic!("{s}"));
            assert_eq!((p.engine, p.host.as_str(), p.port, p.user.as_str(), p.database.as_str(), p.sslmode), (engine, host, port, user, db, ssl), "{s}");
            assert_eq!(got_pw.as_deref(), pw, "{s}");
            let saved = toml::to_string(&p).unwrap();
            assert!(!saved.contains("pw"), "no password in what is saved: {saved}");
        }
        for junk in ["", "postgres", "db.internal", "mysql:/x", "Server", "a=1;b=2"] {
            assert!(parse_profile(junk).is_none(), "{junk:?} is not a connection string");
        }
    }

    #[test]
    fn profiles_get_libpq_defaults() {
        let (p, pw) = parse("postgresql://u:pw@h").unwrap().into_profile();
        assert_eq!((p.host.as_str(), p.port, p.user.as_str(), p.database.as_str()), ("h", 5432, "u", "u"));
        assert_eq!((pw.as_deref(), p.name.as_str()), (Some("pw"), "u@h"));
        let (p, _) = parse("host=/tmp port=5433").unwrap().into_profile();
        assert!(p.is_socket());
        assert_eq!(p.url(), format!("postgresql://{}@%2Ftmp:5433/{}?sslmode=prefer", p.user, p.database));
        let mut q = ConnectionProfile::new("x");
        (q.host, q.port, q.user, q.database, q.sslmode) = ("::1".into(), 6000, "a@b".into(), "d b".into(), SslMode::VerifyFull);
        let (r, pw) = parse(&q.url()).unwrap().into_profile();
        assert_eq!((r.host, r.port, r.user, r.database, r.sslmode, pw), (q.host, q.port, q.user, q.database, q.sslmode, None));
    }

    #[test]
    fn document_engine_urls() {
        let (p, pw) = parse_profile("mongodb://app:s%3Fcret@db.example:27018/shop?authSource=admin&replicaSet=rs0&tls=true").unwrap();
        assert_eq!((p.engine, p.host.as_str(), p.port, p.user.as_str(), p.database.as_str()), (Engine::MongoDb, "db.example", 27018, "app", "shop"));
        assert_eq!(pw.as_deref(), Some("s?cret"));
        assert_eq!(p.sslmode, SslMode::VerifyFull);
        assert_eq!((p.options.get("authSource").map(String::as_str), p.options.get("replicaSet").map(String::as_str)), (Some("admin"), Some("rs0")));
        assert_eq!(p.url(), "mongodb://app@db.example:27018/shop?authSource=admin&replicaSet=rs0&tls=true");
        let (p, _) = parse_profile("mongodb+srv://u@cluster0.ab1.mongodb.net/?retryWrites=true").unwrap();
        assert_eq!((p.host.as_str(), p.sslmode, p.options.get("srv").map(String::as_str)), ("cluster0.ab1.mongodb.net", SslMode::VerifyFull, Some("true")));
        assert_eq!(p.url(), "mongodb+srv://u@cluster0.ab1.mongodb.net/");
        let (p, _) = parse_profile("mongodb://a:27017,b:27017/app").unwrap();
        assert_eq!((p.host.as_str(), p.database.as_str()), ("a:27017,b:27017", "app"));
        let (p, pw) = parse_profile("rediss://:pw@cache:6380/2").unwrap();
        assert_eq!((p.engine, p.host.as_str(), p.port, p.database.as_str(), p.sslmode), (Engine::Redis, "cache", 6380, "2", SslMode::VerifyFull));
        assert_eq!(pw.as_deref(), Some("pw"));
        assert_eq!(p.url(), "rediss://cache:6380/2");
    }
}
