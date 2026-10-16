//! The audit log: one JSON object per tool call in `mcp-audit.jsonl` (config directory), with
//! the time, the client, the tool, the connection, the SQL with its literals replaced by `?`
//! (shown) and as sent (to reopen it), the rows, the duration and the outcome. Never a password
//! or a bound parameter value. The file is 0600 and rotated to `mcp-audit.1.jsonl` past 8 MiB.

use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use dataman_core::sql::{TokenKind, tokenize};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// How a call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    /// Bad arguments.
    Invalid,
    /// Permissions, the guard, the global switch, a denial in the app.
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
    pub ts: chrono::DateTime<chrono::Utc>,
    /// The MCP client (`claude-code`), as it introduced itself.
    pub client: String,
    /// `stdio` (`http` in lines from when the app had an HTTP server).
    pub transport: String,
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// With literals redacted (what the log shows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
    /// The statement as the agent sent it (Copy Query, Open in Tab); absent in older lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    pub ms: f64,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A write that waited for a human: what was decided, by whom, why it was asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalNote>,
    /// `open_query` with `run: false`: the SQL was placed in an agent tab, nothing ran.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draft: bool,
    /// `run_favorite`: the name of the favorite query it ran (or drafted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorite: Option<String>,
}

/// What became of a write that needed a human's Allow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Approved,
    Denied,
    /// No answer in time: rolled back.
    Timeout,
    /// The app was not running (stdio without the app): refused.
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
    /// The local user who answered (the app's), when someone did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Why it was asked: `DELETE without WHERE`, `1204 rows (limit 100)`, `production`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    /// The rows it changed (or would have), when it ran before the question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
}

/// The local user's name (who answers in the app).
pub fn local_user() -> String {
    std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "unknown".into())
}

/// `'…'` strings, dollar-quoted strings and numbers become `?`; identifiers, keywords and `$n`
/// stay. Comments go (they can hold anything).
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
    // Long scripts keep their start.
    match s.char_indices().nth(4000) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s.to_string(),
    }
}

/// Longer statements keep only the redacted start (`sql`): one call never fills the log.
const QUERY_MAX: usize = 256 * 1024;

/// The statement as sent, for [`Entry::query`] (`None` past [`QUERY_MAX`]).
pub fn full(sql: &str) -> Option<String> {
    (sql.len() <= QUERY_MAX).then(|| sql.to_string())
}

const ROTATE_AT: u64 = 8 * 1024 * 1024;

pub struct Audit {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Entry {
    /// The statement to copy or reopen: as sent when the line has it, else the redacted one.
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

    /// Appends one line (errors go to the log: an audit failure never fails a call).
    pub fn write(&self, e: &Entry) {
        let _g = self.lock.lock();
        if let Err(err) = self.append(e) {
            log::warn!("audit log {}: {err}", self.path.display());
        }
    }

    fn append(&self, e: &Entry) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() > ROTATE_AT) {
            let _ = std::fs::rename(&self.path, self.path.with_extension("1.jsonl"));
        }
        let mut line = serde_json::to_string(e).map_err(std::io::Error::other)?;
        line.push('\n');
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        f.write_all(line.as_bytes())
    }
}

/// The last `n` entries of the log at `path`, newest last (reads at most the file's last MiB).
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
        };
        let line = serde_json::to_string(&e).unwrap();
        let back: Entry = serde_json::from_str(&line).unwrap();
        assert_eq!(back, e);
        assert_eq!(back.sql.as_deref(), Some("UPDATE orders SET status = ? WHERE id = ?"));
        assert_eq!(back.statement(), Some(sql));
        // A line written before `query` existed: the redacted text is all there is.
        let old = r#"{"ts":"2026-10-14T12:00:00Z","client":"claude-code","transport":"http","tool":"run_query","connection":"local pg","sql":"SELECT * FROM t WHERE id = ?","ms":1.5,"outcome":"ok"}"#;
        let old: Entry = serde_json::from_str(old).unwrap();
        assert_eq!((old.query.as_deref(), old.statement()), (None, Some("SELECT * FROM t WHERE id = ?")));
        let none: Entry = serde_json::from_str(r#"{"ts":"2026-10-14T12:00:00Z","client":"c","transport":"stdio","tool":"list_tables","ms":1,"outcome":"ok"}"#).unwrap();
        assert_eq!(none.statement(), None);
        assert!(full(&"x".repeat(QUERY_MAX + 1)).is_none());
    }
}
