//! The client-side check is what keeps a read a read on a server that accepts `READ ONLY`
//! without enforcing it. A server may not enforce `statement_timeout` either, so a local timer
//! backs it.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use parking_lot::Mutex;
use serde_json::Value;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{Client, SimpleQueryMessage};

use crate::backend::pg::{TextParam, column_infos, error_info, push_row};
use crate::backend::{Dedicated, RowSource, ch, duck, lite, ms, my};
use crate::conn::Connection;
use crate::guard;
use crate::result::{Cell, ChunkBuilder, ColumnInfo, ErrorClass, ErrorInfo, format_cell};
use crate::sql::Dialect;
use crate::value::{Kind, Storage};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Read,
    Write,
    Rollback,
    Autocommit,
}

pub type Decide = Box<dyn FnOnce(&Outcome) -> futures_util::future::BoxFuture<'static, bool> + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_rows: usize,
    pub max_bytes: usize,
    pub max_cell: usize,
    pub timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_rows: 200, max_bytes: 256 * 1024, max_cell: 2000, timeout: Duration::from_secs(30) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truncation {
    Rows,
    Bytes,
}

impl Truncation {
    pub fn label(self) -> &'static str {
        match self {
            Truncation::Rows => "rows",
            Truncation::Bytes => "bytes",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub type_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    pub verb: String,
    pub affected: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Part {
    pub statement: usize,
    pub sql: String,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    pub truncated: Option<Truncation>,
    pub cut_cells: usize,
    pub elapsed: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    pub truncated: Option<Truncation>,
    pub cut_cells: usize,
    pub bytes: usize,
    pub statements: Vec<Done>,
    pub elapsed: Duration,
    pub committed: bool,
    pub parts: Vec<Part>,
}

impl Outcome {
    fn end_part(&mut self, statement: usize, sql: &str, cut0: usize, elapsed: Duration) {
        self.parts.push(Part {
            statement,
            sql: sql.to_string(),
            columns: self.columns.clone(),
            rows: self.rows.clone(),
            truncated: self.truncated,
            cut_cells: self.cut_cells - cut0,
            elapsed,
        });
    }

    fn start_part(&mut self) {
        self.columns.clear();
        self.rows.clear();
        self.truncated = None;
    }

    pub fn total_rows(&self) -> usize {
        if self.parts.is_empty() { self.rows.len() } else { self.parts.iter().map(|p| p.rows.len()).sum() }
    }

    pub fn affected(&self) -> u64 {
        self.statements.iter().filter_map(|d| d.affected).sum()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Stop {
    #[error("refused: {0}")]
    Refused(String),
    #[error("timed out after {}", fmt_secs(*.0))]
    Timeout(Duration),
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Server(ErrorInfo),
    #[error("{0:#}")]
    Other(anyhow::Error),
}

fn fmt_secs(d: Duration) -> String {
    format!("{:.1} s", d.as_secs_f64())
}

pub fn statements(sql: &str) -> Vec<String> {
    statements_in(sql, Dialect::Postgres)
}

pub fn statements_in(sql: &str, dialect: Dialect) -> Vec<String> {
    crate::sql::split_in(sql, dialect).into_iter().map(|s| sql[s.range].trim().to_string()).filter(|s| !s.is_empty()).collect()
}

pub fn refusal(stmts: &[String], params: usize, mode: Mode, read_only_profile: bool) -> Option<String> {
    refusal_in(stmts, params, mode, read_only_profile, Dialect::Postgres)
}

pub fn refusal_in(stmts: &[String], params: usize, mode: Mode, read_only_profile: bool, dialect: Dialect) -> Option<String> {
    if stmts.is_empty() {
        return Some("empty statement".into());
    }
    if params > 0 && stmts.len() > 1 {
        return Some("parameters need a single statement".into());
    }
    if params > 0 && dialect == Dialect::TSql && let Err(why) = crate::params::tsql_bound(&stmts[0], params) {
        return Some(why);
    }
    if mode == Mode::Autocommit && stmts.len() > 1 {
        return Some("a statement that runs outside a transaction runs alone".into());
    }
    // Outside a transaction only the session holds it read-only, and a session setting is any
    // statement's to change (`set_config` in a function).
    if mode == Mode::Autocommit && read_only_profile && dialect.pin_read_only().is_some() {
        return Some("read-only connection: a statement outside a transaction is not run".into());
    }
    for (i, s) in stmts.iter().enumerate() {
        if mode == Mode::Read {
            if let Some(r) = guard::agent_read_refusal_in(s, dialect) {
                return Some(if stmts.len() > 1 { format!("statement {}: {r}", i + 1) } else { r });
            }
            continue;
        }
        let f = guard::classify_in(s, dialect);
        if f.verb.is_empty() {
            return Some("empty statement".into());
        }
        if f.tx.is_some() {
            return Some(format!("transaction control ({}) is not allowed: every call is its own transaction", f.verb));
        }
        if f.outside_tx && mode != Mode::Autocommit {
            return Some(format!("{} cannot run inside a transaction", f.verb));
        }
        if read_only_profile && let Some(r) = guard::read_only_refusal_in(s, dialect) {
            return Some(r);
        }
    }
    None
}

/// The database `USE x` names (the one of the last `USE`, where a text has more than one).
pub(crate) fn used_database(sql: &str, d: Dialect) -> Option<String> {
    let mut used = None;
    for range in statement_ranges(sql, d) {
        let s = &sql[range];
        let toks: Vec<_> = crate::sql::tokenize_in(s, d).into_iter().filter(|t| !t.kind.is_trivia()).collect();
        if !toks.first().is_some_and(|t| t.text(s).eq_ignore_ascii_case("USE")) {
            continue;
        }
        // `USE database` is a database of that name.
        let name = match toks.get(2).filter(|t| t.text(s) != ";") {
            Some(n) if toks[1].text(s).eq_ignore_ascii_case("DATABASE") => Some(n),
            _ => toks.get(1),
        };
        if let Some(n) = name {
            used = Some(crate::sql::ident_value_in(s, n, d));
        }
    }
    used
}

/// T-SQL needs no `;` between its statements.
fn statement_ranges(sql: &str, d: Dialect) -> Vec<std::ops::Range<usize>> {
    if d == Dialect::TSql { guard::tsql_statements(sql) } else { crate::sql::split_in(sql, d).into_iter().map(|s| s.range).collect() }
}

/// What starts a statement that writes: from here on a `SELECT` before it is no reason to take a
/// name as read. A word followed by `(` is a function (`REPLACE(a, 'x', 'y')`).
const WRITE_WORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "MERGE", "REPLACE", "CREATE", "ALTER", "DROP", "TRUNCATE", "RENAME", "EXCHANGE", "OPTIMIZE", "ATTACH", "DETACH", "UNDROP", "EXEC", "EXECUTE", "CALL", "GRANT", "REVOKE", "LOAD",
];

/// The databases a statement that writes reaches by name: the first part of each name long enough
/// to start with one (SQL Server's four parts have the linked server first), and what follows
/// `DATABASE` (`SCHEMA` where a schema is a database) or `TABLES FROM`.
///
/// Read by the tokens, so a name that is not there is not reached: a table a stored procedure or a
/// view writes to is the server's to know. A name in a query's `SELECT` is only read, so copying
/// out of a read-only database stays allowed; everything else counts, which refuses
/// `CREATE TABLE w.t AS ro.s` as well, and an `x.y` that turns out to be a table and its column.
pub(crate) fn databases_written(sql: &str, d: Dialect, parts: u8, schema_is_database: bool) -> Vec<String> {
    use crate::sql::TokenKind;
    let toks: Vec<_> = crate::sql::tokenize_in(sql, d).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let is_name = |i: usize| toks.get(i).is_some_and(|t| matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword));
    let word = |i: usize| toks.get(i).filter(|t| matches!(t.kind, TokenKind::Ident | TokenKind::Keyword)).map(|t| t.text(sql));
    let is_word = |i: usize, w: &str| word(i).is_some_and(|x| x.eq_ignore_ascii_case(w));
    let is_punct = |i: usize, p: &str| toks.get(i).is_some_and(|t| t.kind == TokenKind::Punct && t.text(sql) == p);
    let mut out: Vec<String> = Vec::new();
    // Per parenthesis: whether a query's `SELECT` has been seen there, or around it.
    let mut read = vec![false];
    let mut writes = false;
    let mut i = 0;
    while i < toks.len() {
        if is_punct(i, "(") {
            read.push(read.last().copied().unwrap_or(false));
        } else if is_punct(i, ")") && read.len() > 1 {
            read.pop();
        }
        if is_word(i, "SELECT") {
            // Before the write it is a common table expression, and in `DELETE x FROM (SELECT …) x`
            // a derived table: both can be what is written to.
            let derived = i >= 2 && is_punct(i - 1, "(") && (is_word(i - 2, "FROM") || is_word(i - 2, "JOIN"));
            if writes && !derived && let Some(r) = read.last_mut() {
                *r = true;
            }
        } else if WRITE_WORDS.iter().any(|w| is_word(i, w)) && !is_punct(i + 1, "(") {
            writes = true;
            if let Some(r) = read.last_mut() {
                *r = false;
            }
        }
        if !is_name(i) {
            i += 1;
            continue;
        }
        let named = is_word(i, "DATABASE") || (schema_is_database && is_word(i, "SCHEMA")) || (is_word(i, "TABLES") && (is_word(i + 1, "FROM") || is_word(i + 1, "IN")));
        if named {
            let mut n = if is_word(i, "TABLES") { i + 2 } else { i + 1 };
            if is_word(n, "IF") {
                n += if is_word(n + 1, "NOT") { 3 } else { 2 };
            }
            if is_name(n) {
                out.push(crate::sql::ident_value_in(sql, &toks[n], d));
            }
        }
        // `a.b.c`, and T-SQL's `db..t` with its default schema left out.
        let first = i;
        let mut count = 1u8;
        let mut second = None;
        while is_punct(i + 1, ".") {
            i += 1;
            count = count.saturating_add(1);
            if is_name(i + 1) {
                i += 1;
                second.get_or_insert(i);
            }
        }
        let after_into = first > 0 && is_word(first - 1, "INTO");
        if count >= parts && (after_into || !read.last().copied().unwrap_or(false)) {
            out.push(crate::sql::ident_value_in(sql, &toks[first], d));
            if count >= 4
                && let Some(s) = second
            {
                out.push(crate::sql::ident_value_in(sql, &toks[s], d));
            }
        }
        i += 1;
    }
    out
}

/// Modes that change how MySQL reads strings (and the composite modes that turn them on).
const MY_LEXING_MODES: &[&str] = &["NO_BACKSLASH_ESCAPES", "ANSI_QUOTES", "ANSI", "ORACLE", "MSSQL", "DB2", "POSTGRESQL", "MAXDB"];

fn my_lexed_sql_mode() -> String {
    let mut e = "CONCAT(',', @@SESSION.sql_mode, ',')".to_string();
    for m in MY_LEXING_MODES {
        e = format!("REPLACE({e}, ',{m},', ',')");
    }
    format!("SET SESSION sql_mode = TRIM(BOTH ',' FROM {e})")
}

/// A literal Dataman writes (`Dialect::quote_literal`) reads as written only under these settings,
/// whatever the server or the database has as its own.
pub(crate) async fn strings_as_written(d: &Dedicated) -> anyhow::Result<()> {
    match d {
        // Shared per transaction, it would reach other clients: agents set it on each transaction.
        Dedicated::Pg(p) if p.pooler.is_some_and(|x| x.per_transaction()) => {}
        Dedicated::Pg(p) => {
            p.client.simple_query("SET standard_conforming_strings = on").await?;
        }
        Dedicated::My(c) => {
            c.execute(&my_lexed_sql_mode()).await?;
        }
        Dedicated::Ms(_) | Dedicated::Lite(_) | Dedicated::Duck(_) | Dedicated::Ch(_) => {}
    }
    Ok(())
}

const GRACE: Duration = Duration::from_millis(500);
const POOL_SIZE: usize = 2;
pub(crate) type Pool = Mutex<Vec<Dedicated>>;

impl Connection {
    /// Whether a name in a statement is a read-only database. A database the user set counts by
    /// any case: the lexer cannot see how the server folds names, and a wrong refusal says why.
    /// Past those, under a connection read-only by default, a name counts when the server has such
    /// a database, and while the list is unknown (the first part of `t.col` is no database).
    fn names_read_only(&self, name: &str) -> bool {
        let Some(name) = self.reached(name) else { return self.read_only_for(name) };
        let set: Vec<bool> = self.read_only_dbs.read().iter().filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, on)| *on).collect();
        if !set.is_empty() {
            return set.contains(&true);
        }
        self.profile.read_only && self.schema.cached_databases().is_none_or(|known| known.iter().any(|d| d.eq_ignore_ascii_case(name)))
    }

