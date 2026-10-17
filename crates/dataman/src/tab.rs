//! A query tab: an editor, its own server session, and the result grid.
//!
//! A script keeps every statement's result: the strip under the editor has one tab per
//! statement that returned rows, a row count, an error or a cancel, and one "messages" tab for
//! the commands without rows (`dataman_core::batch`). The grid, the result bar, find, export and
//! grid edits work on the tab on screen; each result keeps its own grid state (selection,
//! scroll, widths; the sort is in the result) while another is shown.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{Context, Entity, EventEmitter, SharedString, Task, Window, prelude::*};
use dataman_core::batch::{self, Key, Outcome, Views};
use dataman_core::complete::Definition;
use dataman_core::dataview::{Filter, Op, TableView};
use dataman_core::fknav::ForeignKeys;
use dataman_core::guard;
use dataman_core::result::{ErrorInfo, Phase, SortDir};
use dataman_core::{Connection, ResultSet, Session, TxEnd, TxState, TxStatus};

use crate::editor::{CodeEditor, Diagnostic, EditorEvent, Mark, Stmt};
use crate::edit_ui::{EditEvent, GridEdit};
use crate::grid::{GridView, ResultGrid, TableMode};
use crate::params_ui::{ParamEvent, ParamPrompt};
use crate::plan::PlanView;
use dataman_core::explain;
use crate::table_view::TableData;

/// How long typing pauses before SQL Server parses the text (`schedule_parse_check`).
const PARSE_DELAY: std::time::Duration = std::time::Duration::from_millis(600);

pub enum TabEvent {
    /// A statement finished (the history panel refreshes).
    Finished,
    /// Something would replace the result while grid edits are pending: the workspace asks
    /// what to do with them (`QueryTab::pending_run`).
    ConfirmDiscard,
    /// A run waits for the user (`QueryTab::confirm`): dangerous statements, or a failed
    /// transaction.
    Confirm,
    /// COMMIT or ROLLBACK ended (the statements the transaction had, for the notice).
    TxEnded { result: Result<TxEnd, String>, statements: u32 },
    /// The parameter prompt is up (`params`): it takes the keys.
    Params,
    /// It closed (run or cancelled): the keys go back.
    ParamsClosed,
    /// F12 / ⌘-click (reveal in the explorer), ⌥F12 (`peek`: open the definition).
    Definition { def: Definition, peek: bool },
    /// A restored tab ran something while its connection (this profile id) is not connected:
    /// the workspace connects it; the run waits (`QueryTab::adopt`).
    Connect(String),
    /// Its pending grid edits were discarded to close it: the workspace closes it now.
    Close,
    /// Its pending grid edits were discarded to quit: the workspace quits now.
    Quit,
}

/// A run waiting for its parameters' values.
pub struct PendingParams {
    pub prompt: Entity<ParamPrompt>,
    stmts: Vec<Stmt>,
    failed_check: bool,
}

/// A run that waits for the user's answer.
pub enum TabConfirm {
    /// Statements the guard rules ask about (`dataman_core::guard`): Run (⌘↩) or Cancel.
    Guard { stmts: Vec<Stmt>, findings: Vec<guard::Finding> },
    /// The transaction failed and the server refuses everything until it ends: Rollback & run
    /// (⌘↩), Rollback (⌘⌫) or Cancel.
    Failed { stmts: Vec<Stmt> },
    /// The activity monitor's Cancel / Terminate of a session, or its Reset (⌘↩ or esc).
    Monitor(crate::activity::Pending),
}

/// The answer to a [`TabConfirm`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmChoice {
    Cancel,
    /// Guard: run as typed. Failed: roll back, then run.
    Run,
    /// Failed: roll back only.
    Rollback,
}

