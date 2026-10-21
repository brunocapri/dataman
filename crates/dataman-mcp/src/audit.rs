//! Never a password or a bound parameter value. The file is 0600.

use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use dataman_core::sql::{TokenKind, tokenize};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    Invalid,
    Refused,
    Error,
    Cancelled,
    Timeout,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Invalid => "invalid",
            Outcome::Refused => "refused",
            Outcome::Error => "error",
            Outcome::Cancelled => "cancelled",
            Outcome::Timeout => "timeout",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Ties the entry to the reply the app keeps in memory while it is open (never written).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub ts: chrono::DateTime<chrono::Utc>,
    pub client: String,
    pub transport: String,
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    pub ms: f64,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalNote>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draft: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorite: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_by: Option<AllowedBy>,
    /// The statement as it ran, when visibility rewrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewritten: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<Seen>,
}

/// What the agent received, past the privacy rules.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Seen {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<u64>,
    /// Sensitive columns masked, with their mask.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub masked: Vec<(String, dataman_core::privacy::Mask)>,
    /// Values of sensitive data types masked inside the reply, by type.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub values: std::collections::BTreeMap<String, u64>,
}

impl Seen {
    pub fn is_empty(&self) -> bool {
        *self == Seen::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "lowercase")]
pub enum AllowedBy {
    Allowlist { entries: Vec<String> },
    Yolo,
}