    /// Why `sql` must not run with `database` current: a write on a read-only database, or a write
    /// that reaches one by name or after a `USE`, in any statement of the text. Both a tab and an
    /// agent ask this, before anything is sent. Only the current database's state is held on the
    /// server's side (a read-only session); the rest is this check.
    pub fn read_only_refusal(&self, database: &str, sql: &str) -> Option<String> {
        let d = self.dialect();
        let held = self.caps().read_only_held;
        if self.read_only_for(database)
            && let Some(r) = guard::read_only_refusal_held(sql, d, held)
        {
            return Some(r);
        }
        let spec = self.engine().spec();
        let parts = spec.db_in_name?;
        if !self.profile.read_only && !self.read_only_dbs.read().values().any(|on| *on) {
            return None;
        }
        for text in crate::sql::exec_readings(sql, d) {
            let mut current = database.to_string();
            for range in statement_ranges(&text, d) {
                let s = &text[range];
                if let Some(used) = used_database(s, d) {
                    current = used;
                    continue;
                }
                if guard::read_only_refusal_held(s, d, held).is_none() {
                    continue;
                }
                let verb = || guard::classify_in(s, d).verb;
                if self.read_only_for(&current) {
                    return Some(format!("read-only database {current}: {} is not run", verb()));
                }
                if let Some(name) = databases_written(s, d, parts, !spec.caps.schemas).into_iter().find(|n| self.names_read_only(n)) {
                    return Some(format!("read-only database {name}: {} is not run", verb()));
                }
            }
        }
        None
    }

    /// `sql` moves to a read-only database by a `USE`.
    pub(crate) fn uses_read_only(&self, sql: &str) -> bool {
        let d = self.dialect();
        crate::sql::exec_readings(sql, d).iter().any(|text| statement_ranges(text, d).into_iter().any(|r| used_database(&text[r], d).is_some_and(|db| self.read_only_for(&db))))
    }

    /// What an agent's read must not run on a server that keeps no read-only session (Vitess):
    /// its read-only transaction holds writes, and this keeps to reads what it lets through.
    pub(crate) fn unheld_read_refusal(&self, sql: &str) -> Option<String> {
        if self.caps().read_only_held { None } else { guard::read_only_refusal_held(sql, self.dialect(), false) }
    }

    /// The pool of `db`'s connections made while it was read-only (`ro`) or not: a toggle in
    /// the explorer never hands out a connection made for the other state.
    fn agent_pool(&self, db: &str, ro: bool) -> Arc<Pool> {
        self.agent_pools.lock().entry((db.to_string(), ro)).or_default().clone()
    }

    /// A file database's agents get connections of their own make: read-only (and refusing
    /// writes) for reads, refusing escapes for every level.
    pub(crate) async fn agent_take(&self, db: &str, ro: bool) -> anyhow::Result<Dedicated> {
        let pool = self.agent_pool(db, ro);
        loop {
            let d = pool.lock().pop();
            match d {
                Some(d) if !d.db().is_closed() => return Ok(d),
                Some(_) => continue,
                None => break,
            }
        }
        let d = if self.engine() == crate::config::Engine::Sqlite {
            let agent = if ro { lite::Agent::Read } else { lite::Agent::Write };
            Dedicated::Lite(lite::connect(&self.profile_for(db), lite::Open { read_only: ro, agent: Some(agent), kept: Some(self.agent_rules.clone()) }, None).await?)
        } else if !self.dialect().has_transactions() {
            self.dedicated_read_only(db, ro).await?
        } else {
            self.dedicated(Some(db)).await?
        };
        // The guard lexes strings as these settings read them: the server must too. Errors come
        // in English where the login may choose (what privacy::clean_error knows how to clean);
        // PostgreSQL lets only a superuser set lc_messages.
        strings_as_written(&d).await?;
        match &d {
            Dedicated::Pg(p) if p.pooler.is_some_and(|x| x.per_transaction()) => {}
            Dedicated::Pg(p) => {
                let _ = p.client.simple_query("SET application_name = 'Dataman MCP'").await;
                let _ = p.client.simple_query("SET lc_messages = 'C'").await;
            }
            Dedicated::My(c) => {
                let _ = c.execute("SET SESSION lc_messages = 'en_US'").await;
            }
            Dedicated::Ms(c) => {
                let _ = c.execute("SET LANGUAGE us_english").await;
            }
            Dedicated::Lite(_) | Dedicated::Duck(_) | Dedicated::Ch(_) => {}
        }
        Ok(d)
    }

