use std::path::PathBuf;

use crate::connstr::ConnSpec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    pub spec: ConnSpec,
}

pub fn files() -> Vec<PathBuf> {
    let mut v = Vec::new();
    match std::env::var_os("PGSERVICEFILE").filter(|p| !p.is_empty()) {
        Some(p) => v.push(PathBuf::from(p)),
        None => {
            if let Some(h) = std::env::var_os("HOME") {
                v.push(PathBuf::from(h).join(".pg_service.conf"));
            }
        }
    }
    if let Some(d) = std::env::var_os("PGSYSCONFDIR").filter(|p| !p.is_empty()) {
        v.push(PathBuf::from(d).join("pg_service.conf"));
    }
    v
}

pub fn all() -> Vec<Service> {
    let mut out: Vec<Service> = Vec::new();
    for f in files() {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        for s in parse(&text) {
            if !out.iter().any(|o| o.name == s.name) {
                out.push(s);
            }
        }
    }
    out
}

pub fn lookup(name: &str) -> Option<ConnSpec> {
    all().into_iter().find(|s| s.name == name).map(|s| s.spec)
}

pub fn parse(text: &str) -> Vec<Service> {
    let mut out: Vec<Service> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            out.push(Service { name: name.trim().to_string(), spec: ConnSpec::default() });
            continue;
        }
        let (Some(cur), Some((k, v))) = (out.last_mut(), line.split_once('=')) else { continue };
        cur.spec.set(k.trim(), v.trim());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SslMode;

    #[test]
    fn sections() {
        let s = parse(
            "# services\n[prod]\nhost=db.prod.internal\nport = 6543\ndbname=app\nuser=bruno\nsslmode=verify-full\n\n[local]\nhost=/tmp\n# a comment\ndbname=dev\nstray line\n",
        );
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].name, "prod");
        assert_eq!((s[0].spec.host.as_deref(), s[0].spec.port, s[0].spec.database.as_deref()), (Some("db.prod.internal"), Some(6543), Some("app")));
        assert_eq!((s[0].spec.user.as_deref(), s[0].spec.sslmode), (Some("bruno"), Some(SslMode::VerifyFull)));
        let (p, pw) = s[1].spec.clone().into_profile();
        assert_eq!((p.host.as_str(), p.port, p.database.as_str(), pw), ("/tmp", 5432, "dev", None));
        let spec = crate::connstr::parse("service=prod dbname=other").unwrap().or(s[0].spec.clone());
        assert_eq!((spec.database.as_deref(), spec.host.as_deref()), (Some("other"), Some("db.prod.internal")));
    }
}