/// A statement waiting for the user to decide about the pending grid edits it would throw
/// away: "Discard & run" or "Cancel".
pub struct PendingRun {
    pub edits: usize,
    /// What goes ahead once they are discarded: "run", "switch".
    pub verb: &'static str,
    run: Box<dyn FnOnce(&mut QueryTab, &mut Context<QueryTab>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemState {
    Pending,
    Running,
    Ok,
    Failed,
    Cancelled,
}

/// One statement of the last run.
#[derive(Clone)]
pub struct BatchItem {
    /// Byte offset of the statement in the editor.
    pub at: usize,
    pub sql: String,
    pub state: ItemState,
    pub elapsed: Option<Duration>,
    /// Its result, from the moment it starts.
    pub rs: Option<Arc<ResultSet>>,
    /// Values bound to its `$n`.
    pub params: Vec<Option<String>>,
    /// Runs inside a transaction that is rolled back (Explain Analyze).
    pub rollback: bool,
    /// What names its result tab when it is a later result set of the same statement (a T-SQL
    /// batch's): the statement of the batch that most likely returned it.
    pub label: Option<String>,
}

impl BatchItem {
    /// `UPDATE orders`, `CREATE INDEX …`: the first words, for the batch panel.
    pub fn summary(&self) -> String {
        let words: Vec<&str> = self.sql.split_whitespace().filter(|w| !w.starts_with("--")).take(3).collect();
        let mut s = words.join(" ");
        if self.sql.split_whitespace().count() > 3 {
            s.push_str(" …");
        }
        s
    }
}

pub struct QueryTab {
    pub id: usize,
    pub editor: Entity<CodeEditor>,
    pub grid: Entity<ResultGrid>,
    /// An EXPLAIN result on screen is drawn as a plan, in place of the grid.
    pub plan: Entity<PlanView>,
    /// Pending grid edits of the current result.
    pub edit: Entity<GridEdit>,
    conn: Option<Arc<Connection>>,
    /// The database of this tab's session (the connection's own when `None`).
    database: Option<String>,
    session: Option<Arc<Session>>,
    /// Statements of the last run, in order.
    batch: Vec<BatchItem>,
    queue: VecDeque<usize>,
    /// The statement running (or the last one that ran) and its index.
    current: Option<(Arc<ResultSet>, usize)>,
    /// The result tab on screen and its result (none for the messages tab).
    shown: Option<Key>,
    shown_rs: Option<Arc<ResultSet>>,
    /// The tab the user picked (`None`: follow the run).
    pick: Option<Key>,
    /// Grid state of the results not on screen.
    views: Views<GridView>,
    /// Result tabs of this run the user closed (their × in the strip).
    closed: HashSet<Key>,
    /// When the current statement started (the timer in the tab and the toolbar).
    started: Option<Instant>,
    /// When the user asked to cancel (cleared once the server confirms).
    cancel_at: Option<Instant>,
    /// A message for the status bar when there is nothing better (no connection…).
    pub notice: Option<SharedString>,
    schema_version: u64,
    /// SQL Server's syntax check of the text, after typing pauses (dropped on the next edit).
    parse_task: Option<gpui::Task<()>>,
    _watch: Option<Task<()>>,
    /// LAR state of this tab: a live query shown instead of the grid (`crate::lar`).
    pub lar: crate::lar::TabLar,
    /// Waiting for "Discard & run" / "Cancel" (pending grid edits would be lost).
    pub pending_run: Option<PendingRun>,
    /// Something ran in this tab (a live SUBSCRIBE / LISTEN counts): the result panel shows
    /// from the first run on, and stays; before it the editor has the whole work surface.
    pub has_run: bool,
    /// A title given with the tab's SQL (a followed reference), kept while the text is that SQL.
    fixed_title: Option<(SharedString, String)>,
    /// A table opened as data: the editor's statement is built from it (`table_view`).
    pub table: Option<TableData>,
    /// A COMMIT (`true`) or ROLLBACK on its way.
    ending: Option<bool>,
    /// A run waiting for the user (dangerous statements, a failed transaction).
    pub confirm: Option<TabConfirm>,
    /// A run waiting for its parameters' values.
    pub params: Option<PendingParams>,
    /// The last value given to each parameter (`$1`, `:name`), offered again.
    param_values: HashMap<String, String>,
    /// Relations this tab's statements created (temp tables): the lint knows them.
    known: HashSet<String>,
    /// Databases whose function catalog was asked for already.
    catalog_asked: HashSet<String>,
    /// The activity monitor, when this tab is one ("Activity · name"): it takes the editor's,
    /// the toolbar's and the result's place.
    pub monitor: Option<Entity<crate::activity::ActivityView>>,
    /// Restored from the saved session: the connection (profile id) and database it was on,
    /// until that connection is up (`adopt`); other connections do not take the tab meanwhile.
    pub want: Option<(String, Option<String>)>,
    /// Statements run while waiting for `want`'s connection: they run once it connects.
    waiting: Option<Vec<Stmt>>,
    /// The results file last written for this tab and the run it holds (`results_key`): the
    /// next save reuses it while the run is the same.
    pub(crate) saved_results: Option<(u64, String)>,
    /// An agent's tab (`agent`): the client and connection it works for.
    pub agent: Option<crate::agent::AgentTab>,
    /// The agent the next run is for, and the rows it reads of each result (`run_agent`).
    agent_next: Option<(String, usize)>,
    /// The agent the current run's statements run for (taken from `agent_next` as it starts;
    /// `None`: the user's run).
    agent_run: Option<(String, usize)>,
    /// An agent's write shown here while it runs elsewhere (`show_agent`).
    pub(crate) agent_pending: Option<Arc<ResultSet>>,
    /// The editor holds an agent's draft the user has not run yet (`draft_agent`).
    pub(crate) agent_draft: bool,
    /// The result panel is zoomed over the editor and the side panel (each tab its own), and
    /// the side panel it covered (another one opening ends the zoom: it shows).
    pub(crate) zoomed: bool,
    pub(crate) zoom_side: Option<&'static str>,
    /// The favorite this tab was opened from (or saved as): its id and name. The tab is titled
    /// with the name, and ⌘S saves the editor's text into it.
    pub favorite: Option<(String, SharedString)>,
}

impl EventEmitter<TabEvent> for QueryTab {}

impl Drop for QueryTab {
    /// A closed tab must not leave a paused statement holding its backend (and its locks).
    fn drop(&mut self) {
        if let Some(s) = &self.session {
            s.cancel();
        }
    }
}

impl QueryTab {
    pub fn new(id: usize, conn: Option<Arc<Connection>>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // One gutter width for the editor's line numbers and the grid's row numbers.
        let gutter = crate::grid::GutterShare::default();
        let editor = cx.new(|cx| {
            let mut e = CodeEditor::new(window, cx);
            e.share_gutter(gutter.clone());
            e
        });
        let grid = cx.new(|cx| {
            let mut g = ResultGrid::new(cx);
            g.share_gutter(gutter);
            g
        });
        let plan = cx.new(PlanView::new);
        let edit = cx.new(|cx| GridEdit::new(grid.clone(), window, cx));
        cx.subscribe(&edit, |t, _, ev: &EditEvent, cx| {
            // After a commit, show what the server has now.
            if let EditEvent::Committed { .. } = ev {
                t.refresh_shown(cx);
            }
        })
        .detach();
        cx.subscribe(&editor, |t, _, ev: &EditorEvent, cx| match ev {
            EditorEvent::Run(stmts) | EditorEvent::RunAll(stmts) => t.run_stmts(stmts.clone(), cx),
            EditorEvent::Changed => {
                t.refresh_schema(cx);
                t.check_table_text(cx);
                t.schedule_parse_check(cx);
            }
            EditorEvent::Definition { def, peek } => cx.emit(TabEvent::Definition { def: def.clone(), peek: *peek }),
        })
        .detach();
        let mut t = QueryTab {
            id,
            editor,
            grid,
            plan,
            edit,
            conn: None,
            database: None,
            session: None,
            batch: Vec::new(),
            queue: VecDeque::new(),
            current: None,
            shown: None,
            shown_rs: None,
            pick: None,
            views: Views::default(),
            closed: HashSet::new(),
            started: None,
            cancel_at: None,
            notice: None,
            schema_version: 0,
            parse_task: None,
            _watch: None,
            lar: Default::default(),
            pending_run: None,
            has_run: false,
            fixed_title: None,
            table: None,
            ending: None,
            confirm: None,
            params: None,
            param_values: HashMap::new(),
            known: HashSet::new(),
            catalog_asked: HashSet::new(),
            monitor: None,
            want: None,
            waiting: None,
            saved_results: None,
            agent: None,
            agent_next: None,
            agent_run: None,
            agent_pending: None,
            agent_draft: false,
            zoomed: false,
            zoom_side: None,
            favorite: None,
        };
        t.set_connection(conn, cx);
        t
    }

    pub fn connection(&self) -> Option<&Arc<Connection>> {
        self.conn.as_ref()
    }

    /// The result on screen (the active result tab's).
    pub fn result(&self) -> Option<&Arc<ResultSet>> {
        self.shown_rs.as_ref()
    }

    /// The result tab on screen.
    pub fn shown_key(&self) -> Option<Key> {
        self.shown
    }

    /// The result on screen is the statement running now (or the last one that ran).
    pub fn shows_current(&self) -> bool {
        match (&self.shown_rs, &self.current) {
            (Some(a), Some((b, _))) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// Phase of the run for the title bar: failed when a statement failed, else the last one's.
    pub fn run_phase(&self) -> Option<Phase> {
        if self.failed_index().is_some() {
            return Some(Phase::Failed);
        }
        self.current.as_ref().map(|(rs, _)| rs.phase())
    }

    /// The result tabs of the run (`dataman_core::batch`), less the ones closed. A live query
    /// stands alone: no tabs.
    pub fn result_tabs(&self) -> Vec<batch::Tab> {
        if self.lar.live.is_some() {
            return Vec::new();
        }
        let stmts: Vec<(&str, Option<Outcome>)> =
            self.batch.iter().map(|b| (b.label.as_deref().unwrap_or(&b.sql), b.rs.as_deref().map(Outcome::of))).collect();
        let mut tabs = batch::tabs(&stmts);
        tabs.retain(|t| !self.closed.contains(&t.key));
        tabs
    }

    /// The result behind a result tab (none for the messages tab).
    pub fn result_of(&self, key: Key) -> Option<Arc<ResultSet>> {
        match key {
            Key::Stmt(i) => self.batch.get(i).and_then(|b| b.rs.clone()),
            Key::Messages => None,
        }
    }

    fn key_of(&self, rs: &Arc<ResultSet>) -> Option<Key> {
        self.batch.iter().position(|b| b.rs.as_ref().is_some_and(|r| Arc::ptr_eq(r, rs))).map(Key::Stmt)
    }

    /// Puts the active result tab on screen (the pick, else what the run points at) when it is
    /// not there already. Cheap when nothing changed.
    fn sync_active(&mut self, cx: &mut Context<Self>) {
        let tabs = self.result_tabs();
        let Some(key) = batch::active(&tabs, self.pick) else { return };
        let rs = self.result_of(key);
        let same_rs = match (&self.shown_rs, &rs) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if self.shown == Some(key) && same_rs {
            return;
        }
        self.show(key, rs, cx);
    }

    /// Swaps the grid to `rs`, keeping the state of the result it leaves.
    fn show(&mut self, key: Key, rs: Option<Arc<ResultSet>>, cx: &mut Context<Self>) {
        let mut views = std::mem::take(&mut self.views);
        let (old_key, old_rs) = (self.shown, self.shown_rs.clone());
        let rt = self.conn.as_ref().map(|c| c.handle().clone());
        self.grid.update(cx, |g, cx| {
            let old = g.take_view();
            if let (Some(k), Some(r)) = (old_key, old_rs) {
                views.put(k, r, old);
            }
            let v = match &rs {
                Some(rs) => views.take(key, rs).unwrap_or_else(|| GridView::fresh(rs.clone())),
                None => GridView::empty(),
            };
            g.show_view(v, rt, cx);
        });
        self.views = views;
        self.shown = Some(key);
        self.shown_rs = rs.clone();
        let (conn, db) = (self.conn.clone(), self.database().to_string());
        self.edit.update(cx, |e, cx| e.reset(conn, db, rs, cx));
        self.sync_table_grid(cx);
        self.sync_plan(cx);
        cx.notify();
    }

    // -----------------------------------------------------------------------------------------
    // Explain.

    /// Explain (`analyze`: Explain Analyze) of the statement under the cursor. ANALYZE runs it:
    /// on PostgreSQL inside a transaction that is rolled back (a savepoint when one is open),
    /// through the same checks as a run (parameters, the guard, read-only refusal).
    pub fn explain(&mut self, analyze: bool, cx: &mut Context<Self>) {
        let Some(stmt) = self.editor.update(cx, |e, cx| e.current_statement(cx)) else { return };
        let target = self.conn.as_ref().map(|c| explain::Target::of(&c.info)).unwrap_or_default();
        let sql = explain::sql(&stmt.sql, analyze, target);
        let rollback = !target.lar && explain::analyzes(&sql) && !explain::is_explain(&stmt.sql);
        self.run_stmts(vec![Stmt { at: stmt.at, sql, params: Vec::new(), rollback }], cx);
    }

    /// The result on screen is an EXPLAIN that ended: the plan view takes the grid's place.
    pub fn shows_plan(&self, cx: &gpui::App) -> bool {
        self.lar.live.is_none() && self.shown_rs.as_ref().is_some_and(|rs| self.plan.read(cx).is_for(rs))
    }

    /// Hands the plan view the EXPLAIN result on screen, once it is complete.
    fn sync_plan(&mut self, cx: &mut Context<Self>) {
        let rs = self.shown_rs.clone().filter(|r| explain::is_explain(&r.sql) && r.phase() == Phase::Done && r.columns().is_some_and(|c| !c.is_empty()));
        self.plan.update(cx, |p, cx| p.set_result(rs, cx));
    }

    /// The plan on screen as text, for the clipboard: PostgreSQL's own `FORMAT TEXT` for a
    /// plan that did not run the statement (asked again on this session, with the same
    /// values), else printed from the plan (an analyzed one must not run twice).
    pub fn plan_text(&mut self, cx: &mut Context<Self>) -> Task<Option<String>> {
        let Some(plan) = self.plan.read(cx).plan().cloned() else { return Task::ready(None) };
        let rs = self.shown_rs.clone();
        let target = self.conn.as_ref().map(|c| explain::Target::of(&c.info)).unwrap_or_default();
        let ask = match (&rs, &self.session) {
            (Some(rs), Some(s)) if plan.format == explain::Format::Json && !plan.analyzed && !target.lar => {
                explain::inner(&rs.sql).map(|inner| (s.clone(), explain::text_sql(inner, target), rs.params().to_vec()))
            }
            _ => None,
        };
        let Some((session, sql, params)) = ask else { return Task::ready(Some(explain::render_text(&plan))) };
        if self.is_running() {
            return Task::ready(Some(explain::render_text(&plan)));
        }
        // Rolled back like an Explain Analyze: copying opens no transaction and leaves none.
        let rs = session.execute_opts(sql, params, true);
        rs.fetch_all();
        let mut rx = rs.subscribe();
        cx.spawn(async move |_, _| {
            while !rs.phase().is_finished() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
            if rs.phase() != Phase::Done {
                return Some(explain::render_text(&plan));
            }
            let snap = rs.snapshot();
            Some((0..snap.rows()).filter_map(|r| snap.text(r, 0).map(|s| s.into_owned())).collect::<Vec<_>>().join("\n"))
        })
    }

    /// The user picks a result tab (a click, ⌥⌘[ ⌥⌘]): it stays on screen until the next run.
    pub fn select_result(&mut self, key: Key, cx: &mut Context<Self>) {
        if self.shown == Some(key) {
            self.pick = Some(key);
            return;
        }
        self.guard_edits_as("switch", move |t, cx| {
            t.pick = Some(key);
            t.sync_active(cx);
        }, cx);
    }

    /// The next (or previous) result tab, wrapping.
    pub fn cycle_result(&mut self, forward: bool, cx: &mut Context<Self>) {
        let tabs = self.result_tabs();
        if tabs.len() < 2 {
            return;
        }
        if let Some(k) = batch::cycle(&tabs, self.shown, forward) {
            self.select_result(k, cx);
        }
    }

    /// Closes a result tab (the × on its tab in the strip). The one on screen gives way to its
    /// neighbour (the next, else the one before); pending grid edits on it are asked about
    /// first. The strip shows only with two tabs or more, so the last one never closes.
    pub fn close_result(&mut self, key: Key, cx: &mut Context<Self>) {
        let tabs = self.result_tabs();
        let Some(i) = tabs.iter().position(|t| t.key == key) else { return };
        if tabs.len() < 2 {
            return;
        }
        let neighbour = tabs.get(i + 1).or_else(|| i.checked_sub(1).and_then(|j| tabs.get(j))).map(|t| t.key);
        let close = move |t: &mut QueryTab, cx: &mut Context<QueryTab>| {
            t.closed.insert(key);
            if t.shown == Some(key) {
                t.pick = neighbour;
                t.sync_active(cx);
            } else if t.pick == Some(key) {
                t.pick = None;
            }
            t.views.remove(key);
            cx.notify();
        };
        if self.shown == Some(key) {
            self.guard_edits_as("close", close, cx);
        } else {
            close(self, cx);
        }
    }

    /// References resolved for `rs`: on the grid when it is on screen, else kept with its view.
    fn apply_foreign_keys(&mut self, rs: &Arc<ResultSet>, fk: Arc<ForeignKeys>, cx: &mut Context<Self>) {
        if self.shown_rs.as_ref().is_some_and(|r| Arc::ptr_eq(r, rs)) {
            self.grid.update(cx, |g, cx| g.set_foreign_keys(rs, fk, cx));
            return;
        }
        if let Some(v) = self.views.of_result(rs) {
            v.set_foreign_keys(fk);
        } else if let Some(key) = self.key_of(rs) {
            let mut v = GridView::fresh(rs.clone());
            v.set_foreign_keys(fk);
            self.views.put(key, rs.clone(), v);
        }
    }

    /// After a commit of grid edits: the statement on screen runs again in place (its tab keeps
    /// its place among the others); a single result runs as a new run.
    fn refresh_shown(&mut self, cx: &mut Context<Self>) {
        match self.shown {
            Some(Key::Stmt(ix)) if self.batch.len() > 1 && self.queue.is_empty() && !self.is_running() => {
                self.batch[ix].state = ItemState::Pending;
                self.pick = Some(Key::Stmt(ix));
                self.queue.push_back(ix);
                self.next(cx);
            }
            _ => {
                if let Some(sql) = self.result().map(|r| r.sql.clone()) {
                    self.run(vec![sql], cx);
                }
            }
        }
    }

    /// Statements of the last run and their outcome.
    pub fn batch_items(&self) -> &[BatchItem] {
        &self.batch
    }

    /// The index of the statement that failed, if the last run stopped on an error.
    pub fn failed_index(&self) -> Option<usize> {
        self.batch.iter().position(|b| b.state == ItemState::Failed)
    }

    /// Names the tab (`customer · id = 42`) while its editor still holds `sql`.
    pub fn set_title(&mut self, title: impl Into<SharedString>, sql: impl Into<String>, cx: &mut Context<Self>) {
        self.fixed_title = Some((title.into(), sql.into()));
        cx.notify();
    }

    /// Makes this tab the activity monitor `m`: its Cancel / Terminate / Reset come up on this
    /// tab's confirmation sheet.
    pub fn attach_monitor(&mut self, m: Entity<crate::activity::ActivityView>, cx: &mut Context<Self>) {
        cx.subscribe(&m, |t, _, ev: &crate::activity::ActivityEvent, cx| {
            if let crate::activity::ActivityEvent::Confirm(p) = ev {
                t.confirm = Some(TabConfirm::Monitor(p.clone()));
                cx.emit(TabEvent::Confirm);
                cx.notify();
            }
        })
        .detach();
        self.monitor = Some(m);
        cx.notify();
    }

    pub fn title(&self, cx: &gpui::App) -> SharedString {
        if let Some(m) = &self.monitor {
            return m.read(cx).title();
        }
        if let Some((_, name)) = &self.favorite {
            return name.clone();
        }
        let text = self.editor.read(cx).text();
        if let Some((title, sql)) = &self.fixed_title
            && text == sql.as_str()
        {
            return title.clone();
        }
        let first = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with("--"))
            .map(|l| l.chars().take(28).collect::<String>());
        match first {
            Some(f) => f.into(),
            None => format!("console {}", self.id).into(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.current.as_ref().is_some_and(|(rs, _)| !rs.phase().is_finished() && rs.phase() != Phase::Paused)
    }

    /// The run goes on after the current result: statements wait in the queue, or the current
    /// statement returned another result set the tab has not taken up yet.
    pub fn run_continues(&self) -> bool {
        !self.queue.is_empty() || self.current.as_ref().is_some_and(|(rs, _)| rs.next_result().is_some())
    }

    /// The server has not confirmed a cancel yet.
    pub fn is_cancelling(&self) -> bool {
        self.cancel_at.is_some() && self.current.as_ref().is_some_and(|(rs, _)| !rs.phase().is_finished())
    }

    /// Since when a cancel waits for the server.
    pub fn cancel_requested_at(&self) -> Option<Instant> {
        self.cancel_at.filter(|_| self.is_cancelling())
    }

    /// Time since the current statement started (while it runs), or its total.
    pub fn elapsed(&self) -> Option<Duration> {
        let (rs, _) = self.current.as_ref()?;
        let st = rs.status();
        match st.elapsed {
            Some(e) => Some(e),
            // Paused, the clock stops: only running time counts.
            None => self.started.map(|s| s.elapsed().saturating_sub(st.paused_time())),
        }
    }

    /// Binds the tab to a connection (a new session opens on the next statement).
    pub fn set_connection(&mut self, conn: Option<Arc<Connection>>, cx: &mut Context<Self>) {
        let same = match (&self.conn, &conn) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.release_session();
        let lar = conn.as_ref().is_some_and(|c| c.info.is_lar);
        if conn.is_some() {
            // Put on a connection (picked, or the one it waited for): no longer waiting.
            self.want = None;
            self.waiting = None;
        }
        self.conn = conn;
        self.lar = Default::default();
        self.database = None;
        self.schema_version = 0;
        self.known.clear();
        self.editor.update(cx, |e, cx| {
            e.set_server(lar, cx);
            e.set_known(Arc::default(), cx);
        });
        self.refresh_schema(cx);
        cx.notify();
    }

    /// Runs this tab's statements on another database of the same server.
    pub fn set_database(&mut self, db: String, cx: &mut Context<Self>) {
        let Some(conn) = &self.conn else { return };
        if db == self.database() {
            return;
        }
        // Autocomplete needs that database's schema too.
        let _ = conn.preload(&db);
        self.release_session();
        self.database = Some(db);
        self.schema_version = 0;
        cx.notify();
    }

    /// Drops the session, stopping whatever it still runs or holds paused.
    fn release_session(&mut self) {
        if let Some(s) = self.session.take() {
            s.cancel();
        }
    }

    // -----------------------------------------------------------------------------------------
    // Transactions.

    /// The session's transaction, as the server last reported it.
    pub fn tx(&self) -> TxState {
        self.session.as_ref().map(|s| s.tx()).unwrap_or_default()
    }

    /// A transaction is open (or failed) in this tab's session.
    pub fn tx_open(&self) -> bool {
        self.tx().status.is_open()
    }

    /// A COMMIT or ROLLBACK is on its way.
    pub fn ending_tx(&self) -> bool {
        self.ending.is_some()
    }

    /// COMMIT (`commit`) or ROLLBACK the open transaction. The result also goes out as
    /// `TabEvent::TxEnded`.
    pub fn end_tx(&mut self, commit: bool, cx: &mut Context<Self>) -> Task<Result<TxEnd, String>> {
        let Some(s) = self.session.clone().filter(|s| s.tx().status.is_open()) else { return Task::ready(Ok(TxEnd::NoTransaction)) };
        let statements = s.tx().statements;
        self.ending = Some(commit);
        cx.notify();
        let task = s.end_transaction(commit);
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(|e| format!("{e:#}"));
            let _ = this.update(cx, |t, cx| {
                t.ending = None;
                cx.emit(TabEvent::TxEnded { result: result.clone(), statements });
                cx.emit(TabEvent::Finished);
                cx.notify();
            });
            result
        })
    }

    /// The user's answer to the run waiting in `confirm`; false when it does not answer it (⌘⌫
    /// on the guard sheet), which stays up.
    pub fn resolve_confirm(&mut self, choice: ConfirmChoice, cx: &mut Context<Self>) -> bool {
        let Some(c) = self.confirm.take() else { return true };
        match (c, choice) {
            (_, ConfirmChoice::Cancel) => {}
            (TabConfirm::Guard { stmts, .. }, ConfirmChoice::Run) => self.start_run(stmts, cx),
            (c @ TabConfirm::Guard { .. }, ConfirmChoice::Rollback) => {
                self.confirm = Some(c);
                return false;
            }
            (TabConfirm::Failed { stmts }, ConfirmChoice::Run) => {
                let end = self.end_tx(false, cx);
                cx.spawn(async move |this, cx| {
                    if end.await.is_ok() {
                        let _ = this.update(cx, |t, cx| t.run_checked(stmts, false, cx));
                    }
                })
                .detach();
            }
            (TabConfirm::Failed { .. }, ConfirmChoice::Rollback) => self.end_tx(false, cx).detach(),
            (TabConfirm::Monitor(p), ConfirmChoice::Run) => {
                if let Some(m) = &self.monitor {
                    m.update(cx, |m, cx| m.run_pending(p, cx));
                }
            }
            (c @ TabConfirm::Monitor(_), ConfirmChoice::Rollback) => {
                self.confirm = Some(c);
                return false;
            }
        }
        cx.notify();
        true
    }

    pub fn database(&self) -> &str {
        match (&self.database, &self.conn) {
            (Some(d), _) => d,
            (None, Some(c)) => c.database(),
            (None, None) => "",
        }
    }

    /// The SQL dialect of the tab's connection (PostgreSQL without one).
    pub fn dialect(&self) -> dataman_core::sql::Dialect {
        self.conn.as_ref().map(|c| c.dialect()).unwrap_or_default()
    }

    /// SQL Server: `PARSE_DELAY` after the last edit the whole text goes through the server's
    /// parser (`Connection::parse_check`, a connection of its own, nothing runs); its errors
    /// show as squiggles. A newer edit drops the wait and any answer for an older text.
    pub fn schedule_parse_check(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self.conn.clone().filter(|c| c.engine() == dataman_core::Engine::SqlServer) else {
            self.parse_task = None;
            return;
        };
        let (rev, text) = {
            let e = self.editor.read(cx);
            (e.rev(), e.text().to_string())
        };
        if text.len() > dataman_core::parse_check::MAX_BYTES {
            self.parse_task = None;
            self.editor.update(cx, |e, cx| e.set_server_lints(rev, Vec::new(), cx));
            return;
        }
        let db = self.database().to_string();
        let editor = self.editor.downgrade();
        self.parse_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(PARSE_DELAY).await;
            let Ok(found) = conn.parse_check(&db, text).await else { return };
            let _ = editor.update(cx, |e, cx| e.set_server_lints(rev, found, cx));
        }));
    }

