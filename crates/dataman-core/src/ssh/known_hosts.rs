//! `@revoked` keys are refused; `@cert-authority` lines are skipped (host certificates are not
//! supported).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::PublicKey;
use sha1::Sha1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Known,
    Unknown,
    /// The host is recorded with another key of the same type: refuse.
    Changed { path: PathBuf, line: usize },
    Revoked { path: PathBuf, line: usize },
}

pub fn host_name(host: &str, port: u16) -> String {
    if port == 22 { host.to_string() } else { format!("[{host}]:{port}") }
}

pub fn default_files(app_file: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(h) = super::config::home() {
        v.push(h.join(".ssh/known_hosts"));
        v.push(h.join(".ssh/known_hosts2"));
    }
    v.push(app_file.to_path_buf());
    v
}

fn wire(key: &PublicKey) -> (String, Vec<u8>) {
    (key.algorithm().as_str().to_string(), key.to_bytes().unwrap_or_default())
}

pub fn check(files: &[PathBuf], host: &str, port: u16, key: &PublicKey) -> Status {
    let (alg, blob) = wire(key);
    let mut known = false;
    let mut changed = None;
    for f in files {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        match check_text(&text, host, port, &alg, &blob) {
            Verdict { revoked: Some(line), .. } => return Status::Revoked { path: f.clone(), line },
            Verdict { known: true, .. } => known = true,
            Verdict { changed: Some(line), .. } => {
                changed.get_or_insert((f.clone(), line));
            }
            _ => {}
        }
    }
    // As in ssh: a matching key anywhere is enough, unless it is revoked.
    match (known, changed) {
        (true, _) => Status::Known,
        (false, Some((path, line))) => Status::Changed { path, line },
        (false, None) => Status::Unknown,
    }
}

pub fn known_types(files: &[PathBuf], host: &str, port: u16) -> Vec<String> {
    let name = host_name(host, port);
    let mut v: Vec<String> = Vec::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        for line in text.lines() {
            if let Some(l) = parse_line(line)
                && l.marker.is_none()
                && patterns_match(l.patterns, &name)
                && !v.iter().any(|t| t == l.keytype)
            {
                v.push(l.keytype.to_string());
            }
        }
    }
    v
}

#[derive(Default)]
struct Verdict {
    known: bool,
    changed: Option<usize>,
    revoked: Option<usize>,
}

struct Line<'a> {
    marker: Option<&'a str>,
    patterns: &'a str,
    keytype: &'a str,
    blob: &'a str,
}

fn parse_line(line: &str) -> Option<Line<'_>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut it = line.split_whitespace();
    let mut first = it.next()?;
    let marker = if first.starts_with('@') {
        let m = first;
        first = it.next()?;
        Some(m)
    } else {
        None
    };
    Some(Line { marker, patterns: first, keytype: it.next()?, blob: it.next()? })
}

fn check_text(text: &str, host: &str, port: u16, alg: &str, blob: &[u8]) -> Verdict {
    let name = host_name(host, port);
    let mut v = Verdict::default();
    for (i, line) in text.lines().enumerate() {
        let Some(l) = parse_line(line) else { continue };
        if !patterns_match(l.patterns, &name) {
            continue;
        }
        let same = l.keytype == alg && BASE64.decode(l.blob.as_bytes()).is_ok_and(|b| b == blob);
        match l.marker {
            Some("@revoked") if same => v.revoked = Some(i + 1),
            Some(_) => {}
            None if same => v.known = true,
            None if l.keytype == alg => {
                v.changed.get_or_insert(i + 1);
            }
            None => {}
        }
    }
    v
}

fn patterns_match(list: &str, name: &str) -> bool {
    let mut hit = false;
    for p in list.split(',') {
        if let Some(neg) = p.strip_prefix('!') {
            if super::config::wildcard(neg, name) {
                return false;
            }
        } else if let Some(hashed) = p.strip_prefix("|1|") {
            if hashed_match(hashed, name) {
                hit = true;
            }
        } else if super::config::wildcard(p, name) {
            hit = true;
        }
    }
    hit
}