impl AllowedBy {
    pub fn label(&self) -> &'static str {
        match self {
            AllowedBy::Allowlist { .. } => "allowlist",
            AllowedBy::Yolo => "YOLO",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Approved,
    Denied,
    Timeout,
    Unavailable,
}

impl Decision {
    pub fn label(self) -> &'static str {
        match self {
            Decision::Approved => "approved",
            Decision::Denied => "denied",
            Decision::Timeout => "timeout",
            Decision::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalNote {
    pub decision: Decision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
}

pub fn local_user() -> String {
    std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "unknown".into())
}

/// Comments go too: they can hold anything.
pub fn redact(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    for t in tokenize(sql) {
        match t.kind {
            TokenKind::String | TokenKind::DollarString | TokenKind::Number => out.push('?'),
            TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment => {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            _ => out.push_str(t.text(sql)),
        }
    }
    let s = out.trim();
    match s.char_indices().nth(4000) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s.to_string(),
    }
}

const QUERY_MAX: usize = 256 * 1024;

pub fn full(sql: &str) -> Option<String> {
    (sql.len() <= QUERY_MAX).then(|| sql.to_string())
}

const ROTATE_AT: u64 = 8 * 1024 * 1024;

pub struct Audit {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Entry {
    pub fn statement(&self) -> Option<&str> {
        self.query.as_deref().or(self.sql.as_deref()).filter(|s| !s.trim().is_empty())
    }
}

impl Audit {
    pub fn new(path: PathBuf) -> Audit {
        Audit { path, lock: Mutex::new(()) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Errors go to the log: an audit failure never fails a call.
    pub fn write(&self, e: &Entry) {
        let _g = self.lock.lock();
        if let Err(err) = self.append(e) {
            log::warn!("audit log {}: {err}", self.path.display());
        }
    }

    fn append(&self, e: &Entry) -> std::io::Result<()> {
        append_line(&self.path, &serde_json::to_string(e).map_err(std::io::Error::other)?)
    }

    /// What the agent received, as it left Dataman (after the privacy rules), next to the log.
    pub fn write_reply(&self, id: &str, text: &str) {
        let mut at = text.len().min(REPLY_MAX);
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        let r = Reply { id: id.to_string(), ts: chrono::Utc::now(), cut: at < text.len(), reply: text[..at].to_string() };
        let _g = self.lock.lock();
        let path = replies_path(&self.path);
        if let Err(err) = serde_json::to_string(&r).map_err(std::io::Error::other).and_then(|l| append_line(&path, &l)) {
            log::warn!("audit replies {}: {err}", path.display());
        }
    }
}

/// One line, appended in a single write; past 8 MB the file moves to `.1.jsonl` (the one before
/// goes), so a log and its rotated copy stay under 16 MB. 0600: the replies hold data.
fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > ROTATE_AT) {
        let _ = std::fs::rename(path, path.with_extension("1.jsonl"));
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    f.write_all(format!("{line}\n").as_bytes())
}

/// A reply is kept up to 64 KB; `cut` says when it was longer.
pub const REPLY_MAX: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub id: String,
    pub ts: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cut: bool,
    pub reply: String,
}

pub fn replies_path(audit: &Path) -> PathBuf {
    audit.with_file_name("mcp-replies.jsonl")
}

/// The reply logged for a call (by its entry's id), in the replies file or its rotated copy;
/// None once it rotated out.
pub fn reply(audit: &Path, id: &str) -> Option<Reply> {
    let current = replies_path(audit);
    let prefix = format!("{{\"id\":{}", serde_json::to_string(id).ok()?);
    for p in [current.clone(), current.with_extension("1.jsonl")] {
        let Ok(f) = std::fs::File::open(&p) else { continue };
        let found = std::io::BufReader::new(f).lines().map_while(Result::ok).filter(|l| l.starts_with(&prefix)).last();
        if let Some(r) = found.and_then(|l| serde_json::from_str(&l).ok()) {
            return Some(r);
        }
    }
    None
}

pub fn tail(path: &Path, n: usize) -> Vec<Entry> {
    let Ok(mut f) = std::fs::File::open(path) else { return Vec::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(1024 * 1024);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let mut lines: Vec<Entry> = buf
        .lines()
        .skip(if start > 0 { 1 } else { 0 })
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect();
    let drop = lines.len().saturating_sub(n);
    lines.drain(..drop);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction() {
        assert_eq!(redact("SELECT * FROM t WHERE email = 'a@b.c' AND id > 42"), "SELECT * FROM t WHERE email = ? AND id > ?");
        assert_eq!(redact("insert into t values ($1, E'x\\'y', $$secret$$)  -- pw=1\n"), "insert into t values ($1, ?, ?)");
        assert_eq!(redact("select \"Weird Name\", 1.5e3 /* note */ from s.t"), "select \"Weird Name\", ? from s.t");
    }

    #[test]
    fn lines_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let a = Audit::new(dir.path().join("mcp-audit.jsonl"));
        for i in 0..5 {
            a.write(&Entry {
                id: None,
                ts: chrono::Utc::now(),
                client: "claude-code".into(),
                transport: "stdio".into(),
                tool: format!("t{i}"),
                connection: Some("local".into()),
                database: None,
                sql: Some(redact("SELECT 'x'")),
                query: (i == 4).then(|| "SELECT 'x'".into()),
                rows: Some(i),
                ms: 1.5,
                outcome: if i == 4 { Outcome::Refused } else { Outcome::Ok },
                error: (i == 4).then(|| "no".into()),
                approval: (i == 4).then(|| ApprovalNote {
                    decision: Decision::Denied,
                    by: Some("bruno".into()),
                    reasons: vec!["DELETE without WHERE".into()],
                    rows: Some(1204),
                }),
                draft: i == 3,
                favorite: None,
                allowed_by: match i {
                    1 => Some(AllowedBy::Allowlist { entries: vec![redact("TRUNCATE audit_tmp")] }),
                    2 => Some(AllowedBy::Yolo),
                    _ => None,
                },
                rewritten: None,
                seen: None,
            });
        }
        let text = std::fs::read_to_string(a.path()).unwrap();
        assert_eq!(text.lines().count(), 5);
        let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first["tool"], "t0");
        assert_eq!(first["sql"], "SELECT ?");
        assert_eq!(first["outcome"], "ok");
        assert!(
            first.get("database").is_none() && first.get("error").is_none() && first.get("approval").is_none() && first.get("query").is_none() && first.get("draft").is_none(),
            "absent fields stay out: {first}"
        );
        assert!(first.get("allowed_by").is_none(), "{first}");
        let line = |i: usize| serde_json::from_str::<serde_json::Value>(text.lines().nth(i).unwrap()).unwrap();
        assert_eq!(line(1)["allowed_by"], serde_json::json!({"by": "allowlist", "entries": ["TRUNCATE audit_tmp"]}));
        assert_eq!(line(2)["allowed_by"], serde_json::json!({"by": "yolo"}));
        assert_eq!(tail(a.path(), 3)[0].allowed_by, Some(AllowedBy::Yolo));
        let draft: serde_json::Value = serde_json::from_str(text.lines().nth(3).unwrap()).unwrap();
        assert_eq!(draft["draft"], true);
        let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(last["approval"], serde_json::json!({"decision": "denied", "by": "bruno", "reasons": ["DELETE without WHERE"], "rows": 1204}));
        let t = tail(a.path(), 2);
        assert_eq!(t.iter().map(|e| e.tool.as_str()).collect::<Vec<_>>(), ["t3", "t4"]);
        assert_eq!(t[1].outcome, Outcome::Refused);
        assert_eq!(t[1].approval.as_ref().map(|a| a.decision), Some(Decision::Denied));
        assert!(t[0].draft && !t[1].draft);
        assert!(tail(&dir.path().join("none"), 3).is_empty());
    }

