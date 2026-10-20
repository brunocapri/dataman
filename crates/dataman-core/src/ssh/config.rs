use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostConfig {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>,
    pub identities_only: Option<bool>,
    pub proxy_jump: Option<String>,
}

impl HostConfig {
    pub fn real_host(&self, alias: &str) -> String {
        match &self.hostname {
            Some(h) => h.replace("%h", alias).replace("%%", "%"),
            None => alias.to_string(),
        }
    }

    pub fn identity_paths(&self, home: &Path, local_user: &str, remote_user: &str, host: &str, port: u16) -> Vec<PathBuf> {
        self.identity_files.iter().map(|f| PathBuf::from(expand(f, home, local_user, remote_user, host, port))).collect()
    }

    pub fn jumps(&self) -> Vec<Jump> {
        match self.proxy_jump.as_deref() {
            None => vec![],
            Some(v) if v.eq_ignore_ascii_case("none") => vec![],
            Some(v) => v.split(',').filter_map(Jump::parse).collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Jump {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

impl Jump {
    pub fn parse(s: &str) -> Option<Jump> {
        let s = s.trim();
        let s = s.strip_prefix("ssh://").unwrap_or(s);
        let (user, hostport) = match s.rsplit_once('@') {
            Some((u, h)) => (Some(u.to_string()), h),
            None => (None, s),
        };
        let (host, port) = if let Some(r) = hostport.strip_prefix('[') {
            let (h, after) = r.split_once(']')?;
            (h.to_string(), after.strip_prefix(':').and_then(|p| p.parse().ok()))
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p.parse().ok()?)),
                None => (hostport.to_string(), None),
            }
        };
        (!host.is_empty()).then_some(Jump { user, host, port })
    }
}

fn expand(s: &str, home: &Path, local_user: &str, remote_user: &str, host: &str, port: u16) -> String {
    let home_s = home.to_string_lossy();
    let s = match s.strip_prefix("~/") {
        Some(rest) => format!("{home_s}/{rest}"),
        None if s == "~" => home_s.to_string(),
        None => s.to_string(),
    };
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('d') => out.push_str(&home_s),
            Some('u') => out.push_str(local_user),
            Some('r') => out.push_str(remote_user),
            Some('h') => out.push_str(host),
            Some('p') => out.push_str(&port.to_string()),
            Some('%') => out.push('%'),
            Some(o) => {
                out.push('%');
                out.push(o);
            }
            None => out.push('%'),
        }
    }
    out
}

pub fn resolve_default(alias: &str) -> HostConfig {
    match home() {
        Some(h) => resolve_file(&h.join(".ssh").join("config"), alias, &h.join(".ssh")),
        None => HostConfig::default(),
    }
}

pub(crate) fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

pub fn resolve_file(path: &Path, alias: &str, ssh_dir: &Path) -> HostConfig {
    let mut out = HostConfig::default();
    if let Ok(text) = std::fs::read_to_string(path) {
        apply(&text, alias, ssh_dir, &mut out, 0);
    }
    out
}

pub fn resolve(text: &str, alias: &str, ssh_dir: &Path) -> HostConfig {
    let mut out = HostConfig::default();
    apply(text, alias, ssh_dir, &mut out, 0);
    out
}

fn apply(text: &str, alias: &str, ssh_dir: &Path, out: &mut HostConfig, depth: usize) {
    let mut active = true;
    for line in text.lines() {
        let Some((key, args)) = split_line(line) else { continue };
        match key.as_str() {
            "host" => active = host_matches(&args, alias),
            "match" => active = args.len() == 1 && args[0].eq_ignore_ascii_case("all"),
            _ if !active => {}
            "include" if depth < 8 => {
                for pat in &args {
                    for f in include_files(pat, ssh_dir) {
                        if let Ok(t) = std::fs::read_to_string(&f) {
                            apply(&t, alias, ssh_dir, out, depth + 1);
                        }
                    }
                }
            }
            "hostname" => set(&mut out.hostname, &args),
            "user" => set(&mut out.user, &args),
            "port" => {
                if out.port.is_none() {
                    out.port = args.first().and_then(|p| p.parse().ok());
                }
            }
            "identityfile" => out.identity_files.extend(args.first().cloned()),
            "identitiesonly" => {
                if out.identities_only.is_none() {
                    out.identities_only = args.first().map(|v| v.eq_ignore_ascii_case("yes"));
                }
            }
            "proxyjump" => set(&mut out.proxy_jump, &args),
            _ => {}
        }
    }
}

fn set(slot: &mut Option<String>, args: &[String]) {
    if slot.is_none() {
        *slot = args.first().cloned();
    }
}

