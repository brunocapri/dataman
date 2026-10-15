//! The server's activity: activity_sessions, activity_locks (read), cancel_backend and
//! terminate_backend (write).

use diskman_core::activity::{self, Signal};
use diskman_core::guard::WriteClass;
use serde_json::{Value, json};

use super::{Builtin, confirm_prop, table};
use crate::policy::{self, Need, Target, Verdict};
use crate::tool::{Args, BoxFut, Call, Schema, ToolError};

fn conn_only() -> Schema {
    Schema::new().prop("connection", true, json!({"type": "string", "description": "Connection name (or id) from list_connections."}))
}

fn pid_schema() -> Schema {
    confirm_prop(conn_only().prop("pid", true, json!({"type": "integer", "minimum": 1, "description": "The backend's process id (from activity_sessions)."})))
}

pub fn tools() -> Vec<Builtin> {
    vec![
        Builtin {
            name: "activity_sessions",
            description: "Server sessions (pg_stat_activity). Rows: [pid, user, database, application, client, state, wait, xact_age_s, query_age_s, blocked_by, query] with ages in seconds and the query cut to 500 characters; this server's own MCP backends are left out. Idle sessions only with include_idle, background processes only with include_background.",
            schema: conn_only()
                .prop("include_idle", false, json!({"type": "boolean", "description": "Also idle sessions."}))
                .prop("include_background", false, json!({"type": "boolean", "description": "Also background processes (autovacuum, walwriter…)."})),
            need: Some(Need::Read),
            run: sessions,
        },
        Builtin {
            name: "activity_locks",
            description: "Who blocks whom (pg_blocking_pids, pg_locks), as a tree: rows [depth, pid, waits_for, relation, waited_s, blocker_holds, sessions_behind] where depth 0 is a blocker that waits on nobody and each deeper row waits on the row above it; waits_for is the lock mode and type it asked for. Empty when nothing waits.",
            schema: conn_only(),
            need: Some(Need::Read),
            run: locks,
        },
        Builtin {
            name: "cancel_backend",
            description: "Cancels the statement backend pid is running (pg_cancel_backend); the session stays. Needs the connection's MCP tier write or full; on a production connection it waits for the user's Allow in Diskman. Returns whether the server signalled it.",
            schema: pid_schema(),
            need: Some(Need::Write),
            run: cancel,
        },
        Builtin {
            name: "terminate_backend",
            description: "Ends backend pid's session (pg_terminate_backend); its open transaction rolls back. Destructive: needs the connection's MCP tier full and waits for the user's Allow in Diskman. Returns whether the server signalled it.",
            schema: pid_schema(),
            need: Some(Need::Write),
            run: terminate,
        },
    ]
}

fn failed(e: anyhow::Error) -> ToolError {
    ToolError::Failed(format!("{e:#}"))
}

fn cut(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s.to_string(),
    }
}

fn sessions(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        let o = cx.open(&a, Need::Read).await?;
        let rows = o.conn.activity_sessions(&o.database).await.map_err(failed)?;
        let (idle, bg) = (a.bool("include_idle"), a.bool("include_background"));
        let out: Vec<Vec<Value>> = rows
            .into_iter()
            .filter(|s| !s.is_self && !o.conn.owns_backend(s.pid))
            .filter(|s| (idle || !s.is_idle()) && (bg || !s.is_background()))
            .map(|s| {
                vec![
                    json!(s.pid),
                    json!(s.user),
                    json!(s.db),
                    json!(s.app),
                    json!(s.client),
                    json!(s.state),
                    json!(s.wait),
                    json!(s.xact_age),
                    json!(s.query_age),
                    json!(s.blocked_by),
                    json!(s.query.as_deref().map(|q| cut(q, 500))),
                ]
            })
            .collect();
        cx.note_rows(out.len());
        Ok(table(&["pid", "user", "database", "application", "client", "state", "wait", "xact_age_s", "query_age_s", "blocked_by", "query"], out))
    })
}

fn locks(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        let o = cx.open(&a, Need::Read).await?;
        let edges = o.conn.activity_locks(&o.database).await.map_err(failed)?;
        let tree = activity::blocking_tree(&edges);
        let out: Vec<Vec<Value>> = tree
            .iter()
            .map(|r| {
                let e = r.edge.as_ref();
                let waits = e.map(|e| format!("{} · {}", e.mode.clone().unwrap_or_default(), e.locktype.clone().unwrap_or_default()));
                vec![
                    json!(r.depth),
                    json!(r.pid),
                    json!(waits),
                    json!(e.and_then(|e| e.relation.clone())),
                    json!(e.and_then(|e| e.wait)),
                    json!(e.and_then(|e| e.held.clone())),
                    json!(r.behind),
                ]
            })
            .collect();
        cx.note_rows(out.len());
        Ok(table(&["depth", "pid", "waits_for", "relation", "waited_s", "blocker_holds", "sessions_behind"], out))
    })
}

/// What a signal is to the tiers: a cancel stops a statement (a safe write: tier write, asks on
/// production); a terminate ends a session and rolls back its work (destructive: tier full and
/// the user's Allow).
pub fn signal_class(s: Signal) -> WriteClass {
    match s {
        Signal::Cancel => WriteClass::SafeWrite,
        Signal::Terminate => WriteClass::Destructive { reasons: vec!["terminate a session".into()] },
    }
}

fn signal(cx: &Call, a: Args, s: Signal) -> BoxFut<'_, Result<Value, ToolError>> {
    Box::pin(async move {
        let p = cx.target(&a, Need::Write)?;
        let pid = a.u64_or("pid", 0) as i32;
        let sql = s.sql(pid);
        cx.note_sql(&sql);
        // Refused before connecting when the tier refuses it, or when it needs a human and no
        // app runs.
        let class = signal_class(s);
        match policy::write_verdict(cx.hub.settings().mcp_enabled, Target::of(&p), &class) {
            Verdict::Refuse(e) => return Err(ToolError::Refused(e)),
            Verdict::Ask(r) if cx.hub.front().is_none() => return Err(ToolError::Refused(policy::needs_app(&r))),
            _ => {}
        }
        let o = cx.open(&a, Need::Write).await?;
        if o.conn.owns_backend(pid) {
            return Err(ToolError::Refused("that backend is Diskman's own".into()));
        }
        cx.gate(&o, &class, &sql, None).await?;
        let ok = o.conn.signal_backend(&o.database, pid, s).await.map_err(failed)?;
        Ok(json!({"pid": pid, "signalled": ok}))
    })
}

fn cancel(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    signal(cx, a, Signal::Cancel)
}

fn terminate(cx: &Call, a: Args) -> BoxFut<'_, Result<Value, ToolError>> {
    signal(cx, a, Signal::Terminate)
}