    #[test]
    fn full_statement_round_trips_and_old_lines_still_parse() {
        let sql = "UPDATE orders SET status = 'paid' WHERE id = 42";
        let e = Entry {
            id: None,
            ts: chrono::Utc::now(),
            client: "claude-code".into(),
            transport: "stdio".into(),
            tool: "execute".into(),
            connection: Some("local pg".into()),
            database: Some("dataman".into()),
            sql: Some(redact(sql)),
            query: full(sql),
            rows: Some(1),
            ms: 2.0,
            outcome: Outcome::Ok,
            error: None,
            approval: None,
            draft: false,
            favorite: None,
            allowed_by: None,
            rewritten: None,
            seen: None,
        };
        let line = serde_json::to_string(&e).unwrap();
        let back: Entry = serde_json::from_str(&line).unwrap();
        assert_eq!(back, e);
        assert_eq!(back.sql.as_deref(), Some("UPDATE orders SET status = ? WHERE id = ?"));
        assert_eq!(back.statement(), Some(sql));
        let old = r#"{"ts":"2026-10-14T12:00:00Z","client":"claude-code","transport":"http","tool":"run_query","connection":"local pg","sql":"SELECT * FROM t WHERE id = ?","ms":1.5,"outcome":"ok"}"#;
        let old: Entry = serde_json::from_str(old).unwrap();
        assert_eq!((old.query.as_deref(), old.statement()), (None, Some("SELECT * FROM t WHERE id = ?")));
        let none: Entry = serde_json::from_str(r#"{"ts":"2026-10-14T12:00:00Z","client":"c","transport":"stdio","tool":"list_tables","ms":1,"outcome":"ok"}"#).unwrap();
        assert_eq!(none.statement(), None);
        assert!(full(&"x".repeat(QUERY_MAX + 1)).is_none());
    }

    #[test]
    fn replies_next_to_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("mcp-audit.jsonl");
        let a = Audit::new(log.clone());
        a.write_reply("r1", r#"{"rows":[["a•••@acme.com"]]}"#);
        let r = reply(&log, "r1").unwrap();
        assert_eq!((r.reply.as_str(), r.cut), (r#"{"rows":[["a•••@acme.com"]]}"#, false));
        assert!(reply(&log, "nope").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(replies_path(&log)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // Cut at 64 KB, on a character boundary, and said so.
        let long = "é".repeat(REPLY_MAX);
        a.write_reply("r2", &long);
        let r = reply(&log, "r2").unwrap();
        assert!(r.cut && r.reply.len() <= REPLY_MAX && r.reply.chars().all(|c| c == 'é'));
        // Rotation keeps the newest and bounds the total; what rotated out is gone.
        let chunk = "x".repeat(REPLY_MAX - 100);
        for i in 0..300 {
            a.write_reply(&format!("bulk{i}"), &chunk);
        }
        let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let rp = replies_path(&log);
        assert!(size(&rp) + size(&rp.with_extension("1.jsonl")) <= 2 * (ROTATE_AT + REPLY_MAX as u64 + 1024));
        assert!(reply(&log, "bulk299").is_some() && reply(&log, "r1").is_none(), "the oldest rotated out");
        // Several writers at once leave whole lines.
        let a = std::sync::Arc::new(Audit::new(dir.path().join("other").join("mcp-audit.jsonl")));
        let hs: Vec<_> = (0..4)
            .map(|t| {
                let a = a.clone();
                std::thread::spawn(move || (0..50).for_each(|i| a.write_reply(&format!("t{t}-{i}"), &"y".repeat(5000))))
            })
            .collect();
        hs.into_iter().for_each(|h| h.join().unwrap());
        let text = std::fs::read_to_string(replies_path(a.path())).unwrap();
        assert_eq!(text.lines().filter(|l| serde_json::from_str::<Reply>(l).is_ok()).count(), 200);
    }
}
