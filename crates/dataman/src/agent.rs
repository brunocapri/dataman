//! Agent tabs: where an agent works visibly. `open_query` (and `run_query` / `execute` while
//! "Agent queries in tabs" is on) runs in one tab per connection and client, reused while it is
//! free: its SQL goes into the editor, a read runs in that tab's own session (a read-only
//! transaction of its own, never the user's tabs or transactions) and the agent gets the bounded
//! view of the result while the whole result stays in the tab (several reads: a result tab each,
//! every one in the agent's answer); a write runs on the agent's own
//! backend (held for an approval when it needs one) and the tab shows it running, then its
//! counts, its RETURNING rows and what became of it. A tab the user is busy in (an edited text,
//! a run of their own, an open transaction, pending edits) is left alone: the agent gets another.
//! The tab carries a small agent glyph; its statements are in the history as the agent's.
//! A draft (`open_query` with `run: false`) only puts its SQL in the tab, on the connection and
//! database, with nothing from earlier runs left on screen: nothing runs until the user runs it,
//! and then it runs as any statement of theirs (the guard, their read-only setting, their
//! history). A draft the user has not run yet is theirs: only another draft may replace it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use gpui::{App, Context, Entity, Task, Window};
use dataman_core::bounded::{self, Outcome, Stop};
use dataman_core::result::{ChunkBuilder, ColumnInfo, ErrorInfo, Phase};
use dataman_core::value::Kind;
use dataman_core::{Connection, HistoryEntry, ResultSet};
use dataman_mcp::{Drafted, TabDraft, TabRead, TabWrite, WriteShown};
use tokio::sync::oneshot;

use crate::app::Workspace;
use crate::conns::ConnState;
use crate::mcp::{AppFront, FrontMsg};
use crate::tab::QueryTab;

/// Whose agent tab a tab is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTab {
    pub client: String,
    pub connection_id: String,
    /// What the agent last put in the editor (another text: the user took the tab over).
    pub text: String,
}

/// What the tools send the agent tabs.
pub(crate) enum TabMsg {
    Read { req: TabRead, reply: oneshot::Sender<Result<Outcome, Stop>> },
    ShowWrite { req: TabWrite, reply: oneshot::Sender<Option<u64>> },
    WriteDone { tab: u64, sql: String, shown: WriteShown },
    Draft { req: TabDraft, reply: oneshot::Sender<Result<Drafted, String>> },
}

/// How long a read waits for its connection to open in the app (a write's tab waits less: the
/// write itself does not wait for its tab).
const CONNECT_FOR_READ: Duration = Duration::from_secs(60);
const CONNECT_FOR_WRITE: Duration = Duration::from_secs(10);

pub(crate) fn tabs_ready() -> bool {
    true
}

pub(crate) fn run_read(f: &AppFront, req: TabRead) -> BoxFuture<'static, Result<Outcome, Stop>> {
    let (reply, rx) = oneshot::channel();
    let sent = f.send(FrontMsg::Tab(TabMsg::Read { req, reply }));
    Box::pin(async move {
        if !sent {
            return Err(Stop::Refused("Dataman is closing".into()));
        }
        rx.await.unwrap_or_else(|_| Err(Stop::Refused("the agent tab went away".into())))
    })
}

pub(crate) fn show_write(f: &AppFront, req: TabWrite) -> BoxFuture<'static, Option<u64>> {
    let (reply, rx) = oneshot::channel();
    let sent = f.send(FrontMsg::Tab(TabMsg::ShowWrite { req, reply }));
    Box::pin(async move { if sent { rx.await.ok().flatten() } else { None } })
}

pub(crate) fn write_done(f: &AppFront, tab: u64, sql: String, shown: WriteShown) {
    f.send(FrontMsg::Tab(TabMsg::WriteDone { tab, sql, shown }));
}

pub(crate) fn draft(f: &AppFront, req: TabDraft) -> BoxFuture<'static, Result<Drafted, String>> {
    let (reply, rx) = oneshot::channel();
    let sent = f.send(FrontMsg::Tab(TabMsg::Draft { req, reply }));
    Box::pin(async move {
        if !sent {
            return Err("Dataman is closing".into());
        }
        rx.await.unwrap_or_else(|_| Err(dataman_mcp::hub::DRAFTS_NEED_APP.into()))
    })
}

