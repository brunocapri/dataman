// On a connection of its own, never the tab's session: SET PARSEONLY stays in force on it.

use std::ops::Range;
use std::sync::Arc;

use crate::backend::Dedicated;
use crate::config::Engine;
use crate::lint::{Lint, Severity};
use crate::{Connection, Task};

pub const MAX_BYTES: usize = 200 * 1024;

type Slot = tokio::sync::Mutex<Option<Arc<crate::backend::ms::Client>>>;

impl Connection {
    pub fn parse_check(self: &Arc<Self>, database: &str, sql: String) -> Task<Vec<Lint>> {
        let (this, db) = (self.clone(), database.to_string());
        self.spawn(async move {
            if this.engine() != Engine::SqlServer || sql.len() > MAX_BYTES || sql.trim().is_empty() {
                return Ok(Vec::new());
            }
            let slot = this.parse_slot(&db);
            let mut g = slot.lock().await;
            if g.as_ref().is_none_or(|c| c.is_closed()) {
                let Dedicated::Ms(c) = this.dedicated(Some(&db)).await? else { anyhow::bail!("not a SQL Server connection") };
                // In force for every later batch of this connection: it only ever parses.
                // (Read as rows: `execute` probes the transaction state after a batch, and the
                // probe is only parsed here.)
                c.text_rows("SET PARSEONLY ON").await.map_err(|e| anyhow::anyhow!("SET PARSEONLY ON: {e}"))?;
                *g = Some(c);
            }
            let Some(c) = g.clone() else { return Ok(Vec::new()) };
            let mut out = Vec::new();
            for b in crate::sql::split::go_batches(&sql) {
                let Err(e) = c.text_rows(&sql[b.range.clone()]).await else { continue };
                let tiberius::error::Error::Server(_) = &e else { break };
                let info = crate::backend::ms::error_info_at(&e, &sql, b.range.start);
                // An undeclared @variable (137, 1087) is how the editor writes a parameter it asks
                // for when the statement runs: not an error here.
                if matches!(info.code.as_deref(), Some("137" | "1087")) {
                    continue;
                }
                let range = info.position.map(|p| token_at(&sql, char_to_byte(&sql, p as usize - 1))).unwrap_or(b.range.start..b.range.start + 1);
                out.push(Lint { range, severity: Severity::Error, message: info.message });
            }
            Ok(out)
        })
    }

    fn parse_slot(&self, db: &str) -> Arc<Slot> {
        let key = ("parse-only", db.to_string(), String::new());
        let mut g = self.caches.lock();
        if let Some(s) = g.get(&key).cloned().and_then(|a| a.downcast::<Slot>().ok()) {
            return s;
        }
        let s: Arc<Slot> = Arc::new(tokio::sync::Mutex::new(None));
        g.insert(key, s.clone());
        s
    }
}

fn char_to_byte(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map_or(s.len(), |(i, _)| i)
}

fn token_at(s: &str, at: usize) -> Range<usize> {
    let at = at.min(s.len());
    let word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$' | '[' | ']' | '.');
    let end = s[at..].char_indices().find(|(_, c)| !word(*c)).map_or(s.len(), |(i, _)| at + i);
    if end > at {
        return at..end;
    }
    let next = s[at..].chars().next().map_or(at, |c| at + c.len_utf8());
    if next > at { at..next } else { at.saturating_sub(1)..at }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let s = "SELECT * FRM orders";
        assert_eq!(&s[token_at(s, 9)], "FRM");
        assert_eq!(&s[token_at(s, 7)], "*");
        assert_eq!(token_at(s, s.len()), s.len() - 1..s.len());
        assert_eq!(char_to_byte("café x", 4), "café".len());
    }
}
