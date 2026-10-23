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
    let _held = hold(path)?;
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

/// Every write to the log and its replies holds this lock, across processes (the app's hub, each
/// `dataman-mcp`): a clear replaces the files under it, so no line lands in a file going away.
fn hold(path: &Path) -> std::io::Result<std::fs::File> {
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path.with_file_name("mcp-audit.lock"))?;
    f.lock()?;
    Ok(f)
}

/// What Agents › Audit › Clear… removes: everything, what came before a day, or all but the
/// recent past counted back from today. Only the user clears, from the app: no tool does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clear {
    All,
    Before(chrono::NaiveDate),
    Keep(Recent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recent {
    Days30,
    Months3,
    Months6,
}

impl Recent {
    pub const ALL: [Recent; 3] = [Recent::Days30, Recent::Months3, Recent::Months6];

    pub fn label(self) -> &'static str {
        match self {
            Recent::Days30 => "the last 30 days",
            Recent::Months3 => "the last 3 months",
            Recent::Months6 => "the last 6 months",
        }
    }

    /// The first day kept: today less the span (31 Aug less 6 months is 28 Feb).
    pub fn since(self, today: chrono::NaiveDate) -> chrono::NaiveDate {
        let back = |m| today.checked_sub_months(chrono::Months::new(m)).unwrap_or(chrono::NaiveDate::MIN);
        match self {
            Recent::Days30 => today - chrono::Days::new(30),
            Recent::Months3 => back(3),
            Recent::Months6 => back(6),
        }
    }
}

impl Clear {
    /// The first day kept, in the user's time zone; None when nothing is.
    pub fn first_kept(self, today: chrono::NaiveDate) -> Option<chrono::NaiveDate> {
        match self {
            Clear::All => None,
            Clear::Before(d) => Some(d),
            Clear::Keep(r) => Some(r.since(today)),
        }
    }

    /// Calls logged before this go: the start of the first day kept in `now`'s time zone, or
    /// `now` itself for All, so a call logged after the user said yes stays.
    pub fn cutoff<Tz: chrono::TimeZone>(self, now: &chrono::DateTime<Tz>) -> chrono::DateTime<chrono::Utc> {
        match self.first_kept(now.date_naive()) {
            None => now.with_timezone(&chrono::Utc),
            Some(d) => day_start(&now.timezone(), d),
        }
    }
}

/// When a day starts in a time zone: midnight, or the first minute after it where clocks spring
/// forward at midnight.
pub fn day_start<Tz: chrono::TimeZone>(tz: &Tz, d: chrono::NaiveDate) -> chrono::DateTime<chrono::Utc> {
    let midnight = d.and_time(chrono::NaiveTime::MIN);
    (0..=24 * 4)
        .map(|q| midnight + chrono::Duration::minutes(15 * q))
        .find_map(|t| tz.from_local_datetime(&t).earliest())
        .map_or_else(|| midnight.and_utc(), |t| t.with_timezone(&chrono::Utc))
}

/// Just what a clear reads of a line, so lines from other versions still date.
#[derive(Deserialize)]
struct Stamp {
    #[serde(default)]
    id: Option<String>,
    ts: chrono::DateTime<chrono::Utc>,
}

/// The rotated copy, then the current file.
fn both(path: &Path) -> [PathBuf; 2] {
    [path.with_extension("1.jsonl"), path.to_path_buf()]
}

