use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub host: String,
    pub port: String,
    pub database: String,
    pub user: String,
    pub password: String,
}

impl Entry {
    fn matches(&self, host: &str, port: u16, database: &str, user: &str) -> bool {
        let field = |pat: &str, v: &str| pat == "*" || pat == v;
        // A socket in the default directory is "localhost" to libpq; any socket also matches its
        // own path.
        let host_ok = field(&self.host, host) || (is_default_socket(host) && self.host == "localhost");
        host_ok && field(&self.port, &port.to_string()) && field(&self.database, database) && field(&self.user, user)
    }
}

fn is_default_socket(host: &str) -> bool {
    host.is_empty() || matches!(host, "/tmp" | "/var/run/postgresql" | "/run/postgresql" | "/private/tmp")
}

pub fn default_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PGPASSFILE").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pgpass"))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Skip {
    Missing,
    NotAFile,
    /// Group or others can read it (libpq wants 0600 or less).
    Permissions(u32),
}

pub fn read(path: &Path) -> Result<Vec<Entry>, Skip> {
    let meta = std::fs::metadata(path).map_err(|_| Skip::Missing)?;
    if !meta.is_file() {
        return Err(Skip::NotAFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            log::warn!("{}: group or world access, ignored (permissions should be 0600 or less)", path.display());
            return Err(Skip::Permissions(mode & 0o777));
        }
    }
    let text = std::fs::read_to_string(path).map_err(|_| Skip::Missing)?;
    Ok(parse(&text))
}

pub fn lookup(path: &Path, host: &str, port: u16, database: &str, user: &str) -> Option<String> {
    let entries = read(path).ok()?;
    find(&entries, host, port, database, user)
}

pub fn find(entries: &[Entry], host: &str, port: u16, database: &str, user: &str) -> Option<String> {
    entries.iter().find(|e| e.matches(host, port, database, user)).map(|e| e.password.clone()).filter(|p| !p.is_empty())
}

pub fn parse(text: &str) -> Vec<Entry> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Entry> {
    let line = line.trim_end_matches('\r');
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut fields: Vec<String> = Vec::with_capacity(5);
    let mut cur = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            // The password is the rest of the line: its colons need no escape.
            ':' if fields.len() < 4 => fields.push(std::mem::take(&mut cur)),
            '\\' => cur.extend(chars.next()),
            c => cur.push(c),
        }
    }
    fields.push(cur);
    let [host, port, database, user, password]: [String; 5] = fields.try_into().ok()?;
    Some(Entry { host, port, database, user, password })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "\
# comment
db.prod:5432:app:bruno:first
db.prod:*:*:bruno:second
*:5432:*:postgres:pg\\:pass\\\\word
localhost:5433:*:*:local
/srv/pg:5432:*:*:socket
short:line
host\\:with\\:colons:5432:d:u:x:y:z\r
";

    #[test]
    fn libpq_matching() {
        let e = parse(FILE);
        assert_eq!(e.len(), 6, "{e:?}");
        assert_eq!(find(&e, "db.prod", 5432, "app", "bruno").as_deref(), Some("first"));
        assert_eq!(find(&e, "db.prod", 6543, "other", "bruno").as_deref(), Some("second"));
        assert_eq!(find(&e, "anything", 5432, "x", "postgres").as_deref(), Some(r"pg:pass\word"));
        assert_eq!(find(&e, "anything", 5433, "x", "postgres"), None);
        assert_eq!(find(&e, "/tmp", 5433, "x", "me").as_deref(), Some("local"));
        assert_eq!(find(&e, "/srv/pg", 5432, "x", "me").as_deref(), Some("socket"));
        assert_eq!(find(&e, "host:with:colons", 5432, "d", "u").as_deref(), Some("x:y:z"));
        assert_eq!(find(&e, "nowhere", 1, "d", "u"), None);
    }

    #[test]
    #[cfg(unix)]
    fn permissions_are_checked() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pgpass");
        std::fs::write(&path, "h:5432:d:u:secret\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read(&path), Err(Skip::Permissions(0o644)));
        assert_eq!(lookup(&path, "h", 5432, "d", "u"), None);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(lookup(&path, "h", 5432, "d", "u").as_deref(), Some("secret"));
        assert_eq!(read(&dir.path().join("missing")), Err(Skip::Missing));
        assert_eq!(read(dir.path()), Err(Skip::NotAFile));
    }
}