    /// Hands the editor the latest schema snapshot for autocomplete (cheap when unchanged).
    pub fn refresh_schema(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self.conn.clone() else { return };
        let v = conn.schema.version();
        if v == self.schema_version {
            return;
        }
        self.schema_version = v;
        let db = self.database().to_string();
        let snap = conn.schema.snapshot(&db);
        // Functions and types load once per database, on first use, in the background.
        if snap.catalog.is_none() && self.catalog_asked.insert(db.clone()) {
            let task = conn.catalog(&db);
            cx.spawn(async move |this, cx| {
                if task.await.is_ok() {
                    let _ = this.update(cx, |t, cx| t.refresh_schema(cx));
                }
            })
            .detach();
        }
        self.editor.update(cx, |e, cx| e.set_schema(snap, cx));
        // A tab just bound to a SQL Server connection (or another database): its text too.
        self.schedule_parse_check(cx);
    }

    /// Runs statements (SQL only; offsets unknown: they are looked up in the editor).
    pub fn run(&mut self, stmts: Vec<String>, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().to_string();
        let mut from = 0;
        let stmts = stmts
            .into_iter()
            .map(|s| {
                let at = text.get(from..).and_then(|t| t.find(s.as_str())).map(|i| from + i).unwrap_or(from);
                from = at + s.len();
                Stmt { at, sql: s, params: Vec::new(), rollback: false }
            })
            .collect();
        self.run_stmts(stmts, cx);
    }