fn hashed_match(s: &str, name: &str) -> bool {
    let Some((salt, hash)) = s.split_once('|') else { return false };
    let (Ok(salt), Ok(hash)) = (BASE64.decode(salt.as_bytes()), BASE64.decode(hash.as_bytes())) else { return false };
    let Ok(mut mac) = Hmac::<Sha1>::new_from_slice(&salt) else { return false };
    mac.update(name.as_bytes());
    mac.verify_slice(&hash).is_ok()
}

pub fn line_for(host: &str, port: u16, key: &PublicKey) -> String {
    let (alg, blob) = wire(key);
    format!("{} {alg} {}", host_name(host, port), BASE64.encode(&blob))
}

/// Created 0600 (with its directory) when missing.
pub fn append(path: &Path, host: &str, port: u16, key: &PublicKey) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let existing = std::fs::read(path).unwrap_or_default();
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts.open(path)?;
    let sep = if existing.is_empty() || existing.ends_with(b"\n") { "" } else { "\n" };
    writeln!(f, "{sep}{}", line_for(host, port, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const ED_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";

    fn key(b64: &str) -> PublicKey {
        russh::keys::parse_public_key_base64(b64).unwrap()
    }

    fn hashed(name: &str, salt: &[u8]) -> String {
        let mut mac = Hmac::<Sha1>::new_from_slice(salt).unwrap();
        mac.update(name.as_bytes());
        format!("|1|{}|{}", BASE64.encode(salt), BASE64.encode(&mac.finalize().into_bytes()))
    }

    #[test]
    fn verification() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("known_hosts");
        let text = format!(
            "# comment\n\
             bastion.example,203.0.113.10 ssh-ed25519 {ED_A} comment here\n\
             [db.example]:2222\tssh-ed25519   {ED_A}\n\
             {} ssh-ed25519 {ED_B}\n\
             *.wild.example,!bad.wild.example ssh-ed25519 {ED_A}\n\
             @cert-authority *.example ssh-ed25519 {ED_B}\n\
             @revoked revoked.example ssh-ed25519 {ED_A}\n\
             revoked.example ssh-ed25519 {ED_A}\n\
             garbage line\n",
            hashed("[hidden.example]:2200", b"0123456789abcdefghij"),
        );
        std::fs::write(&f, text).unwrap();
        let files = vec![dir.path().join("missing"), f.clone()];
        let (a, b) = (key(ED_A), key(ED_B));
        assert_eq!(check(&files, "bastion.example", 22, &a), Status::Known);
        assert_eq!(check(&files, "203.0.113.10", 22, &a), Status::Known);
        assert_eq!(check(&files, "bastion.example", 22, &b), Status::Changed { path: f.clone(), line: 2 });
        assert_eq!(check(&files, "db.example", 2222, &a), Status::Known);
        assert_eq!(check(&files, "db.example", 22, &a), Status::Unknown);
        assert_eq!(check(&files, "hidden.example", 2200, &b), Status::Known);
        assert_eq!(check(&files, "hidden.example", 2200, &a), Status::Changed { path: f.clone(), line: 4 });
        assert_eq!(check(&files, "x.wild.example", 22, &a), Status::Known);
        assert_eq!(check(&files, "bad.wild.example", 22, &a), Status::Unknown);
        assert_eq!(check(&files, "other.example", 22, &b), Status::Unknown);
        assert_eq!(check(&files, "revoked.example", 22, &a), Status::Revoked { path: f.clone(), line: 7 });
        assert_eq!(known_types(&files, "db.example", 2222), vec!["ssh-ed25519"]);

        let app = dir.path().join("dataman/known_hosts");
        let files = vec![f.clone(), app.clone()];
        assert_eq!(check(&files, "new.example", 2022, &b), Status::Unknown);
        append(&app, "new.example", 2022, &b).unwrap();
        assert_eq!(check(&files, "new.example", 2022, &b), Status::Known);
        assert_eq!(std::fs::read_to_string(&app).unwrap(), format!("[new.example]:2022 ssh-ed25519 {ED_B}\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&app).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
