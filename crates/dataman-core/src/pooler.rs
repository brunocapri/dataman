//! A connection pooler between Dataman and a PostgreSQL server (PgBouncer, Supavisor, a cloud's
//! proxy). In transaction mode it gives each transaction whichever server connection is free:
//! nothing a client leaves on its connection between transactions (a `SET`, a temporary table, a
//! `LISTEN`, a named statement) stays with that client, and the next client of the same server
//! connection finds it instead.
//!
//! That a pooler is there is read from the connection: the process ID in BackendKeyData is the
//! server's own `pg_backend_pid()` on a direct connection, and a number the pooler made up
//! otherwise. Which mode it pools in is not something a client can ask; the host names that
//! say it are in `HINTS`, and any other pooler counts as transaction mode, where nothing is
//! assumed to last past a transaction.

use crate::sql::{Dialect, TokenKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolMode {
    /// A server connection per client for as long as the client stays: sessions behave.
    Session,
    /// A server connection per transaction: session state neither lasts nor stays private.
    Transaction,
}

impl PoolMode {
    pub fn label(self) -> &'static str {
        match self {
            PoolMode::Session => "session mode",
            PoolMode::Transaction => "transaction mode",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pooler {
    pub name: &'static str,
    pub mode: PoolMode,
}

impl Pooler {
    /// Each transaction may run on another server connection, shared with other clients.
    pub fn per_transaction(&self) -> bool {
        self.mode == PoolMode::Transaction
    }

    pub fn label(&self) -> String {
        format!("{}, {}", self.name, self.mode.label())
    }
}

/// A host that says which pooler answers on it. The first that matches wins.
struct Hint {
    /// The host ends with this (case-insensitive).
    suffix: &'static str,
    /// And has this in it (empty: anything).
    has: &'static str,
    port: Option<u16>,
    pooler: Pooler,
}

const PGBOUNCER: &str = "PgBouncer";
const UNKNOWN: Pooler = Pooler { name: "a connection pooler", mode: PoolMode::Transaction };

const HINTS: &[Hint] = &[
    // Neon: `ep-…-pooler.region.aws.neon.tech` is PgBouncer in transaction mode; the endpoint
    // without `-pooler` is Neon's proxy, one server connection per client.
    Hint { suffix: ".neon.tech", has: "-pooler.", port: None, pooler: Pooler { name: PGBOUNCER, mode: PoolMode::Transaction } },
    Hint { suffix: ".neon.tech", has: "", port: None, pooler: Pooler { name: "Neon's proxy", mode: PoolMode::Session } },
    // Supabase's shared pooler (Supavisor): 6543 pools transactions, 5432 sessions.
    Hint { suffix: ".pooler.supabase.com", has: "", port: Some(6543), pooler: Pooler { name: "Supavisor", mode: PoolMode::Transaction } },
    Hint { suffix: ".pooler.supabase.com", has: "", port: Some(5432), pooler: Pooler { name: "Supavisor", mode: PoolMode::Session } },
    // A project's dedicated pooler: PgBouncer on 6543 next to the database.
    Hint { suffix: ".supabase.co", has: "", port: Some(6543), pooler: Pooler { name: PGBOUNCER, mode: PoolMode::Transaction } },
    // RDS Proxy keeps a client on one server connection once it sets session state.
    Hint { suffix: ".rds.amazonaws.com", has: ".proxy-", port: None, pooler: Pooler { name: "RDS Proxy", mode: PoolMode::Session } },
];

/// Set first in an agent's transaction through a pooler per transaction: the server connection
/// may carry what another client of the pooler set on it, and the guard lexes strings with
/// standard_conforming_strings on and resolves names by the login's own search path (what the
/// catalog read, `backend::pg::search_path`).
pub(crate) const AGENT_TX_LOCALS: &str = "SET LOCAL standard_conforming_strings = on; SET LOCAL search_path TO DEFAULT";

/// Poolers that serve many projects on one host and name the project in the user, after a dot
/// (`postgres.<project-ref>` on Supabase's shared pooler).
const TENANT_IN_USER: &[&str] = &[".pooler.supabase.com"];

/// The user to sign in as `role` on the host the profile `host`/`user` reaches: the role as it
/// is, or with the profile user's tenant where the pooler routes by it.
pub fn login_user(host: &str, user: &str, role: &str) -> String {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    match user.rsplit_once('.') {
        Some((_, tenant)) if !tenant.is_empty() && TENANT_IN_USER.iter().any(|s| h.ends_with(s)) => format!("{role}.{tenant}"),
        _ => role.to_string(),
    }
}

/// What is between Dataman and the server, from the BackendKeyData process ID and the server's
/// own `pg_backend_pid()` on the same connection.
pub(crate) fn detect(host: &str, port: u16, key_pid: i32, backend_pid: i32) -> Option<Pooler> {
    if key_pid == backend_pid {
        return None;
    }
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let hint = HINTS.iter().find(|h| host.ends_with(h.suffix) && host.contains(h.has) && h.port.is_none_or(|p| p == port));
    Some(hint.map_or(UNKNOWN, |h| h.pooler))
}

/// Why `sql` must not run through a pooler that hands out a server connection per transaction:
/// what it sets up would stay on a server connection other clients get next, and this tab or
/// agent would not find it there on its next transaction.
pub fn session_state_refusal(sql: &str, pooler: &Pooler) -> Option<String> {
    if !pooler.per_transaction() {
        return None;
    }
    let d = Dialect::Postgres;
    for s in crate::sql::split_in(sql, d) {
        let text = &sql[s.range];
        let toks: Vec<_> = crate::sql::tokenize_in(text, d).into_iter().filter(|t| !t.kind.is_trivia()).collect();
        let words: Vec<String> = toks.iter().map(|t| if matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) { t.text(text).to_ascii_uppercase() } else { t.text(text).to_string() }).collect();
        if let Some(what) = session_state(&words) {
            return Some(format!("{what} would stay on a server connection {} gives to other clients next, and not reach this tab's next transaction", pooler.label()));
        }
    }
    None
}

fn session_state(w: &[String]) -> Option<String> {
    let at = |i: usize, s: &str| w.get(i).is_some_and(|x| x == s);
    let first = w.first()?.as_str();
    match first {
        // SET LOCAL, SET TRANSACTION and SET CONSTRAINTS end with the transaction.
        "SET" if !(at(1, "LOCAL") || at(1, "TRANSACTION") || at(1, "CONSTRAINTS")) => return Some("SET".into()),
        "RESET" | "LISTEN" | "PREPARE" | "EXECUTE" | "DEALLOCATE" | "LOAD" => return Some(first.to_string()),
        "DECLARE" if w.windows(2).any(|p| p[0] == "WITH" && p[1] == "HOLD") => return Some("a cursor WITH HOLD".into()),
        "CREATE" => {
            let is_temp = |i: usize| at(i, "TEMP") || at(i, "TEMPORARY");
            let temp = is_temp(1) || ((at(1, "GLOBAL") || at(1, "LOCAL")) && is_temp(2)) || (at(1, "OR") && at(2, "REPLACE") && is_temp(3));
            let dropped = w.windows(3).any(|p| p[0] == "ON" && p[1] == "COMMIT" && p[2] == "DROP");
            if temp && !dropped {
                return Some("a temporary object (without ON COMMIT DROP)".into());
            }
        }
        _ => {}
    }
    for (i, x) in w.iter().enumerate() {
        if !at(i + 1, "(") {
            continue;
        }
        match x.as_str() {
            "PG_ADVISORY_LOCK" | "PG_ADVISORY_LOCK_SHARED" | "PG_TRY_ADVISORY_LOCK" | "PG_TRY_ADVISORY_LOCK_SHARED" => return Some(format!("{} (a session lock)", x.to_ascii_lowercase())),
            "SET_CONFIG" if !last_argument_is(w, i + 1, "TRUE") => return Some("set_config(…, false)".into()),
            _ => {}
        }
    }
    None
}

/// The last top-level argument of the call whose `(` is at `open` is the single word `want`.
fn last_argument_is(w: &[String], open: usize, want: &str) -> bool {
    let mut depth = 0usize;
    let mut last_comma = open;
    for (i, x) in w.iter().enumerate().skip(open) {
        match x.as_str() {
            "(" | "[" => depth += 1,
            ")" | "]" => {
                depth -= 1;
                if depth == 0 {
                    return i == last_comma + 2 && w[last_comma + 1] == want;
                }
            }
            "," if depth == 1 => last_comma = i,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_reads_the_key_and_the_host() {
        assert_eq!(detect("db.example.com", 5432, 41, 41), None, "the server's own process: direct");
        let p = detect("ep-cool-darkness-123456-pooler.us-east-2.aws.neon.tech", 5432, 7, 41).unwrap();
        assert_eq!((p.name, p.mode), ("PgBouncer", PoolMode::Transaction));
        assert_eq!(detect("ep-cool-darkness-123456.us-east-2.aws.neon.tech", 5432, 7, 41).unwrap().mode, PoolMode::Session);
        assert_eq!(detect("aws-0-eu-west-1.pooler.supabase.com", 6543, 7, 41).unwrap().mode, PoolMode::Transaction);
        assert_eq!(detect("AWS-0-eu-west-1.pooler.supabase.com.", 5432, 7, 41).unwrap().mode, PoolMode::Session);
        assert_eq!(detect("db.abcdefgh.supabase.co", 6543, 7, 41).unwrap().name, "PgBouncer");
        assert_eq!(detect("app.proxy-abc.eu-west-1.rds.amazonaws.com", 5432, 7, 41).unwrap().mode, PoolMode::Session);
        assert_eq!(detect("pool.internal", 6432, 7, 41), Some(UNKNOWN), "an unknown pooler keeps nothing past a transaction");
        assert_eq!(detect("x.neon.tech.evil.example", 5432, 7, 41), Some(UNKNOWN), "a hint is the host's end");
        assert_eq!(detect("db.abcdefgh.supabase.co", 5432, 7, 41), Some(UNKNOWN), "a proxy on the direct port is unknown");
    }

    #[test]
    fn a_tenant_pooler_names_the_project_in_the_user() {
        assert_eq!(login_user("aws-0-eu-west-1.pooler.supabase.com", "postgres.abcdefgh", "dataman_agent_x"), "dataman_agent_x.abcdefgh");
        assert_eq!(login_user("db.abcdefgh.supabase.co", "postgres", "dataman_agent_x"), "dataman_agent_x");
        assert_eq!(login_user("db.internal", "app.ops", "dataman_agent_x"), "dataman_agent_x", "a dot elsewhere is the name's");
    }

    #[test]
    fn session_state_is_refused_per_transaction_only() {
        let tx = Pooler { name: "PgBouncer", mode: PoolMode::Transaction };
        let session = Pooler { name: "Supavisor", mode: PoolMode::Session };
        for sql in [
            "SET search_path = app",
            "set session search_path to app",
            "SET ROLE reporting",
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
            "RESET search_path",
            "LISTEN jobs",
            "PREPARE q AS SELECT 1",
            "EXECUTE q",
            "DEALLOCATE ALL",
            "LOAD 'auto_explain'",
            "DECLARE c CURSOR WITH HOLD FOR SELECT 1",
            "CREATE TEMP TABLE t (a int)",
            "create temporary table t (a int) on commit delete rows",
            "CREATE GLOBAL TEMPORARY TABLE t (a int)",
            "CREATE TEMP VIEW v AS SELECT 1",
            "CREATE OR REPLACE TEMP VIEW v AS SELECT 1",
            "SELECT pg_advisory_lock(42)",
            "SELECT 1; SELECT pg_try_advisory_lock(1, 2)",
            "SELECT set_config('search_path', 'app', false)",
            "SELECT set_config('search_path', 'app', is_local)",
        ] {
            assert!(session_state_refusal(sql, &tx).is_some(), "{sql}");
            assert_eq!(session_state_refusal(sql, &session), None, "{sql}");
        }
        for sql in [
            "SELECT 1",
            "SET LOCAL search_path = app",
            "SET TRANSACTION READ ONLY",
            "SET CONSTRAINTS ALL DEFERRED",
            "BEGIN",
            "CREATE TEMP TABLE t (a int) ON COMMIT DROP",
            "CREATE TABLE temp (a int)",
            "DECLARE c CURSOR FOR SELECT 1",
            "SELECT pg_advisory_xact_lock(42)",
            "SELECT set_config('search_path', 'app', true)",
            "SELECT set_config('a', f('x', false), TRUE)",
            "SELECT 'SET search_path = x'",
            "UNLISTEN *",
        ] {
            assert_eq!(session_state_refusal(sql, &tx), None, "{sql}");
        }
    }
}
