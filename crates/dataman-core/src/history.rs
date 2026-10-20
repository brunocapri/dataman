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
    pub rows: Option<u64>,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

pub const AGENT: &str = "agent";

pub struct History {
    dir: PathBuf,
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

    pub fn take(&self, connection_id: &str) -> Result<Option<Vec<u8>>> {
        let _g = self.lock.lock();
        let file = self.file(connection_id);
        let bytes = match std::fs::read(&file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        std::fs::remove_file(&file)?;
        Ok(Some(bytes))
    }

    pub fn put_back(&self, connection_id: &str, bytes: &[u8]) -> Result<()> {
        let _g = self.lock.lock();
        std::fs::create_dir_all(&self.dir)?;
        let file = self.file(connection_id);
        let mut all = bytes.to_vec();
        match std::fs::read(&file) {
            Ok(since) => all.extend(since),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let tmp = file.with_extension("jsonl.tmp");
        std::fs::write(&tmp, all)?;
        std::fs::rename(&tmp, &file)?;
        Ok(())
    }

    pub fn load_all(&self, connection_ids: &[String], limit: usize) -> Result<Vec<(String, HistoryEntry)>> {
        let mut all = Vec::new();
        for id in connection_ids {
            all.extend(self.load(id, limit)?.into_iter().map(|e| (id.clone(), e)));
        }
        all.sort_by(|a, b| b.1.ts.cmp(&a.1.ts));
        all.truncate(limit);
        Ok(all)
    }

    pub fn remove(&self, connection_id: &str, entry: &HistoryEntry) -> Result<Option<Vec<u8>>> {
        let _g = self.lock.lock();
        let file = self.file(connection_id);
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut taken = None;
        let mut kept = String::with_capacity(text.len());
        for line in text.split_inclusive('\n') {
            if taken.is_none() && serde_json::from_str::<HistoryEntry>(line.trim_end()).is_ok_and(|e| e == *entry) {
                taken = Some(line.as_bytes().to_vec());
            } else {
                kept.push_str(line);
            }
        }
        if taken.is_some() {
            let tmp = file.with_extension("jsonl.tmp");
            std::fs::write(&tmp, kept)?;
            std::fs::rename(&tmp, &file)?;
        }
        Ok(taken)
    }

    pub fn restore(&self, connection_id: &str, bytes: &[u8]) -> Result<()> {
        let _g = self.lock.lock();
        std::fs::create_dir_all(&self.dir)?;
        let file = self.file(connection_id);
        let now = match std::fs::read(&file) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let text = String::from_utf8_lossy(bytes).into_owned() + &String::from_utf8_lossy(&now);
        let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let at = |l: &&str| serde_json::from_str::<HistoryEntry>(l).ok().map(|e| e.ts);
        lines.sort_by_key(at);
        let mut out = lines.join("\n");
        out.push('\n');
        let tmp = file.with_extension("jsonl.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &file)?;
        Ok(())
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
        std::fs::OpenOptions::new().append(true).open(h.file("abc")).unwrap().write_all(b"{\"ts\":").unwrap();
        let got = h.load("abc", 3).unwrap();
        assert_eq!(got.iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(), ["select 4", "select 3", "select 2"]);
        assert_eq!((got[0].source.as_deref(), got[1].source.as_deref()), (Some(AGENT), None), "the agent's mark, absent for the user's");
        let text = std::fs::read_to_string(h.file("abc")).unwrap();
        assert_eq!(text.matches("\"source\"").count(), 1, "absent stays out of the file");
        assert!(h.load("other", 3).unwrap().is_empty());
    }

    #[test]
    fn take_and_put_back() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::new(dir.path().join("history"));
        let entry = |sql: &str| HistoryEntry { ts: Utc::now(), database: "db".into(), sql: sql.into(), duration_ms: 1., rows: None, outcome: Outcome::Ok, error: None, source: None };
        assert!(h.take("abc").unwrap().is_none());
        h.append("abc", &entry("select 1")).unwrap();
        h.append("abc", &entry("select 2")).unwrap();
        let bytes = h.take("abc").unwrap().unwrap();
        assert!(h.load("abc", 10).unwrap().is_empty());
        h.append("abc", &entry("select 3")).unwrap();
        h.put_back("abc", &bytes).unwrap();
        let got = h.load("abc", 10).unwrap();
        assert_eq!(got.iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(), ["select 3", "select 2", "select 1"]);
    }

    #[test]
    fn all_connections_remove_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::new(dir.path().join("history"));
        let at = |s: i64| DateTime::<Utc>::from_timestamp(1_700_000_000 + s, 0).unwrap();
        let entry = |s: i64, sql: &str| HistoryEntry { ts: at(s), database: "db".into(), sql: sql.into(), duration_ms: 1., rows: None, outcome: Outcome::Ok, error: None, source: None };
        h.append("a", &entry(1, "a1")).unwrap();
        h.append("b", &entry(2, "b2")).unwrap();
        h.append("a", &entry(3, "a3")).unwrap();
        h.append("b", &entry(4, "b4")).unwrap();
        let ids = ["a".to_string(), "b".to_string(), "gone".to_string()];
        let sqls = |h: &History| h.load_all(&ids, 10).unwrap().into_iter().map(|(c, e)| format!("{c}:{}", e.sql)).collect::<Vec<_>>();
        assert_eq!(sqls(&h), ["b:b4", "a:a3", "b:b2", "a:a1"]);
        assert_eq!(h.load_all(&ids, 2).unwrap().len(), 2);
        let line = h.remove("a", &entry(1, "a1")).unwrap().unwrap();
        assert!(h.remove("a", &entry(1, "a1")).unwrap().is_none());
        assert_eq!(sqls(&h), ["b:b4", "a:a3", "b:b2"]);
        h.append("a", &entry(5, "a5")).unwrap();
        h.restore("a", &line).unwrap();
        assert_eq!(h.load("a", 10).unwrap().iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(), ["a5", "a3", "a1"]);
        let all = h.take("b").unwrap().unwrap();
        h.append("b", &entry(6, "b6")).unwrap();
        h.restore("b", &all).unwrap();
        assert_eq!(h.load("b", 10).unwrap().iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(), ["b6", "b4", "b2"]);
    }
}
