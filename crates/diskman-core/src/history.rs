//! Query history per connection: one JSON line per execution, append-only.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    Error,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub ts: DateTime<Utc>,
    pub database: String,
    pub sql: String,
    pub duration_ms: f64,
    /// Rows fetched, or rows affected for statements without a result.
    pub rows: Option<u64>,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Who ran it when it was not the user: `agent` (an MCP client, through its agent tab).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// [`HistoryEntry::source`] of an agent's statements.
pub const AGENT: &str = "agent";

pub struct History {
    dir: PathBuf,
    /// Serializes appends from concurrent sessions.
    lock: Mutex<()>,
}

impl History {
    pub fn new(dir: PathBuf) -> History {
        History { dir, lock: Mutex::new(()) }
    }

    fn file(&self, connection_id: &str) -> PathBuf {
        let safe: String = connection_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        self.dir.join(format!("{safe}.jsonl"))
    }

    pub fn append(&self, connection_id: &str, entry: &HistoryEntry) -> Result<()> {
        let _g = self.lock.lock();
        std::fs::create_dir_all(&self.dir)?;
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.file(connection_id))?;
        f.write_all(line.as_bytes())?;
        Ok(())
    }

    /// The last `limit` entries, newest first. Unreadable lines are skipped.
    pub fn load(&self, connection_id: &str, limit: usize) -> Result<Vec<HistoryEntry>> {
        let f = match std::fs::File::open(self.file(connection_id)) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut all: std::collections::VecDeque<HistoryEntry> = Default::default();
        for line in std::io::BufReader::new(f).lines() {
            let Ok(line) = line else { continue };
            if let Ok(e) = serde_json::from_str(&line) {
                if all.len() == limit {
                    all.pop_front();
                }
                all.push_back(e);
            }
        }
        Ok(all.into_iter().rev().collect())
    }

    pub fn clear(&self, connection_id: &str) -> Result<()> {
        let _g = self.lock.lock();
        match std::fs::remove_file(self.file(connection_id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_load_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::new(dir.path().join("history"));
        for i in 0..5 {
            h.append(
                "abc",
                &HistoryEntry {
                    ts: Utc::now(),
                    database: "db".into(),
                    sql: format!("select {i}"),
                    duration_ms: 1.5,
                    rows: Some(1),
                    outcome: Outcome::Ok,
                    error: None,
                    source: (i == 4).then(|| AGENT.to_string()),
                },
            )
            .unwrap();
        }
        // A torn line (crash mid-write) does not break loading.
        std::fs::OpenOptions::new().append(true).open(h.file("abc")).unwrap().write_all(b"{\"ts\":").unwrap();
        let got = h.load("abc", 3).unwrap();
        assert_eq!(got.iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(), ["select 4", "select 3", "select 2"]);
        assert_eq!((got[0].source.as_deref(), got[1].source.as_deref()), (Some(AGENT), None), "the agent's mark, absent for the user's");
        let text = std::fs::read_to_string(h.file("abc")).unwrap();
        assert_eq!(text.matches("\"source\"").count(), 1, "absent stays out of the file");
        assert!(h.load("other", 3).unwrap().is_empty());
    }
}