pub(crate) fn arrived(m: TabMsg, cx: &mut App) {
    let Some((ws, window)) = crate::mcp::workspace(cx) else { return };
    let _ = window.update(cx, |_, window, cx| {
        let _ = ws.update(cx, |w, cx| match m {
            TabMsg::Read { req, reply } => w.agent_read(req, reply, window, cx),
            TabMsg::ShowWrite { req, reply } => w.agent_show_write(req, reply, window, cx),
            TabMsg::WriteDone { tab, sql, shown } => w.agent_write_done(tab, sql, shown, cx),
            TabMsg::Draft { req, reply } => w.agent_draft(req, reply, window, cx),
        });
    });
}

/// What a read waits for.
enum Wait {
    /// The results are ready (the run's statement count).
    Ready(usize),
    Timeout,
    Cancelled,
    /// The tab was closed.
    Gone,
}

/// A JSON value as the grid's text (`None`: NULL).
fn text_of(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

impl Workspace {
    /// The app's connection for an agent's work, connected (its saved password, the Keychain)
    /// when it is not yet, within `wait`.
    fn agent_connection(&mut self, id: &str, wait: Duration, cx: &mut Context<Self>) -> Task<Result<Arc<Connection>, String>> {
        if let Some(c) = self.store.read(cx).connection(id) {
            return Task::ready(Ok(c));
        }
        if self.store.read(cx).profile(id).is_none() {
            return Task::ready(Err("the connection is not in Dataman".into()));
        }
        self.store.update(cx, |s, cx| s.connect(id, None, cx));
        let (store, id) = (self.store.clone(), id.to_string());
        cx.spawn(async move |_, cx| {
            let until = Instant::now() + wait;
            while Instant::now() < until {
                match store.read_with(cx, |s, _| s.states.get(&id).cloned()) {
                    Some(ConnState::Connected(c)) => return Ok(c),
                    Some(ConnState::Failed(e)) => return Err(e),
                    None => return Err("not connected".into()),
                    Some(ConnState::Connecting) => {}
                }
                cx.background_executor().timer(Duration::from_millis(100)).await;
            }
            Err("timed out connecting in Dataman".into())
        })
    }

    /// The client's agent tab for `conn`: a free one, else the empty tab in front, else a new
    /// one opened behind the tab in front (the user's focus stays where it is). A `draft` may
    /// take a tab holding a draft not run yet; nothing else may.
    fn agent_tab(&mut self, conn: Arc<Connection>, client: &str, database: &str, draft: bool, window: &mut Window, cx: &mut Context<Self>) -> Entity<QueryTab> {
        let id = conn.profile.id.clone();
        let mine = |t: &QueryTab| t.agent.as_ref().is_some_and(|a| a.client == client && a.connection_id == id);
        let found = self.tabs.iter().find(|t| mine(t.read(cx)) && t.read(cx).agent_free(draft, cx)).cloned();
        let tab = match found {
            Some(t) => t,
            None => {
                let pristine = self.active_tab().filter(|t| {
                    let t = t.read(cx);
                    t.agent.is_none() && !t.has_run && t.monitor.is_none() && t.table.is_none() && t.editor.read(cx).text().trim().is_empty()
                });
                let tab = match pristine.cloned() {
                    Some(t) => t,
                    None => {
                        let (prev, focus) = (self.active, window.focused(cx));
                        let t = self.new_tab(window, cx);
                        self.active = prev.min(self.tabs.len() - 1);
                        self.sync_bound_views(window, cx);
                        if let Some(f) = focus {
                            window.focus(&f, cx);
                        }
                        t
                    }
                };
                tab.update(cx, |t, _| t.agent = Some(AgentTab { client: client.to_string(), connection_id: id.clone(), text: String::new() }));
                tab
            }
        };
        tab.update(cx, |t, cx| {
            if !t.connection().is_some_and(|c| Arc::ptr_eq(c, &conn)) {
                t.set_connection(Some(conn), cx);
            }
            if t.database() != database {
                t.set_database(database.to_string(), cx);
            }
        });
        cx.notify();
        tab
    }

    fn agent_read(&mut self, req: TabRead, reply: oneshot::Sender<Result<Outcome, Stop>>, window: &mut Window, cx: &mut Context<Self>) {
        let connect = self.agent_connection(&req.connection_id, CONNECT_FOR_READ, cx);
        cx.spawn_in(window, async move |w, cx| {
            let conn = match connect.await {
                Ok(c) => c,
                Err(e) => {
                    let _ = reply.send(Err(Stop::Other(anyhow::anyhow!("cannot connect in Dataman: {e}"))));
                    return;
                }
            };
            let limits = req.limits;
            let started = w.update_in(cx, |w, window, cx| {
                let tab = w.agent_tab(conn, &req.client, &req.database, false, window, cx);
                let rs = tab.update(cx, |t, cx| t.run_agent(req.sql.clone(), req.params.clone(), &req.client, req.title.clone(), limits.max_rows + 1, cx));
                rs.map(|_| tab.downgrade())
            });
            let Ok(Some(tab)) = started else {
                let _ = reply.send(Err(Stop::Refused("the agent tab did not start the statement".into())));
                return;
            };
            // Every statement's result ready (enough rows, the end, a pause, a failure that
            // stops the script), the agent's time limit, or its cancel.
            let deadline = Instant::now() + limits.timeout;
            let (what, results) = loop {
                if req.cancel.is_cancelled() {
                    break (Wait::Cancelled, Vec::new());
                }
                if Instant::now() >= deadline {
                    break (Wait::Timeout, Vec::new());
                }
                match tab.read_with(cx, |t, _| t.agent_results(limits.max_rows).map(|r| (r, t.run_statements()))) {
                    Ok(Some((r, n))) => break (Wait::Ready(n), r),
                    Ok(None) => {}
                    Err(_) => break (Wait::Gone, Vec::new()),
                }
                cx.background_executor().timer(Duration::from_millis(20)).await;
            };
            let answer = match what {
                Wait::Timeout | Wait::Cancelled => {
                    let _ = tab.update(cx, |t, cx| t.cancel(cx));
                    Err(if matches!(what, Wait::Timeout) { Stop::Timeout(limits.timeout) } else { Stop::Cancelled })
                }
                Wait::Gone => Err(Stop::Refused("the agent tab went away".into())),
                Wait::Ready(statements) => {
                    let failed = results.iter().find(|(_, _, rs)| rs.phase() == Phase::Failed);
                    if let Some((k, _, rs)) = failed {
                        let mut e = rs.status().error.unwrap_or_default();
                        if statements > 1 {
                            e.message = format!("statement {}: {}", k + 1, e.message);
                        }
                        Err(Stop::Server(e))
                    } else if results.iter().any(|(_, _, rs)| rs.phase() == Phase::Cancelled) {
                        Err(Stop::Cancelled)
                    } else {
                        Ok(bounded::outcome_of_parts(&results, &limits))
                    }
                }
            };
            let _ = reply.send(answer);
            let Some(rs) = results.last().map(|(_, _, rs)| rs.clone()) else { return };
            // A result past what the tab loads ahead pauses inside its read-only transaction:
            // the rows loaded stay, the rest is read and dropped, and the transaction ends.
            let mut rx = rs.subscribe();
            while !rs.phase().is_finished() {
                if rs.phase() == Phase::Paused {
                    rs.stop_fetching();
                    break;
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn agent_show_write(&mut self, req: TabWrite, reply: oneshot::Sender<Option<u64>>, window: &mut Window, cx: &mut Context<Self>) {
        let connect = self.agent_connection(&req.connection_id, CONNECT_FOR_WRITE, cx);
        cx.spawn_in(window, async move |w, cx| {
            let Ok(conn) = connect.await else {
                let _ = reply.send(None);
                return;
            };
            let id = w
                .update_in(cx, |w, window, cx| {
                    let tab = w.agent_tab(conn, &req.client, &req.database, false, window, cx);
                    let rs = ResultSet::placeholder(req.sql.clone(), Some(&req.client));
                    tab.update(cx, |t, cx| t.show_agent(req.sql.clone(), rs, req.title.clone(), cx));
                    tab.entity_id().as_u64()
                })
                .ok();
            let _ = reply.send(id);
        })
        .detach();
    }

    /// A draft: its SQL in the client's agent tab, on the connection (connected in the app if
    /// it is not yet) and the database; nothing runs. The answer names the tab.
    fn agent_draft(&mut self, req: TabDraft, reply: oneshot::Sender<Result<Drafted, String>>, window: &mut Window, cx: &mut Context<Self>) {
        let connect = self.agent_connection(&req.connection_id, CONNECT_FOR_READ, cx);
        cx.spawn_in(window, async move |w, cx| {
            let conn = match connect.await {
                Ok(c) => c,
                Err(e) => {
                    let _ = reply.send(Err(format!("cannot connect in Dataman: {e}")));
                    return;
                }
            };
            let done = w.update_in(cx, |w, window, cx| {
                let tab = w.agent_tab(conn, &req.client, &req.database, true, window, cx);
                tab.update(cx, |t, cx| {
                    t.draft_agent(&req.sql, req.title.clone(), cx);
                    Drafted { tab: t.title(cx).to_string(), database: t.database().to_string() }
                })
            });
            let _ = reply.send(done.map_err(|_| dataman_mcp::hub::DRAFTS_NEED_APP.to_string()));
        })
        .detach();
    }

    /// Ends the write a tab shows: its rows, counts and what became of it, and the history.
    fn agent_write_done(&mut self, tab: u64, sql: String, shown: WriteShown, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter().find(|t| t.entity_id().as_u64() == tab).cloned() else { return };
        let (rs, conn, database) = {
            let t = tab.read(cx);
            (t.agent_pending.clone(), t.connection().cloned(), t.database().to_string())
        };
        let Some(rs) = rs.filter(|r| !r.phase().is_finished()) else { return };
        let (affected, ms, outcome, error) = match shown {
            WriteShown::Ran { outcome, note } => {
                let cols: Vec<ColumnInfo> = outcome.columns.iter().map(|c| ColumnInfo { name: c.name.clone(), type_name: c.type_name.clone(), type_oid: 0, kind: Kind::Text }).collect();
                let chunk = (!cols.is_empty()).then(|| {
                    let mut b = ChunkBuilder::new(&cols);
                    for r in &outcome.rows {
                        for (i, v) in r.iter().enumerate().take(cols.len()) {
                            match text_of(v) {
                                Some(s) => b.push_text(i, &s),
                                None => b.push_null(i),
                            }
                        }
                        b.end_row();
                    }
                    b.finish()
                });
                let mut messages: Vec<String> = outcome.statements.iter().map(|d| match d.affected {
                    Some(n) => format!("{} {n}", d.verb),
                    None => d.verb.clone(),
                }).collect();
                messages.extend(note.clone());
                let affected = Some(outcome.affected());
                let error = note.map(|n| ErrorInfo { message: n, ..Default::default() });
                rs.finish_with((!cols.is_empty()).then_some(cols), chunk, affected, messages, error.clone(), outcome.elapsed);
                (affected, outcome.elapsed.as_secs_f64() * 1e3, if error.is_some() { dataman_core::Outcome::Error } else { dataman_core::Outcome::Ok }, error.map(|e| e.message))
            }
            WriteShown::Failed(msg) => {
                rs.finish_with(None, None, None, Vec::new(), Some(ErrorInfo { message: msg.clone(), ..Default::default() }), Duration::ZERO);
                (None, 0.0, dataman_core::Outcome::Error, Some(msg))
            }
        };
        if let Some(conn) = conn {
            conn.record_history(&HistoryEntry {
                ts: chrono::Utc::now(),
                database,
                sql,
                duration_ms: ms,
                rows: affected,
                outcome,
                error,
                source: Some(dataman_core::history::AGENT.to_string()),
            });
        }
        cx.notify();
    }
}