    /// An agent's read (`agent`): its SQL in the editor, run in this tab's own session, each
    /// statement a read-only transaction of its own (the guard and the tiers checked them
    /// already; no prompt, no confirmation), `rows` of each result loaded ahead. Several
    /// statements run as a script: a result tab each. The results run and stay here like any
    /// other; [`QueryTab::agent_results`] says when the agent's view of them is ready.
    pub fn run_agent(&mut self, sql: String, params: Vec<Option<String>>, client: &str, title: Option<String>, rows: usize, cx: &mut Context<Self>) -> Option<Arc<ResultSet>> {
        self.put_agent_text(&sql, title, cx);
        self.agent_next = Some((client.to_string(), rows));
        self.agent_pending = None;
        self.agent_draft = false;
        let split = dataman_core::sql::split_in(&sql, self.dialect());
        let stmts = if split.len() > 1 {
            split.into_iter().map(|s| Stmt { at: s.range.start, sql: sql[s.range].to_string(), params: Vec::new(), rollback: false }).collect()
        } else {
            vec![Stmt { at: 0, sql, params, rollback: false }]
        };
        self.start_run(stmts, cx);
        self.current.as_ref().map(|(rs, _)| rs.clone())
    }

    /// The results of the agent's read [`QueryTab::run_agent`] started, once the agent's view
    /// of them is ready: every statement done (or the run stopped at a failure), the last
    /// result finished, paused or past `max_rows` (and no later result set of a T-SQL batch to
    /// wait for). Each with its statement's index and SQL (a later result set's statement).
    pub(crate) fn agent_results(&self, max_rows: usize) -> Option<Vec<(usize, String, Arc<ResultSet>)>> {
        let (rs, ix) = self.current.as_ref()?;
        if !self.queue.is_empty() {
            return None;
        }
        let p = rs.phase();
        let ready = if p.is_finished() { rs.next_result().is_none() } else { (p == Phase::Paused || rs.rows() > max_rows) && !self.more_may_follow(*ix) };
        if !ready {
            return None;
        }
        let mut out = Vec::new();
        let (mut statement, mut at) = (0, None);
        for b in &self.batch {
            let Some(rs) = &b.rs else { continue };
            if at.is_some_and(|a| a != b.at) {
                statement += 1;
            }
            at = Some(b.at);
            out.push((statement, b.label.clone().unwrap_or_else(|| b.sql.clone()), rs.clone()));
        }
        Some(out)
    }

    /// Statements in the current run (a T-SQL batch's later result sets are not more).
    pub(crate) fn run_statements(&self) -> usize {
        let mut ats: Vec<usize> = self.batch.iter().map(|b| b.at).collect();
        ats.dedup();
        ats.len()
    }

    /// An agent's write that runs on the agent's own backend: its SQL in the editor and `rs` (a
    /// placeholder until [`ResultSet::finish_with`] ends it) as this tab's run.
    pub fn show_agent(&mut self, sql: String, rs: Arc<ResultSet>, title: Option<String>, cx: &mut Context<Self>) {
        self.put_agent_text(&sql, title, cx);
        self.agent_draft = false;
        self.notice = None;
        self.has_run = true;
        self.views.clear();
        self.closed.clear();
        self.shown = None;
        self.pick = None;
        self.batch = vec![BatchItem { at: 0, sql, state: ItemState::Running, elapsed: None, rs: Some(rs.clone()), params: Vec::new(), rollback: false, label: None }];
        self.queue.clear();
        self.cancel_at = None;
        self.current = Some((rs.clone(), 0));
        self.started = Some(Instant::now());
        self.agent_pending = Some(rs.clone());
        self.editor.update(cx, |e, cx| {
            e.set_diagnostic(None, cx);
            e.set_marks(Vec::new(), cx);
        });
        self.follow(rs, cx);
    }

    /// An agent's draft: its SQL in the editor (the caret at its end), nothing run, nothing of
    /// earlier runs left on screen (the result area is gone until the user runs it). The user's
    /// run is theirs like any other: the guard, their read-only setting, their history.
    pub fn draft_agent(&mut self, sql: &str, title: Option<String>, cx: &mut Context<Self>) {
        self.put_agent_text(sql, title, cx);
        self.agent_draft = true;
        self.agent_pending = None;
        self.notice = None;
        self.has_run = false;
        self.batch.clear();
        self.queue.clear();
        self.current = None;
        self.views.clear();
        self.closed.clear();
        self.pick = None;
        self.started = None;
        self.cancel_at = None;
        self.saved_results = None;
        self.shown = None;
        self.shown_rs = None;
        let rt = self.conn.as_ref().map(|c| c.handle().clone());
        self.grid.update(cx, |g, cx| {
            let _ = g.take_view();
            g.show_view(GridView::empty(), rt, cx);
        });
        let (conn, db) = (self.conn.clone(), self.database().to_string());
        self.edit.update(cx, |e, cx| e.reset(conn, db, None, cx));
        self.sync_plan(cx);
        cx.notify();
    }