    pub(crate) fn agent_give(&self, db: &str, ro: bool, d: Dedicated) {
        if d.db().is_closed() {
            return;
        }
        let pool = self.agent_pool(db, ro);
        let mut g = pool.lock();
        if g.len() < POOL_SIZE {
            g.push(d);
        }
    }

    /// Values are never spliced into the SQL: `params` bind `$1 … $n` in text format.
    pub async fn bounded(
        &self,
        database: Option<&str>,
        sql: &str,
        params: Vec<Option<String>>,
        mode: Mode,
        limits: Limits,
        cancel: impl Future<Output = ()> + Send,
    ) -> Result<Outcome, Stop> {
        self.bounded_held(database, sql, params, mode, limits, cancel, None).await
    }

    pub async fn bounded_held(
        &self,
        database: Option<&str>,
        sql: &str,
        params: Vec<Option<String>>,
        mode: Mode,
        limits: Limits,
        cancel: impl Future<Output = ()> + Send,
        decide: Option<Decide>,
    ) -> Result<Outcome, Stop> {
        let dialect = self.dialect();
        let stmts = statements_in(sql, dialect);
        let db = database.unwrap_or(self.database()).to_string();
        let ro = self.read_only_for(&db);
        if let Some(r) = refusal_in(&stmts, params.len(), mode, ro, dialect) {
            return Err(Stop::Refused(r));
        }
        if mode == Mode::Rollback
            && let Some(r) = self.unrollable(sql)
        {
            return Err(Stop::Refused(r));
        }
        if mode == Mode::Read
            && let Some(r) = self.unheld_read_refusal(sql)
        {
            return Err(Stop::Refused(r));
        }
        if let Some(r) = self.pooler.and_then(|p| crate::pooler::session_state_refusal(sql, &p)) {
            return Err(Stop::Refused(r));
        }
        if mode != Mode::Read {
            if self.profile.read_only && self.engine().spec().db_in_name.is_some() {
                self.load_databases().await;
            }
            if let Some(r) = self.read_only_refusal(&db, sql) {
                return Err(Stop::Refused(r));
            }
        }
        // Files and code outside the database are an agent's at no level.
        if let Some(r) = guard::file_access(sql, dialect) {
            return Err(Stop::Refused(r));
        }
        // SQLite's readers run on connections that cannot write: a pool of their own. Where nothing
        // can roll a read back, so do those.
        let ro = ro || (mode == Mode::Read && (dialect == Dialect::Sqlite || !dialect.has_transactions()));
        let started = Instant::now();
        let mut d = tokio::select! {
            d = self.agent_take(&db, ro) => d.map_err(Stop::Other)?,
            _ = tokio::time::sleep(limits.timeout) => return Err(Stop::Timeout(limits.timeout)),
        };
        tokio::pin!(cancel);
        let ran = {
            let work = async {
                match &d {
                    Dedicated::Pg(p) => {
                        let shared = p.pooler.is_some_and(|x| x.per_transaction());
                        in_transaction(&p.client, &stmts, &params, mode, &limits, PgTx { read_only: mode == Mode::Read || ro, shared }).await
                    }
                    Dedicated::My(c) => my_in_transaction(c, &stmts, &params, mode, ro, &limits).await,
                    Dedicated::Ms(c) => ms_in_transaction(c, &stmts, &params, mode, &limits).await,
                    Dedicated::Lite(c) => lite_in_transaction(c, &stmts, &params, mode, &limits).await,
                    Dedicated::Duck(c) => duck_in_transaction(c, &stmts, &params, mode, &limits).await,
                    Dedicated::Ch(c) => ch_statements(c, &stmts, &params, mode, &limits).await,
                }
            };
            tokio::pin!(work);
            tokio::select! {
                r = &mut work => r,
                _ = tokio::time::sleep(limits.timeout + GRACE) => Err((Stop::Timeout(limits.timeout), false)),
                _ = &mut cancel => Err((Stop::Cancelled, false)),
            }
        };
        let elapsed = started.elapsed();
        let (r, reuse) = match ran {
            Ok((o, false)) => (Ok(o), false),
            Ok((mut o, true)) => {
                o.elapsed = elapsed;
                let mut cancelled = false;
                let commit = match (mode, decide) {
                    (Mode::Write, Some(f)) => {
                        let wait = f(&o);
                        tokio::select! {
                            ok = wait => ok,
                            _ = &mut cancel => {
                                cancelled = true;
                                false
                            }
                        }
                    }
                    (Mode::Write, None) => true,
                    _ => false,
                };
                match finish(&d, dialect, mode, commit).await {
                    Ok(()) => {
                        o.committed = mode == Mode::Autocommit || (mode == Mode::Write && commit);
                        // A session set up for one statement outside a transaction (its own statement_timeout) is not reused.
                        (if cancelled { Err(Stop::Cancelled) } else { Ok(o) }, mode != Mode::Autocommit)
                    }
                    Err((e, reuse)) => (Err(e), reuse),
                }
            }
            Err((e, reuse)) => (Err(e), reuse),
        };
        let r = match r {
            Ok(o) => Ok(o),
            // A cancel at the time limit (57014 in any language): the server's own
            // `statement_timeout` answered first.
            Err(Stop::Server(e)) if e.class(self.engine()) == ErrorClass::Cancelled && (e.message.contains("timeout") || elapsed >= limits.timeout) => Err(Stop::Timeout(limits.timeout)),
            Err(Stop::Server(e)) if e.class(self.engine()) == ErrorClass::Timeout => Err(Stop::Timeout(limits.timeout)),
            Err(e) => Err(e),
        };
        // A `USE` stays with the connection: the next call on this database must not find it there.
        if reuse && !stmts.iter().any(|s| used_database(s, dialect).is_some()) {
            self.agent_give(&db, ro, d);
        } else {
            if matches!(r, Err(Stop::Timeout(_) | Stop::Cancelled)) {
                let _ = tokio::time::timeout(Duration::from_secs(2), d.cancel()).await;
            }
            if let Some(p) = d.pg_mut() {
                p.notifications.close();
            }
            drop(d);
        }
        r
    }
}

async fn finish(d: &Dedicated, dialect: Dialect, mode: Mode, commit: bool) -> Result<(), Failed> {
    if mode == Mode::Autocommit || !dialect.has_transactions() {
        return Ok(());
    }
    let sql = if commit { dialect.commit() } else { dialect.rollback() };
    match d {
        Dedicated::Pg(p) => p.client.simple_query(sql).await.map(|_| ()).map_err(|e| (Stop::Server(error_info(&e)), !p.client.is_closed())),
        Dedicated::My(c) => c.execute(sql).await.map(|_| ()).map_err(|e| (Stop::Server(my::error_info_at(&e, sql)), !c.is_closed())),
        Dedicated::Ms(c) => c.execute(sql).await.map(|_| ()).map_err(|e| (Stop::Server(ms::error_info_at(&e, sql, 0)), !c.is_closed())),
        Dedicated::Lite(c) => c.execute(sql).await.map(|_| ()).map_err(|e| (Stop::Server(lite::error_info_at(&e, sql)), !c.is_closed())),
        Dedicated::Duck(c) => c.execute(sql).await.map(|_| ()).map_err(|e| (Stop::Server(duck::error_info_at(&e, sql)), !c.is_closed())),
        Dedicated::Ch(c) => c.execute(sql).await.map(|_| ()).map_err(|e| (Stop::Server(ch::error_info_at(&e, sql)), !c.is_closed())),
    }
}