/// The calls a clear at `cutoff` removes, in the log and its rotated copy.
pub fn count_before(audit: &Path, cutoff: chrono::DateTime<chrono::Utc>) -> usize {
    both(audit)
        .iter()
        .filter_map(|p| std::fs::File::open(p).ok())
        .flat_map(|f| std::io::BufReader::new(f).split(b'\n').map_while(Result::ok))
        .filter(|l| serde_json::from_slice::<Stamp>(l).is_ok_and(|s| s.ts < cutoff))
        .count()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cleared {
    pub calls: usize,
    pub replies: usize,
}

/// Removes the calls logged before `cutoff` with their replies, and any older reply whose call
/// is gone (a reply is written just before its call, so a newer one waits for it). Each file is
/// written whole beside itself, then renamed over it, under the writers' lock.
pub fn clear_before(audit: &Path, cutoff: chrono::DateTime<chrono::Utc>) -> std::io::Result<Cleared> {
    let _held = hold(audit)?;
    let (mut gone, mut kept) = (std::collections::HashSet::new(), std::collections::HashSet::new());
    let mut out = Cleared::default();
    for p in both(audit) {
        out.calls += rewrite(&p, |s| {
            let keep = s.ts >= cutoff;
            if let Some(id) = s.id {
                if keep { kept.insert(id) } else { gone.insert(id) };
            }
            keep
        })?;
    }
    for p in both(&replies_path(audit)) {
        out.replies += rewrite(&p, |s| s.id.is_none_or(|id| !gone.contains(&id) && (s.ts >= cutoff || kept.contains(&id))))?;
    }
    Ok(out)
}

/// Keeps the lines `keep` says to (one that does not parse goes too, uncounted); the lines
/// removed. A writer that took no lock (an older `dataman-mcp`) still loses nothing appended
/// before the rename: the file is read to its end as it is then.
fn rewrite(path: &Path, mut keep: impl FnMut(Stamp) -> bool) -> std::io::Result<usize> {
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let tmp = path.with_extension("jsonl.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut w = std::io::BufWriter::new(opts.open(&tmp)?);
    let (mut removed, mut dropped, mut left, mut read) = (0, 0, 0, 0u64);
    let mut r = std::io::BufReader::new(f);
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = r.read_until(b'\n', &mut line)?;
        if n == 0 {
            if std::fs::metadata(path)?.len() > read {
                continue;
            }
            break;
        }
        read += n as u64;
        let text = line.strip_suffix(b"\n").unwrap_or(&line);
        match serde_json::from_slice::<Stamp>(text).map(&mut keep) {
            Ok(true) => {
                w.write_all(text)?;
                w.write_all(b"\n")?;
                left += 1;
            }
            Ok(false) => removed += 1,
            Err(_) if text.is_empty() => {}
            Err(_) => dropped += 1,
        }
    }
    let w = w.into_inner().map_err(|e| e.into_error())?;
    if removed + dropped == 0 {
        drop(w);
        std::fs::remove_file(&tmp)?;
        return Ok(0);
    }
    if left == 0 {
        drop(w);
        std::fs::remove_file(&tmp)?;
        std::fs::remove_file(path)?;
    } else {
        w.sync_all()?;
        std::fs::rename(&tmp, path)?;
    }
    if let Some(dir) = path.parent().and_then(|d| std::fs::File::open(d).ok()) {
        let _ = dir.sync_all();
    }
    Ok(removed)
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

    fn call(id: &str, ts: &str) -> Entry {
        Entry {
            id: Some(id.into()),
            ts: ts.parse().unwrap(),
            client: "claude-code".into(),
            transport: "stdio".into(),
            tool: "run_query".into(),
            connection: None,
            database: None,
            sql: None,
            query: None,
            rows: None,
            ms: 1.,
            outcome: Outcome::Ok,
            error: None,
            approval: None,
            draft: false,
            favorite: None,
            allowed_by: None,
            rewritten: None,
            seen: None,
        }
    }

    fn reply_at(path: &Path, id: &str, ts: &str) {
        let r = Reply { id: id.into(), ts: ts.parse().unwrap(), cut: false, reply: format!("reply of {id}") };
        append_line(&replies_path(path), &serde_json::to_string(&r).unwrap()).unwrap();
    }

    fn ids(path: &Path) -> Vec<String> {
        both(path).iter().filter_map(|p| std::fs::read_to_string(p).ok()).flat_map(|t| t.lines().map(|l| serde_json::from_str::<Stamp>(l).unwrap().id.unwrap()).collect::<Vec<_>>()).collect()
    }

    fn day(s: &str) -> chrono::NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn cutoffs_in_the_local_time_zone() {
        let brt = chrono::FixedOffset::west_opt(3 * 3600).unwrap();
        let now = "2026-10-23T14:30:00-03:00".parse::<chrono::DateTime<chrono::FixedOffset>>().unwrap();
        let at = |s: &str| s.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
        assert_eq!(Clear::All.cutoff(&now), at("2026-10-23T17:30:00Z"), "everything up to the moment the user said yes");
        // Before a day: the day starts at local midnight, 03:00 UTC here.
        assert_eq!(Clear::Before(day("2026-10-01")).cutoff(&now), at("2026-10-01T03:00:00Z"));
        assert_eq!(Clear::Keep(Recent::Days30).cutoff(&now), at("2026-09-23T03:00:00Z"));
        assert_eq!(Clear::Keep(Recent::Months3).cutoff(&now), at("2026-07-23T03:00:00Z"));
        assert_eq!(Clear::Keep(Recent::Months6).cutoff(&now), at("2026-04-23T03:00:00Z"));
        // Counted from today in the user's zone: at 22:00 on the 23rd it is already the 24th in UTC.
        let late = "2026-10-23T22:00:00-03:00".parse::<chrono::DateTime<chrono::FixedOffset>>().unwrap();
        assert_eq!(Clear::Keep(Recent::Days30).cutoff(&late), at("2026-09-23T03:00:00Z"));
        assert_eq!(Clear::Keep(Recent::Days30).cutoff(&late.with_timezone(&chrono::Utc)), at("2026-09-24T00:00:00Z"));
        // Months from a long month's end land on the shorter month's last day.
        assert_eq!(Recent::Months6.since(day("2026-08-31")), day("2026-02-28"));
        assert_eq!(Recent::Months3.since(day("2024-05-31")), day("2024-02-29"));
        assert_eq!(Recent::Days30.since(day("2026-03-01")), day("2026-01-30"));
        assert_eq!(Clear::All.first_kept(day("2026-10-23")), None);
        assert_eq!(day_start(&brt, day("2026-10-22")), at("2026-10-22T03:00:00Z"));
        assert_eq!(day_start(&chrono::Utc, day("2026-10-22")), at("2026-10-22T00:00:00Z"));
    }

    #[test]
    fn clearing_before_a_day_keeps_the_day_itself() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("mcp-audit.jsonl");
        let a = Audit::new(log.clone());
        // 22 Oct starts at 03:00 UTC in -03:00: a call at 02:59:59.999 UTC is still the 21st.
        for (id, ts) in [("a", "2026-10-20T12:00:00Z"), ("b", "2026-10-22T02:59:59.999Z"), ("c", "2026-10-22T03:00:00Z"), ("d", "2026-10-23T10:00:00Z")] {
            a.write(&call(id, ts));
        }
        let brt = chrono::FixedOffset::west_opt(3 * 3600).unwrap();
        let cutoff = day_start(&brt, day("2026-10-22"));
        assert_eq!(count_before(&log, cutoff), 2);
        assert_eq!(clear_before(&log, cutoff).unwrap(), Cleared { calls: 2, replies: 0 });
        assert_eq!(ids(&log), ["c", "d"]);
        assert_eq!(count_before(&log, cutoff), 0);
        assert_eq!(clear_before(&log, cutoff).unwrap(), Cleared::default(), "nothing left to clear");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600, "the rewritten log stays 0600");
        }
        assert!(!log.with_extension("jsonl.tmp").exists());
    }

    #[test]
    fn clearing_takes_the_replies_and_the_rotated_copy() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("mcp-audit.jsonl");
        // The rotated copy holds the oldest calls.
        let old = log.with_extension("1.jsonl");
        for (id, ts) in [("r1", "2026-06-01T10:00:00Z"), ("r2", "2026-09-30T10:00:00Z")] {
            append_line(&old, &serde_json::to_string(&call(id, ts)).unwrap()).unwrap();
        }
        let a = Audit::new(log.clone());
        for (id, ts) in [("c1", "2026-09-01T10:00:00Z"), ("c2", "2026-10-10T10:00:00Z")] {
            a.write(&call(id, ts));
        }
        reply_at(&log, "r1", "2026-06-01T09:59:59Z");
        reply_at(&log, "c1", "2026-09-01T09:59:59Z");
        // A kept call's reply, written a moment before the call itself.
        reply_at(&log, "c2", "2026-10-10T09:59:59.9Z");
        // A reply whose call is long gone (rotated out of the log): an orphan.
        reply_at(&log, "lost", "2026-05-01T00:00:00Z");
        // A reply whose call is still on its way: newer than the cutoff, it waits.
        reply_at(&log, "coming", "2026-10-23T10:00:00Z");
        let cutoff = day_start(&chrono::Utc, day("2026-09-23"));
        assert_eq!(count_before(&log, cutoff), 2);
        assert_eq!(clear_before(&log, cutoff).unwrap(), Cleared { calls: 2, replies: 3 });
        assert_eq!(ids(&log), ["r2", "c2"]);
        let left: Vec<String> = std::fs::read_to_string(replies_path(&log)).unwrap().lines().map(|l| serde_json::from_str::<Reply>(l).unwrap().id).collect();
        assert_eq!(left, ["c2", "coming"]);
        assert!(reply(&log, "c1").is_none() && reply(&log, "c2").is_some());
        // Everything: the files go, and the next call starts a new log.
        let now = "2026-10-23T12:00:00Z".parse().unwrap();
        assert_eq!(count_before(&log, now), 2);
        assert_eq!(clear_before(&log, now).unwrap(), Cleared { calls: 2, replies: 2 });
        assert!(!log.exists() && !old.exists() && !replies_path(&log).exists());
        a.write(&call("next", "2026-10-23T12:00:01Z"));
        assert_eq!(ids(&log), ["next"]);
    }

    fn left_replies(log: &Path) -> Vec<String> {
        std::fs::read_to_string(replies_path(log)).unwrap_or_default().lines().map(|l| serde_json::from_str::<Reply>(l).unwrap().id).collect()
    }

    #[test]
    fn a_clear_loses_no_call_written_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("mcp-audit.jsonl");
        let a = std::sync::Arc::new(Audit::new(log.clone()));
        for i in 0..2000 {
            a.write(&call(&format!("old{i}"), "2026-01-01T00:00:00Z"));
            a.write_reply(&format!("old{i}"), &"z".repeat(200));
        }
        // Writers of their own (as the app's hub and a dataman-mcp would be) keep going while
        // the log is cleared.
        let hs: Vec<_> = (0..4)
            .map(|t| {
                let log = log.clone();
                std::thread::spawn(move || {
                    let w = Audit::new(log);
                    for i in 0..200 {
                        let id = format!("new{t}-{i}");
                        w.write_reply(&id, "fresh");
                        w.write(&call(&id, "2026-10-23T12:00:00Z"));
                    }
                })
            })
            .collect();
        let cleared = clear_before(&log, "2026-06-01T00:00:00Z".parse().unwrap()).unwrap();
        hs.into_iter().for_each(|h| h.join().unwrap());
        assert_eq!(cleared, Cleared { calls: 2000, replies: 2000 });
        let left = ids(&log);
        assert_eq!(left.len(), 800, "every call written during the clear is there");
        assert!(left.iter().all(|id| id.starts_with("new")));
        let replies = left_replies(&log);
        assert_eq!(replies.len(), 800);
        assert!(replies.iter().all(|id| id.starts_with("new")));
    }
}