    fn put_agent_text(&mut self, sql: &str, title: Option<String>, cx: &mut Context<Self>) {
        self.editor.update(cx, |e, cx| e.set_text(sql, cx));
        if let Some(a) = &mut self.agent {
            a.text = sql.to_string();
        }
        if let Some(t) = title.filter(|t| !t.trim().is_empty()) {
            self.set_title(t, sql.to_string(), cx);
        }
    }

    /// An agent may use this tab now: it is its tab, nothing runs or waits in it, no
    /// transaction is open, no edits are pending, and the editor still holds what the agent put
    /// there (the user did not take it over). A draft the user has not run yet is theirs to run:
    /// only another `draft` may replace it.
    pub fn agent_free(&self, draft: bool, cx: &gpui::App) -> bool {
        let Some(a) = &self.agent else { return false };
        (draft || !self.agent_draft)
            && !self.is_running()
            && !self.run_continues()
            && !self.tx_open()
            && !self.ending_tx()
            && self.confirm.is_none()
            && self.params.is_none()
            && self.pending_run.is_none()
            && self.agent_pending.as_ref().is_none_or(|r| r.phase().is_finished())
            && self.edit.read(cx).pending_count() == 0
            && self.monitor.is_none()
            && self.table.is_none()
            && self.lar.live.is_none()
            && self.editor.read(cx).text() == a.text.as_str()
    }

    /// Runs `f` now, or — when grid edits are pending and `f` would replace their result —
    /// once the user chose "Discard & run" (nothing is discarded silently).
    pub fn guard_edits(&mut self, f: impl FnOnce(&mut QueryTab, &mut Context<QueryTab>) + 'static, cx: &mut Context<Self>) {
        self.guard_edits_as("run", f, cx);
    }

    /// `guard_edits` for something else than a run (`verb` names it on the sheet's button).
    pub(crate) fn guard_edits_as(&mut self, verb: &'static str, f: impl FnOnce(&mut QueryTab, &mut Context<QueryTab>) + 'static, cx: &mut Context<Self>) {
        let edits = self.edit.read(cx).pending_count();
        if edits == 0 {
            f(self, cx);
            return;
        }
        self.pending_run = Some(PendingRun { edits, verb, run: Box::new(f) });
        cx.emit(TabEvent::ConfirmDiscard);
        cx.notify();
    }

    /// The user's answer: discard the edits and run, or keep them and run nothing.
    pub fn resolve_pending_run(&mut self, discard: bool, cx: &mut Context<Self>) {
        let Some(p) = self.pending_run.take() else { return };
        if discard {
            self.edit.update(cx, |e, cx| e.discard_all(cx));
            (p.run)(self, cx);
        }
        cx.notify();
    }

    pub fn run_stmts(&mut self, stmts: Vec<Stmt>, cx: &mut Context<Self>) {
        if stmts.is_empty() {
            return;
        }
        self.guard_edits(move |t, cx| t.run_stmts_now(stmts, cx), cx);
    }

    fn run_stmts_now(&mut self, stmts: Vec<Stmt>, cx: &mut Context<Self>) {
        self.run_checked(stmts, true, cx);
    }

    /// The checks before a run: a failed transaction first (`failed_check`; the server would
    /// refuse every statement), then the guard rules for the connection's environment. A
    /// read-only connection skips the guard: it refuses every write anyway (`next`).
    fn run_checked(&mut self, stmts: Vec<Stmt>, failed_check: bool, cx: &mut Context<Self>) {
        // Parameters first: their values are asked for once, then everything else follows.
        if let Some(prompt) = self.param_prompt(&stmts, cx) {
            self.params = Some(PendingParams { prompt, stmts, failed_check });
            cx.emit(TabEvent::Params);
            cx.notify();
            return;
        }
        let Some(conn) = self.conn.clone() else {
            return self.start_run(stmts, cx);
        };
        if failed_check
            && self.tx().status == TxStatus::Failed
            && !stmts.first().is_some_and(|s| guard::classify_in(&s.sql, conn.dialect()).ends_failed_tx())
        {
            self.confirm = Some(TabConfirm::Failed { stmts });
            cx.emit(TabEvent::Confirm);
            cx.notify();
            return;
        }
        if !conn.read_only_for(self.database()) {
            let sqls: Vec<&str> = stmts.iter().map(|s| s.sql.as_str()).collect();
            let findings = guard::review_in(&sqls, conn.profile.env, conn.dialect());
            if !findings.is_empty() {
                self.confirm = Some(TabConfirm::Guard { stmts, findings });
                cx.emit(TabEvent::Confirm);
                cx.notify();
                return;
            }
        }
        self.start_run(stmts, cx);
    }

    /// The prompt for the statements' parameters (`$n`, `:name`) not bound yet, with the types
    /// the statements show and the values given last time; `None` when there are none.
    fn param_prompt(&mut self, stmts: &[Stmt], cx: &mut Context<Self>) -> Option<Entity<ParamPrompt>> {
        let snap = self.conn.as_ref().map(|c| c.schema.snapshot(self.database()));
        let dialect = self.dialect();
        let mut rows: Vec<(String, Option<String>, String)> = Vec::new();
        for s in stmts.iter().filter(|s| s.params.is_empty()) {
            let Some(mut p) = dataman_core::params::detect_in(&s.sql, dialect) else { continue };
            if let Some(snap) = &snap {
                dataman_core::params::infer(&mut p, snap);
            }
            for x in p.params {
                match rows.iter_mut().find(|r| r.0 == x.key) {
                    Some(r) => {
                        if r.1.is_none() {
                            r.1 = x.hint;
                        }
                    }
                    None => {
                        let last = self.param_values.get(&x.key).cloned().unwrap_or_default();
                        rows.push((x.key, x.hint, last));
                    }
                }
            }
        }
        if rows.is_empty() {
            return None;
        }
        let prompt = cx.new(|cx| ParamPrompt::new(rows, cx));
        cx.subscribe(&prompt, |t, _, ev: &ParamEvent, cx| t.resolve_params(matches!(ev, ParamEvent::Run), cx)).detach();
        Some(prompt)
    }

    /// Run (`run`) with the values typed, or cancel. Variables become `$n`; every value is
    /// bound, never written into the SQL. The values are kept for the next run.
    pub fn resolve_params(&mut self, run: bool, cx: &mut Context<Self>) {
        let Some(p) = self.params.take() else { return };
        cx.emit(TabEvent::ParamsClosed);
        cx.notify();
        if !run {
            return;
        }
        let values: HashMap<String, String> = p.prompt.read(cx).values(cx).into_iter().collect();
        self.param_values.extend(values.clone());
        let dialect = self.dialect();
        let stmts = p
            .stmts
            .into_iter()
            .map(|mut s| {
                if s.params.is_empty()
                    && let Some(d) = dataman_core::params::detect_in(&s.sql, dialect)
                {
                    s.params = d.params.iter().map(|x| dataman_core::params::bind_value(values.get(&x.key).map_or("", String::as_str))).collect();
                    s.sql = d.sql;
                }
                s
            })
            .collect();
        self.run_checked(stmts, p.failed_check, cx);
    }

    fn start_run(&mut self, stmts: Vec<Stmt>, cx: &mut Context<Self>) {
        let Some(conn) = self.conn.clone() else {
            // A restored tab connects on its first run (as the explorer would) and runs then.
            if let Some((id, _)) = self.want.clone() {
                self.waiting = Some(stmts);
                cx.emit(TabEvent::Connect(id));
                return;
            }
            self.notice = Some("No connection: pick one in the explorer".into());
            cx.notify();
            return;
        };
        self.notice = None;
        self.has_run = true;
        // A draft run (by the user) is no longer one waiting for them.
        self.agent_draft = false;
        // Whose run this is: the agent's when `run_agent` started it, else the user's.
        self.agent_run = self.agent_next.take();
        let db = self.database.clone();
        self.session.get_or_insert_with(|| conn.session(db.as_deref()));
        // Re-run replaces the whole set. The result on screen stays until the first statement
        // starts, but is no longer a tab of the run.
        self.views.clear();
        self.closed.clear();
        self.shown = None;
        self.pick = None;
        self.batch = stmts
            .into_iter()
            .map(|s| BatchItem { at: s.at, sql: s.sql, state: ItemState::Pending, elapsed: None, rs: None, params: s.params, rollback: s.rollback, label: None })
            .collect();
        self.queue = (0..self.batch.len()).collect();
        self.cancel_at = None;
        self.editor.update(cx, |e, cx| {
            e.set_diagnostic(None, cx);
            e.set_marks(Vec::new(), cx);
        });
        self.next(cx);
    }

    /// Runs the last batch again from statement `ix` (after fixing an error).
    pub fn run_from(&mut self, ix: usize, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().to_string();
        // The statements may have moved since: re-split from the failed one's offset (which
        // may now fall inside a character: back to its start).
        let at = floor_char(&text, self.batch.get(ix).map(|b| b.at).unwrap_or(0));
        let stmts: Vec<Stmt> = dataman_core::sql::batches_in(&text[at..], self.dialect())
            .into_iter()
            .filter(|s| !text[at + s.range.start..at + s.range.end].trim().is_empty())
            .map(|s| Stmt { at: at + s.range.start, sql: text[at + s.range.start..at + s.range.end].to_string(), params: Vec::new(), rollback: false })
            .collect();
        self.run_stmts(stmts, cx);
    }