/// ClickHouse has no transaction to run them in: a read is held to it by its read-only connection
/// (the server refuses anything else), a write is alone in its call (`Facts::outside_tx`) and has
/// been allowed by then. The caller's timer cancels.
async fn ch_statements(c: &Arc<ch::Client>, stmts: &[String], params: &[Option<String>], mode: Mode, limits: &Limits) -> Result<(Outcome, bool), Failed> {
    let mut out = Outcome::default();
    for (i, s) in stmts.iter().enumerate() {
        let last = i + 1 == stmts.len();
        let p = if last { params } else { &[] };
        let (cut0, t0) = (out.cut_cells, Instant::now());
        if mode == Mode::Read {
            out.start_part();
        }
        let r = ch_statement(c, s, p, mode, !last, limits, &mut out).await;
        if mode == Mode::Read && r.is_ok() {
            out.end_part(i, s, cut0, t0.elapsed());
        }
        match r {
            Ok(true) => {}
            Ok(false) => {
                let _ = tokio::time::timeout(Duration::from_secs(2), c.cancel()).await;
                return Ok((out, false));
            }
            Err(e) => return Err((e, true)),
        }
    }
    Ok((out, true))
}

async fn ch_statement(c: &Arc<ch::Client>, sql: &str, params: &[Option<String>], mode: Mode, more: bool, limits: &Limits, out: &mut Outcome) -> Result<bool, Stop> {
    let verb = guard::classify_in(sql, Dialect::ClickHouse).verb;
    let (cols, mut rows) = c.query(sql, params).await.map_err(|e| Stop::Server(ch::error_info_at(&e, sql)))?;
    let returns_rows = !cols.is_empty();
    if returns_rows {
        start_rows(&cols, mode, out);
    }
    let mut b = ChunkBuilder::new(&cols);
    loop {
        let next = match rows.next_into(&mut b).await {
            // What was read before it is still the answer, cut as a result past its bytes is.
            Err(ch::ChError::RowTooLong) if !out.rows.is_empty() => {
                out.truncated = Some(Truncation::Bytes);
                return Ok(false);
            }
            r => r.map_err(|e| Stop::Server(ch::error_info(&e)))?,
        };
        if !next {
            break;
        }
        let chunk = b.finish();
        // A cell that turned its column to text: the column is text from here on.
        let cols: Vec<ColumnInfo> = cols.iter().enumerate().map(|(i, c)| ColumnInfo { kind: b.kind(i), ..c.clone() }).collect();
        match keep_row(&cols, &chunk, mode, more, limits, out) {
            Keep::Yes | Keep::Skip => {}
            Keep::Stop => return Ok(false),
        }
    }
    let affected = rows.affected();
    out.statements.push(Done { verb, affected: if returns_rows && mode == Mode::Read { None } else { affected } });
    Ok(true)
}

/// DuckDB has no statement time limit either: the caller's timer interrupts. A read runs in a
/// READ ONLY transaction, rolled back.
async fn duck_in_transaction(c: &Arc<duck::Client>, stmts: &[String], params: &[Option<String>], mode: Mode, limits: &Limits) -> Result<(Outcome, bool), Failed> {
    if mode != Mode::Autocommit {
        let begin = Dialect::DuckDb.begin_for(mode == Mode::Read);
        c.execute(begin).await.map_err(|e| (Stop::Server(duck::error_info_at(&e, begin)), false))?;
    }
    let mut out = Outcome::default();
    for (i, s) in stmts.iter().enumerate() {
        let last = i + 1 == stmts.len();
        let p = if last { params } else { &[] };
        let (cut0, t0) = (out.cut_cells, Instant::now());
        if mode == Mode::Read {
            out.start_part();
        }
        let r = duck_statement(c, s, p, mode, !last, limits, &mut out).await;
        if mode == Mode::Read && r.is_ok() {
            out.end_part(i, s, cut0, t0.elapsed());
        }
        match r {
            Ok(true) => {}
            Ok(false) => {
                let _ = c.cancel().await;
                return Ok((out, false));
            }
            Err(e) => {
                let ok = mode == Mode::Autocommit || c.execute(Dialect::DuckDb.rollback()).await.is_ok();
                return Err((e, ok && !c.is_closed()));
            }
        }
    }
    Ok((out, true))
}

async fn duck_statement(c: &Arc<duck::Client>, sql: &str, params: &[Option<String>], mode: Mode, more: bool, limits: &Limits, out: &mut Outcome) -> Result<bool, Stop> {
    let verb = guard::classify_in(sql, Dialect::DuckDb).verb;
    let ((cols, _), mut rows) = c.stream(sql, params).await.map_err(|e| Stop::Server(duck::error_info_at(&e, sql)))?;
    let returns_rows = !cols.is_empty();
    if returns_rows {
        start_rows(&cols, mode, out);
    }
    let mut b = ChunkBuilder::new(&cols);
    loop {
        let next = rows.next_into(&mut b).await.map_err(|e| Stop::Server(duck::error_info(&e)))?;
        if !next {
            break;
        }
        let chunk = b.finish();
        let cols: Vec<ColumnInfo> = cols.iter().enumerate().map(|(i, c)| ColumnInfo { kind: b.kind(i), ..c.clone() }).collect();
        match keep_row(&cols, &chunk, mode, more, limits, out) {
            Keep::Yes | Keep::Skip => {}
            Keep::Stop => return Ok(false),
        }
    }
    let affected = rows.affected();
    out.statements.push(Done { verb, affected: if returns_rows && mode == Mode::Read { None } else { affected } });
    Ok(true)
}

/// SQLite has no statement time limit: the caller's timer interrupts. A read runs on a
/// connection that cannot write (opened read-only, `query_only`), inside a transaction that is
/// rolled back.
async fn lite_in_transaction(c: &Arc<lite::Client>, stmts: &[String], params: &[Option<String>], mode: Mode, limits: &Limits) -> Result<(Outcome, bool), Failed> {
    if mode != Mode::Autocommit {
        let begin = Dialect::Sqlite.begin();
        c.execute(begin).await.map_err(|e| (Stop::Server(lite::error_info_at(&e, begin)), false))?;
    }
    let mut out = Outcome::default();
    for (i, s) in stmts.iter().enumerate() {
        let last = i + 1 == stmts.len();
        let p = if last { params } else { &[] };
        let (cut0, t0) = (out.cut_cells, Instant::now());
        if mode == Mode::Read {
            out.start_part();
        }
        let r = lite_statement(c, s, p, mode, !last, limits, &mut out).await;
        if mode == Mode::Read && r.is_ok() {
            out.end_part(i, s, cut0, t0.elapsed());
        }
        match r {
            Ok(true) => {}
            Ok(false) => {
                let _ = c.cancel().await;
                return Ok((out, false));
            }
            Err(e) => {
                let ok = mode == Mode::Autocommit || c.execute(Dialect::Sqlite.rollback()).await.is_ok();
                return Err((e, ok && !c.is_closed()));
            }
        }
    }
    Ok((out, true))
}

async fn lite_statement(c: &Arc<lite::Client>, sql: &str, params: &[Option<String>], mode: Mode, more: bool, limits: &Limits, out: &mut Outcome) -> Result<bool, Stop> {
    let verb = guard::classify_in(sql, Dialect::Sqlite).verb;
    let ((cols, _), mut rows) = c.stream(sql, params, None).await.map_err(|e| Stop::Server(lite::error_info_at(&e, sql)))?;
    let returns_rows = !cols.is_empty();
    if returns_rows {
        start_rows(&cols, mode, out);
    }
    let mut b = ChunkBuilder::new(&cols);
    loop {
        let next = rows.next_into(&mut b).await.map_err(|e| Stop::Server(lite::error_info(&e)))?;
        if !next {
            break;
        }
        let chunk = b.finish();
        // A cell that turned its column to text: the column is text from here on.
        let cols: Vec<ColumnInfo> = cols.iter().enumerate().map(|(i, c)| ColumnInfo { kind: b.kind(i), ..c.clone() }).collect();
        match keep_row(&cols, &chunk, mode, more, limits, out) {
            Keep::Yes | Keep::Skip => {}
            Keep::Stop => return Ok(false),
        }
    }
    let affected = rows.affected();
    out.statements.push(Done { verb, affected: if returns_rows && mode == Mode::Read { None } else { affected } });
    Ok(true)
}

