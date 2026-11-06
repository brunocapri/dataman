//! Scan again: the columns worth a rule of their own, read on the user's connection. A name that
//! says it all counts by itself; any other name hit counts only if the column holds values the
//! Masks would let through. Suggestions only: nothing here decides what agents get.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::Value;

use crate::Task;
use crate::bounded::{Limits, Mode};
use crate::config::Engine;
use crate::conn::Connection;
use crate::dataview::sample_column_sql;
use crate::nosql::ItemKind;
use crate::privacy::{AgentRules, Mask, NameHit, Names, SensitiveCol, Types, sensitive_name, values_call_for_rule};

/// Collections (or indices) sampled for their fields, at most.
const COLLECTIONS: usize = 200;
/// Rows read of a column, and how long one read may take: a sparse column of a big table scans
/// until it finds them.
const SAMPLE_ROWS: u32 = 20;
const PROBE: Duration = Duration::from_secs(3);
/// All the reads of one scan, one at a time. Columns past it, like a read that fails, stay
/// suggested: not knowing what a column holds is no reason to drop it.
const BUDGET: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub schema: Option<String>,
    pub table: String,
    /// A nested field by its path.
    pub column: String,
    pub how: Mask,
}

impl Found {
    fn entry(&self) -> String {
        format!("{}.{}", self.table, self.column)
    }
}

#[derive(Debug, Default)]
pub struct Scan {
    pub found: Vec<Found>,
    /// Collections whose fields could not be read.
    pub unread: usize,
}

impl Scan {
    /// Adds what no entry speaks for yet; how many.
    pub fn add_to(&self, rules: &mut AgentRules, names: Names) -> usize {
        let mut added = 0;
        for f in &self.found {
            if !rules.names_column(f.schema.as_deref(), &f.table, &f.column, names) {
                rules.sensitive.push(SensitiveCol { col: f.entry(), how: f.how, on: true });
                added += 1;
            }
        }
        added
    }
}

impl Connection {
    pub fn scan_sensitive(self: &Arc<Self>, db: &str, rules: AgentRules, types: Types) -> Task<Scan> {
        let (this, db) = (self.clone(), db.to_string());
        self.spawn(async move {
            let (hits, unread) = this.name_hits(&db).await?;
            let names = Names::of(this.engine());
            let deadline = Instant::now() + BUDGET;
            let mut found: Vec<Found> = Vec::new();
            for (f, hit) in hits {
                let spoken_for = rules.names_column(f.schema.as_deref(), &f.table, &f.column, names);
                if hit.sure || spoken_for || this.holds_unmasked(&db, &f, &types, deadline).await {
                    found.push(f);
                }
            }
            found.sort_by_key(Found::entry);
            Ok(Scan { found, unread })
        })
    }

    /// Every column (or sampled field) whose name is a hit, but for views.
    async fn name_hits(self: &Arc<Self>, db: &str) -> Result<(Vec<(Found, NameHit)>, usize)> {
        let mut hits = Vec::new();
        let mut unread = 0;
        if self.engine().is_sql() {
            // As the server has it now: another session may have changed it since it was read.
            self.schema.invalidate(Some(db));
            self.preload(db).await?;
            let snap = self.schema.snapshot(db);
            for (rel, cols) in &snap.columns {
                let Some(r) = snap.relations.get(*rel) else { continue };
                if r.kind.is_view() {
                    continue;
                }
                hits.extend(cols.iter().filter_map(|c| {
                    let hit = sensitive_name(&c.name)?;
                    Some((Found { schema: Some(r.schema.clone()), table: r.name.clone(), column: c.name.clone(), how: hit.how }, hit))
                }));
            }
        } else {
            // Documents have no schema: the fields of a sample of each collection (or index), a
            // nested one by the last part of its path.
            let mongo = self.engine() == Engine::MongoDb;
            let colls = self.nosql_tree(mongo.then_some(db)).await?;
            for c in colls.iter().filter(|c| c.kind == ItemKind::Collection && !c.name.starts_with('.') && !c.name.starts_with("system.")).take(COLLECTIONS) {
                match self.sample_fields(db, &c.name).await {
                    Ok(fields) => hits.extend(fields.into_iter().filter_map(|(path, _)| {
                        let hit = sensitive_name(path.rsplit('.').next().unwrap_or(&path))?;
                        Some((Found { schema: mongo.then(|| db.to_string()), table: c.name.clone(), column: path, how: hit.how }, hit))
                    })),
                    Err(e) => {
                        log::warn!("scan: fields of {}: {e:#}", c.name);
                        unread += 1;
                    }
                }
            }
        }
        Ok((hits, unread))
    }

    /// What the column holds calls for a rule, or that can't be told.
    async fn holds_unmasked(self: &Arc<Self>, db: &str, f: &Found, types: &Types, deadline: Instant) -> bool {
        let is_sql = self.engine().is_sql();
        if !is_sql && !self.engine().caps().field_values {
            return true;
        }
        if Instant::now() >= deadline {
            log::warn!("scan: out of time before reading {}", f.entry());
            return true;
        }
        let read = if is_sql { self.sample_sql_column(db, f).await } else { self.sample_field(db, f).await };
        match read {
            Ok(values) => values_call_for_rule(&values, SAMPLE_ROWS as usize, types),
            Err(e) => {
                log::warn!("scan: reading {}: {e:#}", f.entry());
                true
            }
        }
    }

    /// On the agents' bounded path: read-only, and a read past its time is stopped on the server,
    /// not left holding the connection the explorer reads the catalog on.
    async fn sample_sql_column(&self, db: &str, f: &Found) -> Result<Vec<String>> {
        let sql = sample_column_sql(self.dialect(), f.schema.as_deref().unwrap_or_default(), &f.table, &f.column, SAMPLE_ROWS);
        let limits = Limits { max_rows: SAMPLE_ROWS as usize, max_bytes: 64 * 1024, max_cell: 200, timeout: PROBE };
        let out = self.bounded(Some(db), &sql, Vec::new(), Mode::Read, limits, std::future::pending()).await?;
        Ok(out.rows.into_iter().filter_map(|mut r| if r.is_empty() { None } else { Some(r.swap_remove(0)) }).filter_map(text).collect())
    }

    async fn sample_field(self: &Arc<Self>, db: &str, f: &Found) -> Result<Vec<String>> {
        let read = self.sample_field_values(db, &f.table, &f.column, SAMPLE_ROWS.into()).abort_on_drop();
        tokio::time::timeout(PROBE, read).await?
    }
}

fn text(v: Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s),
        other => Some(other.to_string()),
    }
}