fn split_line(line: &str) -> Option<(String, Vec<String>)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(line.len());
    let key = line[..end].to_ascii_lowercase();
    let rest = line[end..].trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut has = false;
    for c in rest.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                has = true;
            }
            c if c.is_whitespace() && !quoted => {
                if has {
                    args.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        args.push(cur);
    }
    Some((key, args))
}

fn host_matches(patterns: &[String], alias: &str) -> bool {
    let mut hit = false;
    for p in patterns {
        if let Some(neg) = p.strip_prefix('!') {
            if wildcard(neg, alias) {
                return false;
            }
        } else if wildcard(p, alias) {
            hit = true;
        }
    }
    hit
}

pub(crate) fn wildcard(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.to_lowercase().chars().collect();
    let s: Vec<char> = s.to_lowercase().chars().collect();
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn include_files(pat: &str, ssh_dir: &Path) -> Vec<PathBuf> {
    let expanded = match (pat.strip_prefix("~/"), home()) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(pat),
    };
    let full = if expanded.is_absolute() { expanded } else { ssh_dir.join(expanded) };
    let name = full.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![full];
    }
    let Some(dir) = full.parent() else { return vec![] };
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| wildcard(&name, &n.to_string_lossy()))).collect())
        .unwrap_or_default();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
# global defaults come first only when written first
Host bastion
    HostName 203.0.113.10
    User jump
    Port 2222
    IdentityFile ~/.ssh/bastion_ed25519

Host db-* !db-legacy
    HostName %h.internal
    User=deploy
    ProxyJump bastion
    IdentitiesOnly yes
    IdentityFile "~/.ssh/my keys/%r@%h"

Match exec "false"
    User nobody

Host *
    User fallback
    IdentityFile ~/.ssh/id_ed25519
    Port 22
"#;

    #[test]
    fn alias_resolution() {
        let dir = Path::new("/home/me/.ssh");
        let b = resolve(CONFIG, "bastion", dir);
        assert_eq!((b.hostname.as_deref(), b.user.as_deref(), b.port), (Some("203.0.113.10"), Some("jump"), Some(2222)));
        assert_eq!(b.identity_files, vec!["~/.ssh/bastion_ed25519", "~/.ssh/id_ed25519"]);
        assert!(b.jumps().is_empty());

        let d = resolve(CONFIG, "db-main", dir);
        assert_eq!((d.user.as_deref(), d.port, d.identities_only), (Some("deploy"), Some(22), Some(true)));
        assert_eq!((d.real_host("db-main"), b.real_host("bastion")), ("db-main.internal".into(), "203.0.113.10".into()));
        assert_eq!(d.jumps(), vec![Jump { user: None, host: "bastion".into(), port: None }]);
        let home = Path::new("/home/me");
        assert_eq!(
            d.identity_paths(home, "me", "deploy", "db-main", 22),
            vec![PathBuf::from("/home/me/.ssh/my keys/deploy@db-main"), PathBuf::from("/home/me/.ssh/id_ed25519")]
        );

        let l = resolve(CONFIG, "db-legacy", dir);
        assert_eq!((l.hostname, l.user.as_deref(), l.proxy_jump), (None, Some("fallback"), None));
        assert_eq!(resolve(CONFIG, "BASTION", dir).port, Some(2222));
        let u = resolve(CONFIG, "example.com", dir);
        assert_eq!((u.hostname, u.port), (None, Some(22)));
    }

    #[test]
    fn jumps_and_includes() {
        assert_eq!(
            HostConfig { proxy_jump: Some("alice@j1:2200,ssh://j2,[::1]:23".into()), ..Default::default() }.jumps(),
            vec![
                Jump { user: Some("alice".into()), host: "j1".into(), port: Some(2200) },
                Jump { user: None, host: "j2".into(), port: None },
                Jump { user: None, host: "::1".into(), port: Some(23) },
            ]
        );
        assert!(HostConfig { proxy_jump: Some("none".into()), ..Default::default() }.jumps().is_empty());

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("conf.d")).unwrap();
        std::fs::write(dir.path().join("conf.d/10-work"), "Host work\n  HostName work.example\n  Port 2022\n").unwrap();
        std::fs::write(dir.path().join("conf.d/20-other"), "Host work\n  HostName ignored.example\n").unwrap();
        std::fs::write(dir.path().join("config"), "Include conf.d/*\nHost work\n  User me\n").unwrap();
        let w = resolve_file(&dir.path().join("config"), "work", dir.path());
        assert_eq!((w.hostname.as_deref(), w.port, w.user.as_deref()), (Some("work.example"), Some(2022), Some("me")));
        assert!(wildcard("10.0.*.?", "10.0.12.7") && !wildcard("10.0.*.?", "10.0.12.70"));
    }
}