/// DDL commits on its own in MySQL: a write that changes the schema is not undone by a
/// ROLLBACK (the agents' tiers ask before running it).
///
/// `ro`: the database is read-only. MySQL's read-only session makes every transaction so; a
/// server that drops the setting (Vitess) still holds one begun READ ONLY.
async fn my_in_transaction(
    c: &Arc<my::Client>,
    stmts: &[String],
    params: &[Option<String>],
    mode: Mode,
    ro: bool,
    limits: &Limits,
) -> Result<(Outcome, bool), Failed> {
    let server = |e: mysql_async::Error, sql: &str| Stop::Server(my::error_info_at(&e, sql));
    if mode != Mode::Autocommit {
        let begin = Dialect::MySql.begin_for(mode == Mode::Read || ro);
        c.execute(begin).await.map_err(|e| (server(e, begin), false))?;
    }
    // MySQL bounds SELECTs (MAX_EXECUTION_TIME, ms), MariaDB every statement (seconds); the
    // local timer covers the rest.
    let ms = limits.timeout.as_millis().max(1);
    if c.execute(&format!("SET SESSION MAX_EXECUTION_TIME = {ms}")).await.is_err() {
        let _ = c.execute(&format!("SET SESSION max_statement_time = {}", (ms as f64 / 1000.0).max(0.001))).await;
    }
    let mut out = Outcome::default();
    for (i, s) in stmts.iter().enumerate() {
        let last = i + 1 == stmts.len();
        let p = if last { params } else { &[] };
        let (cut0, t0) = (out.cut_cells, Instant::now());
        if mode == Mode::Read {
            out.start_part();
        }
        let r = my_statement(c, s, p, mode, !last, limits, &mut out).await;
        if mode == Mode::Read && r.is_ok() {
            out.end_part(i, s, cut0, t0.elapsed());
        }
        match r {
            Ok(true) => {}
            Ok(false) => {
                let _ = tokio::time::timeout(Duration::from_secs(2), c.cancel()).await;
                return Ok((out, false));
            }
            Err(e) => {
                let ok = mode == Mode::Autocommit || c.execute(Dialect::MySql.rollback()).await.is_ok();
                return Err((e, ok && !c.is_closed()));
            }
        }
    }
    Ok((out, true))
}

async fn my_statement(c: &Arc<my::Client>, sql: &str, params: &[Option<String>], mode: Mode, more: bool, limits: &Limits, out: &mut Outcome) -> Result<bool, Stop> {
    let verb = guard::classify_in(sql, Dialect::MySql).verb;
    let (cols, mut rows) = my::query(c, sql, params).await.map_err(|e| Stop::Server(my::error_info_at(&e, sql)))?;
    let returns_rows = !cols.is_empty();
    if returns_rows {
        start_rows(&cols, mode, out);
    }
    let mut b = ChunkBuilder::new(&cols);
    loop {
        let next = rows.next_into(&mut b).await.map_err(|e| Stop::Server(my::error_info(&e)))?;
        if !next {
            break;
        }
        let chunk = b.finish();
        match keep_row(&cols, &chunk, mode, more, limits, out) {
            Keep::Yes | Keep::Skip => {}
            Keep::Stop => return Ok(false),
        }
    }
    let affected = rows.affected();
    out.statements.push(Done { verb, affected: if returns_rows && mode == Mode::Read { None } else { affected } });
    Ok(true)
}

/// SQL Server has neither read-only transactions nor a statement time limit: a read is a read
/// by the client-side check, and the local timer cancels and drops the connection.
async fn ms_in_transaction(c: &Arc<ms::Client>, stmts: &[String], params: &[Option<String>], mode: Mode, limits: &Limits) -> Result<(Outcome, bool), Failed> {
    let server = |e: ms::MsError, sql: &str| Stop::Server(ms::error_info_at(&e, sql, 0));
    if mode != Mode::Autocommit {
        let begin = Dialect::TSql.begin_for(mode == Mode::Read);
        c.execute(begin).await.map_err(|e| (server(e, begin), false))?;
    }
    let mut out = Outcome::default();
    for (i, s) in stmts.iter().enumerate() {
        let last = i + 1 == stmts.len();
        let p = if last { params } else { &[] };
        if mode == Mode::Read {
            out.start_part();
        }
        match ms_statement(c, i, s, p, mode, !last, limits, &mut out).await {
            Ok(true) => {}
            Ok(false) => {
                let _ = tokio::time::timeout(Duration::from_secs(2), c.cancel()).await;
                return Ok((out, false));
            }
            Err(e) => {
                let ok = c.execute(Dialect::TSql.rollback_if_open()).await.is_ok();
                return Err((e, ok && !c.is_closed()));
            }
        }
    }
    Ok((out, true))
}

async fn ms_statement(c: &Arc<ms::Client>, ix: usize, sql: &str, params: &[Option<String>], mode: Mode, more: bool, limits: &Limits, out: &mut Outcome) -> Result<bool, Stop> {
    let verb = guard::classify_in(sql, Dialect::TSql).verb;
    let (mut cols, mut rows) = ms::query(c, sql, params).await.map_err(|e| Stop::Server(ms::error_info_at(&e, sql, 0)))?;
    let returns_rows = !cols.is_empty();
    let read = mode == Mode::Read;
    let several = read && crate::sql::split::may_return_more(sql);
    let label = |k: usize| if several { crate::sql::split::result_statement(sql, k).unwrap_or(sql).to_string() } else { sql.to_string() };
    let mut n = 0u64;
    let mut k = 0;
    let affected = 'sets: loop {
        let (cut0, t0) = (out.cut_cells, Instant::now());
        if !cols.is_empty() {
            start_rows(&cols, mode, out);
        }
        let mut b = ChunkBuilder::new(&cols);
        while rows.next_into(&mut b).await.map_err(|e| Stop::Server(ms::error_info(&e)))? {
            n += 1;
            let chunk = b.finish();
            match keep_row(&cols, &chunk, mode, more || several, limits, out) {
                Keep::Yes => {}
                Keep::Skip if read => break,
                Keep::Skip => {}
                Keep::Stop => {
                    if read {
                        out.end_part(ix, &label(k), cut0, t0.elapsed());
                    }
                    return Ok(false);
                }
            }
        }
        match rows.next_result().await {
            ms::Next::Columns(next) if read => {
                out.end_part(ix, &label(k), cut0, t0.elapsed());
                out.start_part();
                cols = rows.columns(&next);
                k += 1;
            }
            ms::Next::Columns(_) => loop {
                match rows.next_result().await {
                    ms::Next::Columns(_) => continue,
                    ms::Next::End { affected, .. } => break 'sets affected,
                    ms::Next::Fail(e) | ms::Next::Error(e) => return Err(Stop::Server(ms::error_info_at(&e, sql, 0))),
                }
            },
            ms::Next::End { affected, .. } => {
                if read {
                    out.end_part(ix, &label(k), cut0, t0.elapsed());
                }
                break affected;
            }
            ms::Next::Fail(e) | ms::Next::Error(e) => return Err(Stop::Server(ms::error_info_at(&e, sql, 0))),
        }
    };
    out.statements.push(Done { verb, affected: if returns_rows { (mode != Mode::Read).then_some(n) } else { affected } });
    Ok(true)
}

fn start_rows(cols: &[ColumnInfo], mode: Mode, out: &mut Outcome) {
    out.columns = cols.iter().map(|c| Column { name: c.name.clone(), type_name: c.type_name.clone() }).collect();
    out.rows.clear();
    out.truncated = None;
    if mode != Mode::Read {
        out.bytes = 0;
    }
}

enum Keep {
    Yes,
    Skip,
    Stop,
}

fn stops(mode: Mode, why: Truncation, more: bool) -> bool {
    mode == Mode::Read && (why == Truncation::Bytes || !more)
}

fn keep_row(cols: &[ColumnInfo], chunk: &crate::result::Chunk, mode: Mode, more: bool, limits: &Limits, out: &mut Outcome) -> Keep {
    let past = |out: &mut Outcome, why: Truncation| {
        let why = *out.truncated.get_or_insert(why);
        if stops(mode, why, more) { Keep::Stop } else { Keep::Skip }
    };
    if out.truncated.is_some() || out.rows.len() >= limits.max_rows {
        return past(out, Truncation::Rows);
    }
    let mut vals = Vec::with_capacity(cols.len());
    let mut size = 0;
    for (i, c) in cols.iter().enumerate() {
        let (v, n, cut) = json_cell(c.kind, chunk.cell(0, i), limits.max_cell);
        size += n;
        out.cut_cells += cut as usize;
        vals.push(v);
    }
    if out.bytes + size > limits.max_bytes && !out.rows.is_empty() {
        return past(out, Truncation::Bytes);
    }
    out.bytes += size;
    out.rows.push(vals);
    Keep::Yes
}

type Failed = (Stop, bool);