    fn next(&mut self, cx: &mut Context<Self>) {
        // Only the statements `run_agent` starts are the agent's.
        let agent = self.agent_run.clone();
        let (Some(ix), Some(session)) = (self.queue.pop_front(), self.session.clone()) else { return };
        let sql = self.batch[ix].sql.clone();
        // A read-only database refuses writes before they reach the server (LAR accepts READ
        // ONLY without enforcing it, SQL Server has no such session setting); a script stops
        // there, like on an error. Asked per statement: a toggle in the explorer counts at once.
        let read_only = session.connection().read_only_for(&session.database);
        let refusal = if read_only { guard::read_only_refusal_in(&sql, self.dialect()) } else { None };
        // LAR: a live statement is a single live result of its own; inside a script it is refused
        // (the script stops there, like on an error).
        let refusal = refusal.map(|m| (Some("25006".to_string()), m)).or_else(|| {
            (self.batch.len() > 1 && crate::lar::is_live(self, &sql)).then(|| (None, "SUBSCRIBE and LISTEN … AS run alone, not in a script".to_string()))
        });
        if let Some((code, message)) = refusal {
            let err = ErrorInfo { code, message, ..Default::default() };
            let rs = ResultSet::refused(sql, err);
            self.batch[ix].rs = Some(rs.clone());
            self.current = Some((rs, ix));
            self.started = Some(Instant::now());
            self.on_finished(cx);
            self.sync_active(cx);
            cx.notify();
            return;
        }
        // LAR: live statements run on their own connection.
        if crate::lar::start_live(self, &sql, cx) {
            self.batch[ix].state = ItemState::Ok;
            self.update_marks(cx);
            cx.emit(TabEvent::Finished);
            self.next(cx);
            cx.notify();
            return;
        }
        self.batch[ix].state = ItemState::Running;
        // Tables this statement creates (temp ones too) are not unknown to the lint.
        let created = dataman_core::lint::created_names_in(&sql, self.dialect());
        if !created.is_subset(&self.known) {
            self.known.extend(created);
            let known = Arc::new(self.known.clone());
            self.editor.update(cx, |e, cx| e.set_known(known, cx));
        }
        let rs = match &agent {
            Some((client, rows)) => {
                let rs = session.execute_agent(sql, self.batch[ix].params.clone(), client);
                rs.request_rows(*rows);
                rs
            }
            None => session.execute_opts(sql, self.batch[ix].params.clone(), self.batch[ix].rollback),
        };
        // A table's page is bounded by its LIMIT, a plan is one cell (or a few lines): read
        // them all.
        if self.table.as_ref().is_some_and(|d| d.stmt == rs.sql) || explain::is_explain(&rs.sql) {
            rs.fetch_all();
        }
        self.batch[ix].rs = Some(rs.clone());
        self.current = Some((rs.clone(), ix));
        self.started = Some(Instant::now());
        self.follow(rs, cx);
    }

    /// Whether a statement's later result sets may still come after the one on screen: a T-SQL
    /// batch of several statements (or `GO n` repeats).
    fn more_may_follow(&self, ix: usize) -> bool {
        let Some(b) = self.batch.get(ix) else { return false };
        self.dialect() == dataman_core::sql::Dialect::TSql && dataman_core::sql::split::may_return_more(&b.sql)
    }

    /// A later result set of the statement at `ix` (a T-SQL batch returns any number): a result
    /// tab of its own, right after the one before it, followed like a statement of the run.
    fn follow_next_result(&mut self, ix: usize, next: Arc<ResultSet>, cx: &mut Context<Self>) {
        let Some(item) = self.batch.get(ix).cloned() else { return };
        // Its place among the statement's result sets names it.
        let first = self.batch[..=ix].iter().rposition(|b| b.at != item.at || b.sql != item.sql).map_or(0, |p| p + 1);
        let k = ix + 1 - first;
        let label = dataman_core::sql::split::result_statement(&item.sql, k).map(str::to_string);
        // The statement's first result set is named the same way once it has a second.
        if k == 1 && self.batch[first].label.is_none() {
            self.batch[first].label = dataman_core::sql::split::result_statement(&item.sql, 0).map(str::to_string);
        }
        let at = ix + 1;
        self.batch.insert(at, BatchItem { state: ItemState::Running, elapsed: None, rs: Some(next.clone()), label, ..item });
        for q in self.queue.iter_mut() {
            if *q >= at {
                *q += 1;
            }
        }
        self.closed = std::mem::take(&mut self.closed)
            .into_iter()
            .map(|k| match k {
                Key::Stmt(i) if i >= at => Key::Stmt(i + 1),
                k => k,
            })
            .collect();
        self.current = Some((next.clone(), at));
        self.follow(next, cx);
    }