/// How an agent's transaction opens. What the server must enforce is set on the transaction
/// itself: through a pooler per transaction (`shared`) the session holds nothing, and a direct
/// session's default_transaction_read_only is any statement's to change.
#[derive(Clone, Copy)]
struct PgTx {
    read_only: bool,
    shared: bool,
}

async fn in_transaction(client: &Client, stmts: &[String], params: &[Option<String>], mode: Mode, limits: &Limits, tx: PgTx) -> Result<(Outcome, bool), Failed> {
    begin(client, mode, limits, tx).await?;
    let mut out = Outcome::default();
    for (i, s) in stmts.iter().enumerate() {
        let last = i + 1 == stmts.len();
        let p = if last { params } else { &[] };
        let (cut0, t0) = (out.cut_cells, Instant::now());
        if mode == Mode::Read {
            out.start_part();
        }
        let r = statement(client, s, p, mode, !last, limits, tx, &mut out).await;
        if mode == Mode::Read && r.is_ok() {
            out.end_part(i, s, cut0, t0.elapsed());
        }
        match r {
            Ok(true) => {}
            Ok(false) => return Ok((out, false)),
            Err(e) => {
                let ok = mode == Mode::Autocommit || client.simple_query(Dialect::Postgres.rollback()).await.is_ok();
                return Err((e, ok && !client.is_closed()));
            }
        }
    }
    Ok((out, true))
}

async fn begin(client: &Client, mode: Mode, limits: &Limits, tx: PgTx) -> Result<(), Failed> {
    if mode == Mode::Autocommit {
        // Shared, a session's timeout would reach other clients: the caller's timer cancels.
        if !tx.shared {
            let timeout = format!("SET statement_timeout = {}", limits.timeout.as_millis().max(1));
            let _ = client.simple_query(&timeout).await;
        }
        return Ok(());
    }
    let d = Dialect::Postgres;
    let begin = match d.pin_read_only() {
        Some(pin) if tx.read_only => format!("{}; {pin}", d.begin_for(true)),
        _ => d.begin().to_string(),
    };
    let begin = begin.as_str();
    client.simple_query(begin).await.map_err(|e| (Stop::Server(error_info(&e)), false))?;
    if tx.shared {
        client.simple_query(crate::pooler::AGENT_TX_LOCALS).await.map_err(|e| (Stop::Server(error_info(&e)), false))?;
    }
    let timeout = format!("SET LOCAL statement_timeout = {}", limits.timeout.as_millis().max(1));
    if client.simple_query(&timeout).await.is_err() {
        client.simple_query(Dialect::Postgres.rollback()).await.map_err(|e| (Stop::Server(error_info(&e)), false))?;
        client.simple_query(begin).await.map_err(|e| (Stop::Server(error_info(&e)), false))?;
    }
    Ok(())
}

async fn statement(client: &Client, sql: &str, params: &[Option<String>], mode: Mode, more: bool, limits: &Limits, tx: PgTx, out: &mut Outcome) -> Result<bool, Stop> {
    let verb = crate::guard::classify(sql).verb;
    let stmt = match client.describe(sql).await {
        Ok(s) => s,
        // Statements the server cannot prepare run through the simple protocol (text results).
        // A failed prepare aborted the transaction: start it again first. Never with parameters.
        Err(e) if e.as_db_error().is_some() && params.is_empty() && out.statements.is_empty() && !crate::backend::pg::several_commands(&e) => {
            if mode != Mode::Autocommit {
                client.simple_query(Dialect::Postgres.rollback()).await.map_err(|e| Stop::Server(error_info(&e)))?;
                begin(client, mode, limits, tx).await.map_err(|(e, _)| e)?;
            }
            return simple(client, sql, verb, mode, more, limits, out).await.map_err(|e2| {
                let first = error_info(&e);
                if e2.as_db_error().is_some_and(|d| Some(d.code().code()) == first.code.as_deref()) { Stop::Server(first) } else { Stop::Server(error_info(&e2)) }
            });
        }
        Err(e) => return Err(Stop::Server(error_info(&e))),
    };
    if params.is_empty() && crate::backend::pg::needs_text(&stmt) {
        return simple(client, sql, verb, mode, more, limits, out).await.map_err(|e| Stop::Server(error_info(&e)));
    }
    let cols = column_infos(&stmt);
    let types: Vec<Type> = stmt.columns().iter().map(|c| c.type_().clone()).collect();
    let storage: Vec<Storage> = cols.iter().map(|c| c.kind.storage()).collect();
    let binds: Vec<TextParam> = params.iter().map(|p| TextParam(p.as_deref())).collect();
    let stream = client.query_raw(&stmt, binds.iter().map(|p| p as &(dyn ToSql + Sync))).await.map_err(|e| Stop::Server(error_info(&e)))?;
    futures_util::pin_mut!(stream);
    let returns_rows = !cols.is_empty();
    if returns_rows {
        start_rows(&cols, mode, out);
    }
    let mut b = ChunkBuilder::new(&cols);
    while let Some(row) = stream.as_mut().next().await {
        let row = row.map_err(|e| Stop::Server(error_info(&e)))?;
        if let Some(why) = out.truncated {
            if stops(mode, why, more) {
                return Ok(false);
            }
            continue;
        }
        if out.rows.len() >= limits.max_rows {
            out.truncated = Some(Truncation::Rows);
            if stops(mode, Truncation::Rows, more) {
                return Ok(false);
            }
            continue;
        }
        push_row(&mut b, &row, &types, &storage);
        let chunk = b.finish();
        let mut vals = Vec::with_capacity(cols.len());
        let mut size = 0;
        for (i, c) in cols.iter().enumerate() {
            let (v, n, cut) = json_cell(c.kind, chunk.cell(0, i), limits.max_cell);
            size += n;
            out.cut_cells += cut as usize;
            vals.push(v);
        }
        if out.bytes + size > limits.max_bytes && !out.rows.is_empty() {
            out.truncated = Some(Truncation::Bytes);
            if stops(mode, Truncation::Bytes, more) {
                return Ok(false);
            }
            continue;
        }
        out.bytes += size;
        out.rows.push(vals);
    }
    let affected = stream.rows_affected();
    out.statements.push(Done { verb, affected: if returns_rows && mode == Mode::Read { None } else { affected } });
    Ok(true)
}

async fn simple(client: &Client, sql: &str, verb: String, mode: Mode, more: bool, limits: &Limits, out: &mut Outcome) -> Result<bool, tokio_postgres::Error> {
    let stream = client.simple_query_raw(sql).await?;
    futures_util::pin_mut!(stream);
    let mut affected = None;
    let mut first = true;
    while let Some(m) = stream.as_mut().next().await {
        match m? {
            SimpleQueryMessage::RowDescription(cols) if first => {
                out.columns = cols.iter().map(|c| Column { name: c.name().to_string(), type_name: "text".into() }).collect();
            }
            SimpleQueryMessage::Row(r) if first => {
                if out.truncated.is_some() || out.rows.len() >= limits.max_rows {
                    let why = *out.truncated.get_or_insert(Truncation::Rows);
                    if stops(mode, why, more) {
                        return Ok(false);
                    }
                    continue;
                }
                let mut vals = Vec::with_capacity(r.len());
                let mut size = 0;
                for i in 0..r.len() {
                    let (v, n, cut) = match r.get(i) {
                        Some(s) => json_cell(Kind::Text, Cell::Text(s), limits.max_cell),
                        None => (Value::Null, 4, false),
                    };
                    size += n;
                    out.cut_cells += cut as usize;
                    vals.push(v);
                }
                if out.bytes + size > limits.max_bytes && !out.rows.is_empty() {
                    out.truncated = Some(Truncation::Bytes);
                    if stops(mode, Truncation::Bytes, more) {
                        return Ok(false);
                    }
                    continue;
                }
                out.bytes += size;
                out.rows.push(vals);
            }
            SimpleQueryMessage::CommandComplete(n) => {
                if first {
                    affected = Some(n);
                }
                first = false;
            }
            _ => {}
        }
    }
    let has_rows = !out.columns.is_empty();
    out.statements.push(Done { verb, affected: if has_rows && mode == Mode::Read { None } else { affected } });
    Ok(true)
}

pub fn outcome_of(rs: &crate::result::ResultSet, limits: &Limits) -> Outcome {
    let snap = rs.snapshot();
    let st = rs.status();
    let mut out = Outcome::default();
    let cols: Vec<ColumnInfo> = rs.columns().map(|c| c.as_ref().clone()).unwrap_or_default();
    out.columns = cols.iter().map(|c| Column { name: c.name.clone(), type_name: c.type_name.clone() }).collect();
    let n = snap.source_rows();
    for r in 0..n {
        if out.rows.len() >= limits.max_rows {
            out.truncated = Some(Truncation::Rows);
            break;
        }
        let mut vals = Vec::with_capacity(cols.len());
        let mut size = 0;
        for (i, c) in cols.iter().enumerate() {
            let (v, len, cut) = json_cell(c.kind, snap.source_cell(r, i), limits.max_cell);
            size += len;
            out.cut_cells += cut as usize;
            vals.push(v);
        }
        if out.bytes + size > limits.max_bytes && !out.rows.is_empty() {
            out.truncated = Some(Truncation::Bytes);
            break;
        }
        out.bytes += size;
        out.rows.push(vals);
    }
    let unfinished = st.phase.is_some_and(|p| !p.is_finished()) || st.partial;
    if out.truncated.is_none() && unfinished && !cols.is_empty() {
        out.truncated = Some(Truncation::Rows);
    }
    let verb = crate::sql::statement_verb(&rs.sql);
    out.statements.push(Done { verb, affected: if cols.is_empty() { st.affected } else { None } });
    out.elapsed = st.elapsed.unwrap_or_default();
    out
}

pub fn outcome_of_parts(sets: &[(usize, String, Arc<crate::result::ResultSet>)], limits: &Limits) -> Outcome {
    let mut out = Outcome::default();
    for (statement, sql, rs) in sets {
        let left = limits.max_bytes.saturating_sub(out.bytes);
        if left == 0 {
            break;
        }
        let o = outcome_of(rs, &Limits { max_bytes: left, ..*limits });
        out.parts.push(Part {
            statement: *statement,
            sql: sql.clone(),
            columns: o.columns.clone(),
            rows: o.rows.clone(),
            truncated: o.truncated,
            cut_cells: o.cut_cells,
            elapsed: o.elapsed,
        });
        out.bytes += o.bytes;
        out.cut_cells += o.cut_cells;
        out.elapsed += o.elapsed;
        out.statements.extend(o.statements);
        (out.columns, out.rows, out.truncated) = (o.columns, o.rows, o.truncated);
        if o.truncated == Some(Truncation::Bytes) {
            break;
        }
    }
    out
}

pub fn json_cell(kind: Kind, cell: Cell<'_>, max_cell: usize) -> (Value, usize, bool) {
    match cell {
        Cell::Null => (Value::Null, 4, false),
        Cell::Bool(b) => (Value::Bool(b), 5, false),
        Cell::Int(v) if !matches!(kind, Kind::Date | Kind::Time | Kind::Timestamp | Kind::TimestampTz) => (Value::from(v), 20, false),
        Cell::Float(_) | Cell::Int(_) => {
            let text = format_cell(kind, cell).map(|s| s.into_owned()).unwrap_or_default();
            if matches!(cell, Cell::Float(_))
                && let Some(n) = text.parse::<f64>().ok().and_then(serde_json::Number::from_f64)
            {
                return (Value::Number(n), text.len(), false);
            }
            let n = text.len() + 2;
            (Value::String(text), n, false)
        }
        Cell::Text(s) => {
            if kind == Kind::Json
                && s.len() <= max_cell
                && let Ok(v) = serde_json::from_str::<Value>(s)
            {
                return (v, s.len(), false);
            }
            let (text, cut) = cut(s, max_cell);
            let n = text.len() + 2;
            (Value::String(text), n, cut)
        }
    }
}