    /// Follows a running result: its references, the result tab on screen, the gutter marks,
    /// and a watch that moves the run on when it ends.
    fn follow(&mut self, rs: Arc<ResultSet>, cx: &mut Context<Self>) {
        let (conn, db) = (self.conn.clone(), self.database().to_string());
        // Which columns hold references (foreign keys, LAR links): resolved once per result on
        // the core's threads (catalog reads on the metadata connection, never this session).
        if let Some(conn) = &conn {
            let (task, rs) = (conn.foreign_keys(&db, &rs), rs.clone());
            cx.spawn(async move |this, cx| {
                if let Ok(fk) = task.await
                    && !fk.is_empty()
                {
                    let _ = this.update(cx, |t, cx| t.apply_foreign_keys(&rs, fk, cx));
                }
            })
            .detach();
        }
        // On screen when the run is followed (not when the user picked another result).
        self.sync_active(cx);
        self.update_marks(cx);
        let mut rx = rs.subscribe();
        let rs = rs.clone();
        self._watch = Some(cx.spawn(async move |this, cx| {
            let mut announced = false;
            // Wake on every change, and every 100 ms for the clock while running or waiting
            // for a cancel (not while paused: an idle app must not redraw).
            let mut tick = true;
            loop {
                let changed = next_change(&mut rx, tick, cx).await;
                let done = this
                    .update(cx, |t, cx| {
                        cx.notify();
                        // A paused result is already in the history: let the panel refresh.
                        if !announced && rs.phase() == Phase::Paused {
                            announced = true;
                            cx.emit(TabEvent::Finished);
                            // Inside a batch the next statement must run: keep the rows loaded and
                            // drain the rest rather than cancel it (cancelling would abort a
                            // transaction) or keep it all (memory). So with a T-SQL batch whose
                            // later result sets wait behind this one.
                            let ix = t.current.as_ref().map(|(_, i)| *i).unwrap_or(0);
                            if !t.queue.is_empty() || t.more_may_follow(ix) {
                                rs.stop_fetching();
                            } else if let Some(c) = t.conn.clone() {
                                // "Fetch all ~30k": the planner's estimate, asked once, off
                                // the tab's session.
                                c.ask_row_estimate(t.database(), &rs);
                            }
                        }
                        let finished = t.current.as_ref().map(|(r, _)| r.phase().is_finished()).unwrap_or(true);
                        if finished {
                            t.on_finished(cx);
                        }
                        // Columns or the end can fold a statement into the messages tab.
                        t.sync_active(cx);
                        // The columns of a table's page arrived: the header shows its sort.
                        t.sync_table_grid(cx);
                        // An EXPLAIN that ended: its plan replaces the grid.
                        t.sync_plan(cx);
                        tick = rs.phase() == Phase::Running || t.cancel_at.is_some();
                        finished
                    })
                    .unwrap_or(true);
                if done || !changed {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn on_finished(&mut self, cx: &mut Context<Self>) {
        let Some((rs, ix)) = self.current.clone() else { return };
        let status = rs.status();
        // The statement goes on with another result set: this one is done, the run is not.
        if let Some(next) = rs.next_result() {
            if let Some(item) = self.batch.get_mut(ix) {
                item.elapsed = status.elapsed.or(self.started.map(|s| s.elapsed()));
                item.state = ItemState::Ok;
            }
            self.follow_next_result(ix, next, cx);
            return;
        }
        self.cancel_at = None;
        if let Some(item) = self.batch.get_mut(ix) {
            item.elapsed = status.elapsed.or(self.started.map(|s| s.elapsed()));
            item.state = match rs.phase() {
                Phase::Failed => ItemState::Failed,
                Phase::Cancelled => ItemState::Cancelled,
                _ => ItemState::Ok,
            };
        }
        cx.emit(TabEvent::Finished);
        self.maybe_count(&rs, cx);
        match rs.phase() {
            Phase::Done if !self.queue.is_empty() => {
                self.update_marks(cx);
                self.next(cx);
            }
            Phase::Failed => {
                self.queue.clear();
                self.update_marks(cx);
                if let Some(err) = status.error {
                    let at = self.batch[ix].at;
                    // An Explain's position counts from the `EXPLAIN (…)` Dataman put before
                    // the statement the editor holds.
                    let typed = self.editor.read(cx).text().get(at..).is_some_and(explain::is_explain);
                    let prefix = match explain::inner(&rs.sql) {
                        Some(inner) if !typed => rs.sql.len() - inner.len(),
                        _ => 0,
                    };
                    let diag = diagnostic(&rs.sql, at, prefix, &err);
                    self.editor.update(cx, |e, cx| e.set_diagnostic(Some(diag), cx));
                }
            }
            _ => {
                self.queue.clear();
                self.update_marks(cx);
            }
        }
        // Statements like CREATE TABLE change the schema: refresh autocomplete's view of it.
        if let Some(conn) = &self.conn {
            let first = rs.sql.trim_start().split_whitespace().next().unwrap_or("").to_ascii_uppercase();
            if matches!(first.as_str(), "CREATE" | "DROP" | "ALTER") {
                let db = self.database().to_string();
                conn.schema.invalidate(Some(&db));
                let _ = conn.preload(&db);
                // The catalog went with the cache: it loads again on the next refresh.
                self.catalog_asked.remove(&db);
            }
        }
    }

    /// Gutter marks for a multi-statement run: ✓ done, ✕ failed, · not run yet.
    fn update_marks(&mut self, cx: &mut Context<Self>) {
        if self.batch.len() < 2 {
            return;
        }
        let marks = self
            .batch
            .iter()
            .map(|b| {
                let m = match b.state {
                    ItemState::Ok => Mark::Ok,
                    ItemState::Failed => Mark::Err,
                    ItemState::Running => Mark::Running,
                    ItemState::Pending | ItemState::Cancelled => Mark::Todo,
                };
                (b.at, m)
            })
            .collect();
        self.editor.update(cx, |e, cx| e.set_marks(marks, cx));
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        // ⌘. also stops a table's count.
        self.stop_count(cx);
        self.queue.clear();
        if let Some(s) = &self.session
            && self.current.as_ref().is_some_and(|(rs, _)| !rs.phase().is_finished())
        {
            self.cancel_at.get_or_insert_with(Instant::now);
            s.cancel();
        }
        cx.notify();
    }

    /// Demo screenshots: show the "cancel requested" state without sending the cancel.
    pub fn demo_pretend_cancel(&mut self, cx: &mut Context<Self>) {
        self.cancel_at = Some(Instant::now() - Duration::from_millis(1200));
        cx.notify();
    }

    /// "Fetch next 10k" on a paused result.
    pub fn fetch_more(&mut self, cx: &mut Context<Self>) {
        if let Some(rs) = &self.shown_rs {
            // The fetcher keeps 10k rows past what is requested.
            rs.request_rows(rs.rows());
        }
        cx.notify();
    }

    pub fn fetch_all(&mut self, cx: &mut Context<Self>) {
        if let Some(rs) = &self.shown_rs {
            rs.fetch_all();
        }
        cx.notify();
    }

    // -----------------------------------------------------------------------------------------
    // A table's data view (`table_view`).

    /// Whether the result on screen is a page of the tab's table view.
    pub fn shows_table(&self) -> bool {
        match (&self.table, &self.shown_rs) {
            (Some(d), Some(rs)) => rs.sql == d.stmt,
            _ => false,
        }
    }

    /// Opens `view` in this tab: its `SELECT` goes to the editor and runs; the table's row
    /// estimate is read on the side.
    pub fn open_table(&mut self, mut view: TableView, cx: &mut Context<Self>) {
        view.dialect = self.dialect();
        if let Some(conn) = self.conn.clone() {
            let task = conn.estimate_rows(self.database(), &view.schema, &view.table);
            cx.spawn(async move |this, cx| {
                if let Ok(n) = task.await {
                    let _ = this.update(cx, |t, cx| {
                        if let Some(d) = &mut t.table {
                            d.estimate = n;
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        }
        self.table = Some(TableData::new(view));
        self.apply_table(cx);
    }

    /// Runs the view's page: the editor shows its statement, the tab is named after the table.
    fn apply_table(&mut self, cx: &mut Context<Self>) {
        let Some(d) = &mut self.table else { return };
        d.sql = d.view.sql();
        d.stmt = d.sql.trim_end_matches(';').to_string();
        // Other conditions: the count of the old ones is moot.
        if d.total_of != d.view.count_sql() {
            d.counting = None;
        }
        let (sql, stmt, title) = (d.sql.clone(), d.stmt.clone(), d.view.table.clone());
        self.editor.update(cx, |e, cx| e.set_text(&sql, cx));
        self.set_title(title, sql, cx);
        self.run(vec![stmt], cx);
        self.sync_table_grid(cx);
    }

    /// Changes the view and runs its page, once pending grid edits are dealt with (nothing
    /// changes when the user keeps them).
    fn change_table(&mut self, f: impl FnOnce(&mut TableView) + 'static, cx: &mut Context<Self>) {
        if self.table.is_none() {
            return;
        }
        self.guard_edits(
            move |t, cx| {
                if let Some(d) = &mut t.table {
                    f(&mut d.view);
                }
                t.apply_table(cx);
            },
            cx,
        );
    }

    /// The statement was edited by hand: from now on a plain query tab.
    fn check_table_text(&mut self, cx: &mut Context<Self>) {
        let Some(d) = &self.table else { return };
        if self.editor.read(cx).text() != d.sql {
            self.table = None;
            self.grid.update(cx, |g, cx| g.set_table_mode(None, cx));
            cx.notify();
        }
    }

    /// Tells the grid whether it shows a page of the view (server-side sort, row offset).
    fn sync_table_grid(&mut self, cx: &mut Context<Self>) {
        let mode = match (&self.table, &self.shown_rs) {
            (Some(d), Some(rs)) if rs.sql == d.stmt => {
                let sort = d.view.sort.as_ref().and_then(|(name, dir)| {
                    rs.columns().and_then(|c| c.iter().position(|c| &c.name == name)).map(|i| (i, *dir))
                });
                Some(TableMode { sort, offset: d.view.offset() as usize })
            }
            _ => None,
        };
        self.grid.update(cx, |g, cx| g.set_table_mode(mode, cx));
    }

    fn column_of(&self, col: usize) -> Option<dataman_core::result::ColumnInfo> {
        self.shown_rs.as_ref()?.columns()?.get(col).cloned()
    }

    /// A header click or the header menu: sort on the server by result column `col`.
    pub fn table_sort(&mut self, col: usize, dir: Option<SortDir>, cx: &mut Context<Self>) {
        let Some(c) = self.column_of(col) else { return };
        self.change_table(
            move |v| {
                v.sort = dir.map(|d| (c.name, d));
                v.page = 0;
            },
            cx,
        );
    }

    /// Goes to page `page` (clamped to the last one when the total is known).
    pub fn table_page(&mut self, page: i64, cx: &mut Context<Self>) {
        let Some(d) = &self.table else { return };
        let mut page = page.max(0) as u64;
        if let Some(n) = d.total.filter(|_| d.total_of == d.view.count_sql()) {
            page = page.min(d.view.last_page(n));
        }
        if page == d.view.page {
            return;
        }
        self.change_table(move |v| v.page = page, cx);
    }

    /// ⌥⌘→ / ⌥⌘← and the pager's arrows.
    pub fn table_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(d) = &self.table else { return };
        if !self.shows_table() {
            return;
        }
        let rows = self.shown_rs.as_ref().map_or(0, |r| r.rows());
        let finished = self.shown_rs.as_ref().is_some_and(|r| r.phase().is_finished());
        let p = crate::table_view::pager(d, rows, finished);
        let page = d.view.page as i64;
        match forward {
            true if p.next => self.table_page(page + 1, cx),
            false if p.prev => self.table_page(page - 1, cx),
            _ => {}
        }
    }

    /// Adds a filter (or replaces filter `i`); back to the first page.
    pub fn add_filter(&mut self, f: Filter, replace: Option<usize>, cx: &mut Context<Self>) {
        self.change_table(
            move |v| {
                match replace {
                    Some(i) if i < v.filters.len() => v.filters[i] = f,
                    _ => v.filters.push(f),
                }
                v.page = 0;
            },
            cx,
        );
    }

    pub fn remove_filter(&mut self, i: usize, cx: &mut Context<Self>) {
        self.change_table(
            move |v| {
                if i < v.filters.len() {
                    v.filters.remove(i);
                }
                v.page = 0;
            },
            cx,
        );
    }

    /// "Filter by value" on a cell: rows whose column equals it (`None`: IS NULL).
    pub fn filter_value(&mut self, col: usize, value: Option<String>, cx: &mut Context<Self>) {
        let Some(c) = self.column_of(col) else { return };
        let f = match value {
            None => Filter::new(c.name, c.kind, Op::IsNull, ""),
            // Quoted, so a value that reads `NULL` stays text.
            Some(v) if v.trim().eq_ignore_ascii_case("null") => Filter::new(c.name, c.kind, Op::Eq, dataman_core::sqlgen::quote_literal(&v)),
            Some(v) => Filter::new(c.name, c.kind, Op::Eq, v),
        };
        self.add_filter(f, None, cx);
    }

    /// A page finished: count the rows under the view's conditions, unless the page told the
    /// total already (a short first page), the count runs or is known, or the user stopped it.
    fn maybe_count(&mut self, rs: &Arc<ResultSet>, cx: &mut Context<Self>) {
        let Some(d) = &mut self.table else { return };
        if rs.sql != d.stmt || rs.phase() != Phase::Done {
            return;
        }
        let key = d.view.count_sql();
        if d.total_of == key && (d.total.is_some() || d.counting.is_some()) {
            return;
        }
        if d.stopped.as_deref() == Some(key.as_str()) {
            return;
        }
        if d.view.page == 0 && (rs.rows() as u64) < d.view.page_size {
            d.total = Some(rs.rows() as u64);
            d.total_of = key;
            return;
        }
        self.start_count(cx);
    }

    /// Counts the rows under the view's conditions on a backend of its own.
    pub fn start_count(&mut self, cx: &mut Context<Self>) {
        let db = self.database().to_string();
        let (Some(conn), Some(d)) = (self.conn.clone(), &mut self.table) else { return };
        let key = d.view.count_sql();
        d.stopped = None;
        d.total = None;
        d.total_of = key.clone();
        let count = conn.count_rows(&db, key.clone());
        let task = count.task;
        let job = cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |t, cx| {
                if let Some(d) = &mut t.table
                    && d.total_of == key
                {
                    d.counting = None;
                    if let Ok(n) = r {
                        d.total = Some(n);
                    }
                    cx.notify();
                }
            });
        });
        d.counting = Some((count.stop, job));
        cx.notify();
    }

    /// Stops the count (it is not restarted for these conditions until asked).
    pub fn stop_count(&mut self, cx: &mut Context<Self>) {
        if let Some(d) = &mut self.table
            && d.counting.take().is_some()
        {
            d.stopped = Some(d.total_of.clone());
            cx.notify();
        }
    }
}

// -------------------------------------------------------------------------------------------
// The saved session (`crate::restore`, `dataman_core::session`).

impl QueryTab {
    /// What the session keeps of this tab, less its results file.
    pub fn session_state(&self, cx: &gpui::App) -> dataman_core::session::TabState {
        let e = self.editor.read(cx);
        let (conn, database) = match (&self.conn, &self.want) {
            (Some(c), _) => (Some(c.profile.id.clone()), Some(self.database().to_string())),
            (None, Some((id, db))) => (Some(id.clone()), db.clone()),
            (None, None) => (None, None),
        };
        dataman_core::session::TabState {
            sql: e.text().to_string(),
            selection: e.selection_ends(),
            scroll: e.scroll_points(),
            title: self.fixed_title.as_ref().map(|(t, _)| t.to_string()),
            title_sql: self.fixed_title.as_ref().map(|(_, s)| s.clone()),
            conn,
            database,
            table: self.table.as_ref().map(|d| d.view.clone()),
            results: None,
            shown: self.shown,
            closed: self.closed.iter().copied().collect(),
            favorite: self.favorite.as_ref().map(|(id, _)| id.clone()),
        }
    }

    /// The title a favorite saved from this tab gets: the name it was given (a file's, a
    /// followed reference's) or its first line of SQL (comments aside), flattened and capped.
    pub fn favorite_name(&self, cx: &gpui::App) -> String {
        let text = self.editor.read(cx).text();
        if let Some((title, sql)) = &self.fixed_title
            && text == sql.as_str()
        {
            let t = title.to_string();
            return t.strip_suffix(".sql").or_else(|| t.strip_suffix(".SQL")).map(str::to_string).unwrap_or(t);
        }
        let first = text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("--")).unwrap_or("");
        crate::history::fav_name(first)
    }

    /// The run as it stands (statements, states, results): a new run, rows arriving, a sort
    /// or a result's end changes it.
    pub fn results_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for b in &self.batch {
            (b.at, b.state as u8, b.elapsed.map(|d| d.as_micros())).hash(&mut h);
            if let Some(rs) = &b.rs {
                (Arc::as_ptr(rs) as usize, rs.rows(), rs.phase() as u8, rs.sort_state().map(|(c, d)| (c, d as u8))).hash(&mut h);
            }
        }
        h.finish()
    }

    /// The statements of the last run and their results, to save. A live query and a monitor
    /// save none (they are not results).
    pub fn save_items(&self) -> Vec<dataman_core::session::SaveItem> {
        use dataman_core::session::ItemState as S;
        if self.lar.live.is_some() || self.monitor.is_some() {
            return Vec::new();
        }
        self.batch
            .iter()
            .map(|b| dataman_core::session::SaveItem {
                at: b.at,
                sql: b.sql.clone(),
                state: match b.state {
                    ItemState::Pending => S::Pending,
                    ItemState::Ok => S::Ok,
                    ItemState::Failed => S::Failed,
                    // Running when saved: if the app goes now, the run was cut short.
                    ItemState::Running | ItemState::Cancelled => S::Cancelled,
                },
                elapsed: b.elapsed,
                label: b.label.clone(),
                params: b.params.clone(),
                rs: b.rs.clone(),
            })
            .collect()
    }

    /// A saved tab back: its text, selection and scroll, title and table view. Its connection
    /// is `want`ed (set by the workspace) until it is up.
    pub fn restore(&mut self, st: &dataman_core::session::TabState, cx: &mut Context<Self>) {
        let (sel, scroll) = (st.selection, st.scroll);
        self.editor.update(cx, |e, cx| e.restore_saved(&st.sql, sel, scroll, cx));
        if let (Some(t), Some(s)) = (&st.title, &st.title_sql) {
            self.fixed_title = Some((t.clone().into(), s.clone()));
        }
        // A table's page: only while the editor still holds its statement.
        if let Some(view) = st.table.clone()
            && view.sql() == st.sql
        {
            self.table = Some(TableData::new(view));
        }
        cx.notify();
    }

    /// The saved results of the tab's last run back: the result strip, the result on screen
    /// (`shown`, the closed tabs gone), the gutter marks and the error in the editor. They are
    /// snapshots: grid edits refuse them until they run again.
    pub fn restore_results(&mut self, items: Vec<dataman_core::session::RestoredItem>, shown: Option<Key>, closed: Vec<Key>, file: Option<String>, cx: &mut Context<Self>) {
        use dataman_core::session::ItemState as S;
        if items.is_empty() {
            return;
        }
        self.batch = items
            .into_iter()
            .map(|it| BatchItem {
                at: it.at,
                sql: it.sql,
                state: match it.state {
                    S::Pending => ItemState::Pending,
                    S::Ok => ItemState::Ok,
                    S::Failed => ItemState::Failed,
                    S::Cancelled => ItemState::Cancelled,
                },
                elapsed: it.elapsed,
                rs: it.rs,
                params: it.params,
                rollback: false,
                label: it.label,
            })
            .collect();
        self.queue.clear();
        self.current = self.batch.iter().enumerate().rev().find_map(|(i, b)| b.rs.clone().map(|r| (r, i)));
        self.views.clear();
        self.closed = closed.into_iter().collect();
        self.pick = shown.filter(|k| !self.closed.contains(k));
        self.shown = None;
        self.shown_rs = None;
        self.has_run = true;
        self.sync_active(cx);
        self.update_marks(cx);
        if let Some(ix) = self.failed_index()
            && let Some(rs) = self.batch[ix].rs.clone()
            && let Some(err) = rs.status().error
        {
            let at = self.batch[ix].at;
            let typed = self.editor.read(cx).text().get(at..).is_some_and(explain::is_explain);
            let prefix = match explain::inner(&rs.sql) {
                Some(inner) if !typed => rs.sql.len() - inner.len(),
                _ => 0,
            };
            let diag = diagnostic(&rs.sql, at, prefix, &err);
            self.editor.update(cx, |e, cx| e.set_diagnostic(Some(diag), cx));
        }
        // The file holds this run: the next save keeps it as it is.
        self.saved_results = file.map(|f| (self.results_key(), f));
        cx.notify();
    }

    /// The connection this restored tab waited for is up: the tab goes on it (and on its
    /// database), its saved results get their references (the "Go to" of foreign keys), and
    /// what was run meanwhile runs now.
    pub fn adopt(&mut self, conn: Arc<Connection>, cx: &mut Context<Self>) {
        let db = self.want.take().and_then(|(_, db)| db);
        let waiting = self.waiting.take();
        self.set_connection(Some(conn.clone()), cx);
        if let Some(db) = db {
            self.set_database(db, cx);
        }
        let db = self.database().to_string();
        for rs in self.batch.iter().filter_map(|b| b.rs.clone()).filter(|r| r.is_restored() && r.columns().is_some_and(|c| !c.is_empty())) {
            let task = conn.foreign_keys(&db, &rs);
            cx.spawn(async move |this, cx| {
                if let Ok(fk) = task.await
                    && !fk.is_empty()
                {
                    let _ = this.update(cx, |t, cx| t.apply_foreign_keys(&rs, fk, cx));
                }
            })
            .detach();
        }
        if let Some(stmts) = waiting {
            self.run_checked(stmts, true, cx);
        }
    }
}

/// `i` moved back to a character boundary of `s` (and within it): offsets kept from before an
/// edit must not split a character.
pub fn floor_char(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Where the error points in the editor, and the one-line message beside it. `prefix`: bytes
/// at the start of `sql` that are not in the editor (an Explain's `EXPLAIN (…) `).
fn diagnostic(sql: &str, at: usize, prefix: usize, err: &ErrorInfo) -> Diagnostic {
    // The server's position counts characters from 1 within the statement. Past the end
    // ("at end of input") points at the end of the statement.
    let pos = err
        .position
        .map(|p| at + sql.char_indices().nth(p.saturating_sub(1) as usize).map(|(i, _)| i).unwrap_or(sql.len()).saturating_sub(prefix));
    let msg = match &err.code {
        Some(c) => format!("{c} {}", err.message),
        None => err.message.clone(),
    };
    Diagnostic { stmt: at..at + sql.len() - prefix, at: pos, message: msg.into() }
}

/// Waits for the next change of the result (or 100 ms with `tick`); false when the result is
/// gone.
async fn next_change(rx: &mut tokio::sync::watch::Receiver<u64>, tick: bool, cx: &mut gpui::AsyncApp) -> bool {
    use std::pin::pin;
    use std::task::Poll;
    let mut changed = pin!(rx.changed());
    let mut timer = pin!(cx.background_executor().timer(Duration::from_millis(100)));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(r) = changed.as_mut().poll(cx) {
            return Poll::Ready(r.is_ok());
        }
        if tick && timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(true);
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_snap_to_characters() {
        let s = "Zürich;";
        assert_eq!(floor_char(s, 2), 1, "inside ü");
        assert_eq!(floor_char(s, 3), 3);
        assert_eq!(floor_char(s, 99), s.len());
        assert_eq!(floor_char("", 5), 0);
    }
}