fn cut(s: &str, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((at, _)) => (format!("{}…", &s[..at]), true),
        None => (s.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_is_followed() {
        let d = Dialect::ClickHouse;
        assert_eq!(used_database("USE system", d).as_deref(), Some("system"));
        assert_eq!(used_database("/* x */ use `a b` ;", d).as_deref(), Some("a b"));
        assert_eq!(used_database("USE DATABASE d", d).as_deref(), Some("d"));
        assert_eq!(used_database("USE database", d).as_deref(), Some("database"), "a database named so");
        assert_eq!(used_database("USE database;", d).as_deref(), Some("database"));
        assert_eq!(used_database(r"USE `a\x62`", d).as_deref(), Some("ab"), "as the server reads the name");
        assert_eq!(used_database("SELECT 1", d), None);
        assert_eq!(used_database("USE [ro db]", Dialect::TSql).as_deref(), Some("ro db"));
        assert_eq!(used_database("USE a\nINSERT INTO t VALUES (1)\nUSE b", Dialect::TSql).as_deref(), Some("b"), "no ; between them");
        assert_eq!(used_database("USE `ro`; INSERT INTO t VALUES (1)", Dialect::MySql).as_deref(), Some("ro"));
    }

    #[test]
    fn what_a_write_reaches_by_name() {
        let ch = |sql: &str| databases_written(sql, Dialect::ClickHouse, 2, true);
        let my = |sql: &str| databases_written(sql, Dialect::MySql, 2, true);
        let ms = |sql: &str| databases_written(sql, Dialect::TSql, 3, false);
        let duck = |sql: &str| databases_written(sql, Dialect::DuckDb, 2, false);
        for sql in [
            "INSERT INTO ro.items VALUES (1)",
            "INSERT INTO TABLE `ro`.`items` VALUES (1)",
            "ALTER TABLE ro.items DELETE WHERE 1",
            "ALTER TABLE \"ro\" . items UPDATE a = 1 WHERE 1",
            "OPTIMIZE TABLE ro.items",
            "RENAME TABLE w.a TO ro.b",
            "EXCHANGE TABLES w.a AND ro.b",
            "CREATE TABLE ro.t ENGINE = Memory AS SELECT 1",
            "CREATE MATERIALIZED VIEW w.mv TO ro.t AS SELECT 1",
            "ALTER TABLE w.t MOVE PARTITION p TO TABLE ro.t",
            "DROP DATABASE IF EXISTS ro",
            "TRUNCATE ALL TABLES FROM ro",
            "TRUNCATE ALL TABLES FROM IF EXISTS ro",
            "INSERT INTO w.t SELECT 1 FROM x UPDATE ro.t SET a = 1",
            "WITH 1 AS y INSERT INTO ro.t SELECT y",
        ] {
            assert!(ch(sql).contains(&"ro".to_string()), "{sql}: {:?}", ch(sql));
        }
        // What is only read of it.
        for sql in [
            "INSERT INTO w.t SELECT * FROM ro.s",
            "INSERT INTO w.t (a) SELECT replace(a, 'x', 'y') FROM ro.s",
            "INSERT INTO w.t SELECT a FROM (SELECT a FROM ro.s) x",
            "CREATE TABLE w.t ENGINE = Memory AS SELECT * FROM ro.s",
            "ALTER TABLE w.t DELETE WHERE id IN (SELECT id FROM ro.s)",
            "CREATE VIEW w.v AS SELECT * FROM ro.s",
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET a = 1",
        ] {
            assert!(!ch(sql).contains(&"ro".to_string()), "{sql}: {:?}", ch(sql));
        }
        assert_eq!(my("UPDATE w.t JOIN ro.s ON w.t.a = ro.s.a SET ro.s.b = 1"), ["w", "ro", "w", "ro", "ro"], "either side of a join can be written");
        assert!(my("DELETE FROM ro.t").contains(&"ro".to_string()));
        assert!(my("DROP SCHEMA ro").contains(&"ro".to_string()));
        assert!(my("REPLACE INTO ro.t VALUES (1)").contains(&"ro".to_string()));
        assert!(my("CALL ro.p()").contains(&"ro".to_string()));
        assert!(my("UPDATE w.t SET a = (SELECT b FROM ro.s LIMIT 1)").iter().all(|n| n == "w"));
        assert!(my("UPDATE w.t JOIN (SELECT a FROM ro.s) x USING (a) SET b = 1").contains(&"ro".to_string()), "a derived table can be what is written to");

        for sql in [
            "INSERT INTO ro.dbo.t VALUES (1)",
            "UPDATE [ro].dbo.t SET a = 1",
            "DELETE FROM ro..t",
            "SELECT * INTO ro.dbo.t FROM w.dbo.s",
            "EXEC ro.dbo.p",
            "MERGE ro.dbo.t USING w.dbo.s ON 1 = 1 WHEN MATCHED THEN DELETE;",
            "WITH c AS (SELECT * FROM ro.dbo.t) UPDATE c SET a = 1",
            "DELETE x FROM (SELECT * FROM ro.dbo.t) x",
            "UPDATE x SET a = 1 FROM ro.dbo.t x",
            "ALTER DATABASE ro SET READ_ONLY",
            "INSERT INTO link.ro.dbo.t VALUES (1)",
        ] {
            assert!(ms(sql).contains(&"ro".to_string()), "{sql}: {:?}", ms(sql));
        }
        assert!(ms("INSERT INTO dbo.t VALUES (1)").is_empty(), "a schema, not a database");
        assert!(ms("INSERT INTO w.dbo.t SELECT * FROM ro.dbo.s").iter().all(|n| n == "w"));

        assert!(duck("INSERT INTO ro.main.t VALUES (1)").contains(&"ro".to_string()));
        assert!(duck("INSERT INTO ro.t VALUES (1)").contains(&"ro".to_string()));
        assert!(duck("DROP DATABASE ro").contains(&"ro".to_string()));
    }

    #[test]
    fn json_cells() {
        assert_eq!(json_cell(Kind::Int, Cell::Null, 10).0, Value::Null);
        assert_eq!(json_cell(Kind::Text, Cell::Text(""), 10).0, Value::String(String::new()), "empty is not NULL");
        assert_eq!(json_cell(Kind::Int, Cell::Int(42), 10).0, Value::from(42));
        assert_eq!(json_cell(Kind::Bool, Cell::Bool(true), 10).0, Value::Bool(true));
        assert_eq!(json_cell(Kind::Float4, Cell::Float(0.1f32 as f64), 10).0.to_string(), "0.1");
        assert_eq!(json_cell(Kind::Float, Cell::Float(f64::NAN), 10).0, Value::String("NaN".into()));
        assert_eq!(json_cell(Kind::Numeric, Cell::Text("12.3400"), 10).0, Value::String("12.3400".into()), "numeric stays exact");
        assert_eq!(json_cell(Kind::Json, Cell::Text("{\"a\":[1]}"), 100).0, serde_json::json!({"a": [1]}));
        assert_eq!(json_cell(Kind::Date, Cell::Int(0), 10).0, Value::String("2000-01-01".into()));
        let (v, _, c) = json_cell(Kind::Text, Cell::Text("café au lait"), 4);
        assert_eq!((v, c), (Value::String("café…".into()), true));
    }

    #[test]
    fn outcome_of_a_result() {
        use crate::result::{ChunkBuilder, ResultSet};
        let cols = vec![
            ColumnInfo { name: "id".into(), type_name: "int4".into(), type_oid: 23, kind: Kind::Int },
            ColumnInfo { name: "t".into(), type_name: "text".into(), type_oid: 25, kind: Kind::Text },
        ];
        let mut b = ChunkBuilder::new(&cols);
        for i in 0..5 {
            b.push_i64(0, i);
            if i == 2 {
                b.push_null(1);
            } else {
                b.push_text(1, &"x".repeat(10));
            }
            b.end_row();
        }
        let rs = ResultSet::from_chunk("SELECT id, t FROM x", cols, b.finish());
        let o = outcome_of(&rs, &Limits { max_rows: 3, ..Limits::default() });
        assert_eq!((o.rows.len(), o.truncated), (3, Some(Truncation::Rows)));
        assert_eq!(o.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "t"]);
        assert_eq!(o.rows[2], vec![Value::from(2), Value::Null]);
        assert_eq!((o.statements[0].verb.as_str(), o.statements[0].affected), ("SELECT", None));
        let o = outcome_of(&rs, &Limits { max_rows: 10, max_cell: 4, ..Limits::default() });
        assert_eq!((o.rows.len(), o.truncated, o.cut_cells), (5, None, 4));
        assert_eq!(o.rows[0][1], Value::String("xxxx…".into()));
        let live = ResultSet::new("SELECT 1");
        assert_eq!(outcome_of(&live, &Limits::default()).truncated, None, "no columns yet: nothing to cut");
    }

    #[test]
    fn outcome_of_several_results() {
        use crate::result::{ChunkBuilder, ResultSet};
        let cols = vec![ColumnInfo { name: "t".into(), type_name: "text".into(), type_oid: 25, kind: Kind::Text }];
        let set = |sql: &str, n: usize| {
            let mut b = ChunkBuilder::new(&cols);
            for _ in 0..n {
                b.push_text(0, &"x".repeat(98));
                b.end_row();
            }
            ResultSet::from_chunk(sql, cols.clone(), b.finish())
        };
        let sets = vec![(0, "SELECT a".to_string(), set("SELECT a", 5)), (1, "SELECT b".to_string(), set("SELECT b", 2)), (2, "SELECT c".to_string(), set("SELECT c", 9))];
        let o = outcome_of_parts(&sets, &Limits { max_rows: 3, ..Limits::default() });
        let shape: Vec<(usize, &str, usize, Option<Truncation>)> = o.parts.iter().map(|p| (p.statement, p.sql.as_str(), p.rows.len(), p.truncated)).collect();
        assert_eq!(shape, [(0, "SELECT a", 3, Some(Truncation::Rows)), (1, "SELECT b", 2, None), (2, "SELECT c", 3, Some(Truncation::Rows))]);
        assert_eq!((o.rows.len(), o.total_rows(), o.statements.len()), (3, 8, 3), "the last part is also the outcome's rows");
        let o = outcome_of_parts(&sets, &Limits { max_rows: 10, max_bytes: 450, ..Limits::default() });
        let shape: Vec<(usize, usize, Option<Truncation>)> = o.parts.iter().map(|p| (p.statement, p.rows.len(), p.truncated)).collect();
        assert_eq!(shape, [(0, 4, Some(Truncation::Bytes))]);
        let o = outcome_of_parts(&sets, &Limits { max_rows: 10, max_bytes: 650, ..Limits::default() });
        let shape: Vec<(usize, usize, Option<Truncation>)> = o.parts.iter().map(|p| (p.statement, p.rows.len(), p.truncated)).collect();
        assert_eq!(shape, [(0, 5, None), (1, 1, Some(Truncation::Bytes))]);
    }

    #[test]
    fn refusals() {
        let s = |x: &str| statements(x);
        assert_eq!(s("select 1; ; select 2;"), vec!["select 1", "select 2"]);
        assert_eq!(refusal(&s("SELECT 1; SELECT 2; TABLE t"), 0, Mode::Read, false), None);
        assert!(refusal(&s("SELECT 1; DELETE FROM t; SELECT 2"), 0, Mode::Read, false).unwrap().starts_with("statement 2: DELETE is not a read"));
        assert!(refusal(&s("SELECT $1; SELECT 2"), 1, Mode::Read, false).unwrap().contains("single statement"));
        assert!(refusal(&s("DELETE FROM t WHERE id = 1"), 0, Mode::Read, false).unwrap().contains("not a read"));
        assert!(refusal(&s(""), 0, Mode::Write, false).unwrap().contains("empty"));
        assert!(refusal(&s("BEGIN; INSERT INTO t VALUES (1)"), 0, Mode::Write, false).unwrap().contains("transaction control"));
        assert!(refusal(&s("CREATE INDEX CONCURRENTLY i ON t (x)"), 0, Mode::Write, false).unwrap().contains("inside a transaction"));
        assert!(refusal(&s("INSERT INTO t VALUES (1)"), 0, Mode::Write, true).unwrap().contains("read-only connection"));
        assert!(refusal(&s("INSERT INTO t VALUES ($1); SELECT 1"), 1, Mode::Write, false).unwrap().contains("single statement"));
        assert_eq!(refusal(&s("INSERT INTO t VALUES (1); UPDATE t SET a = 2 WHERE b"), 0, Mode::Write, false), None);
        assert_eq!(refusal(&s("SELECT * FROM t WHERE id = $1"), 1, Mode::Read, false), None);
        assert_eq!(refusal(&s("VACUUM t"), 0, Mode::Autocommit, false), None);
        assert_eq!(refusal(&s("CREATE INDEX CONCURRENTLY i ON t (x)"), 0, Mode::Autocommit, false), None);
        assert!(refusal(&s("VACUUM a; VACUUM b"), 0, Mode::Autocommit, false).unwrap().contains("alone"));
        assert!(refusal(&s("COMMIT"), 0, Mode::Autocommit, false).unwrap().contains("transaction control"));
        assert!(refusal(&s("CREATE INDEX CONCURRENTLY i ON t (x)"), 0, Mode::Autocommit, true).unwrap().contains("read-only"));
    }
}
