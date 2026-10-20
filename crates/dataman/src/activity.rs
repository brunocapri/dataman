use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, ScrollHandle, SharedString, StyledText,
    Task, WeakEntity, Window, actions, div, prelude::*,
};
use crate::theme::px;
use dataman_core::Connection;
use dataman_core::activity::{self as act, LockEdge, Maintenance, SessionRow, Severity, Signal, State, StatementRow, Statements, TableRow, Thresholds, TreeRow};
use dataman_core::result::{Cell, ChunkBuilder, ColumnInfo, ResultSet};
use dataman_core::value::Kind;

use crate::assets::{Icon, icon};
use crate::grid::{CellDecor, Decorations, GridEvent, ResultGrid, RowDecor, Tone, tone_strong};
use crate::input::{InputEvent, TextInput};
use crate::theme::{ActiveTheme, Theme, metrics, mono};
use crate::ui::{self, Btn};

actions!(activity, [Refresh, LockUp, LockDown]);

pub const CONTEXT: &str = "ActivityMonitor";
const LOCKS_CONTEXT: &str = "ActivityLocks";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        crate::keymap::kb("cmd-r", Refresh, Some(CONTEXT)),
        crate::keymap::kb("up", LockUp, Some(LOCKS_CONTEXT)),
        crate::keymap::kb("down", LockDown, Some(LOCKS_CONTEXT)),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Sessions,
    Locks,
    Tables,
    Statements,
}

pub const SECTIONS: [Section; 4] = [Section::Sessions, Section::Locks, Section::Tables, Section::Statements];

impl Section {
    fn label(self) -> &'static str {
        match self {
            Section::Sessions => "Sessions",
            Section::Locks => "Locks",
            Section::Tables => "Tables",
            Section::Statements => "Statements",
        }
    }

    fn ix(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Every {
    S1,
    S2,
    S5,
    Off,
}

const EVERY: [Every; 4] = [Every::S1, Every::S2, Every::S5, Every::Off];

impl Every {
    pub fn duration(self) -> Option<Duration> {
        match self {
            Every::S1 => Some(Duration::from_secs(1)),
            Every::S2 => Some(Duration::from_secs(2)),
            Every::S5 => Some(Duration::from_secs(5)),
            Every::Off => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Every::S1 => "1s",
            Every::S2 => "2s",
            Every::S5 => "5s",
            Every::Off => "off",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Avail {
    #[default]
    Unknown,
    Yes,
    No,
}

#[derive(Clone, Debug)]
pub enum Pending {
    Signal { signal: Signal, session: SessionRow, own: bool },
    Reset,
}

pub enum ActivityEvent {
    OpenSql { sql: String, explain: bool },
    Confirm(Pending),
    Notice(String),
    Details { toggle: bool },
}

pub struct ActivityView {
    pub conn: Arc<Connection>,
    pub db: String,
    focus: FocusHandle,
    locks_focus: FocusHandle,
    locks_scroll: ScrollHandle,
    pub section: Section,
    pub every: Every,
    visible: bool,
    ticker: Option<Task<()>>,
    inflight: bool,
    again: bool,
    probed: bool,
    pub avail: [Avail; 4],
    sessions: Vec<SessionRow>,
    shown: Vec<SessionRow>,
    edges: Vec<LockEdge>,
    tree: Vec<TreeRow>,
    tables: Vec<TableRow>,
    bloat_due: bool,
    statements: Option<Statements>,
    statements_fixed: bool,
    pub sessions_grid: Entity<ResultGrid>,
    pub tables_grid: Entity<ResultGrid>,
    pub statements_grid: Entity<ResultGrid>,
    filter: Entity<TextInput>,
    pub show_idle: bool,
    pub show_background: bool,
    th: Thresholds,
    pub sel_pid: Option<i32>,
    sel_table: Option<(String, String)>,
    sel_stmt: Option<String>,
    pub details: Entity<ActivityDetails>,
    pub refreshes: u64,
    pub redraws: u64,
    pub renders: u64,
    pub detached: bool,
    generation: u64,
}

impl EventEmitter<ActivityEvent> for ActivityView {}

impl Focusable for ActivityView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ActivityView {
    pub fn new(conn: Arc<Connection>, db: String, cx: &mut Context<Self>) -> Self {
        let grid = |cx: &mut Context<Self>, widths: &[f32]| {
            let w = widths.to_vec();
            cx.new(|cx| {
                let mut g = ResultGrid::new(cx);
                g.prefer_widths(w);
                g
            })
        };
        let sessions_grid = grid(cx, &[72., 100., 100., 130., 150., 150., 170., 88., 88., 120., 96., 520.]);
        let tables_grid = grid(cx, &[110., 170., 92., 92., 76., 112., 132., 112., 140., 96., 96., 96., 140., 92., 88.]);
        let statements_grid = grid(cx, &[480., 92., 92., 92., 92., 72., 104., 100., 100.]);
        for g in [&sessions_grid, &tables_grid, &statements_grid] {
            cx.observe(g, |m, _, cx| m.on_grid(cx)).detach();
            cx.subscribe(g, |_, _, ev: &GridEvent, cx| {
                if matches!(ev, GridEvent::EditCell { .. } | GridEvent::ViewValue { .. }) {
                    cx.emit(ActivityEvent::Details { toggle: false });
                }
            })
            .detach();
        }
        let filter = cx.new(|cx| TextInput::new(cx, "Filter").bare());
        cx.subscribe(&filter, |m, _, ev: &InputEvent, cx| {
            if let InputEvent::Changed = ev {
                m.rebuild_sessions(cx);
                cx.notify();
            }
        })
        .detach();
        let me = cx.weak_entity();
        let details = cx.new(|cx| ActivityDetails { monitor: me, scroll: ScrollHandle::new(), focus: cx.focus_handle() });
        ActivityView {
            conn,
            db,
            focus: cx.focus_handle(),
            locks_focus: cx.focus_handle(),
            locks_scroll: ScrollHandle::new(),
            section: Section::Sessions,
            every: Every::S2,
            visible: false,
            ticker: None,
            inflight: false,
            again: false,
            probed: false,
            avail: [Avail::Unknown; 4],
            sessions: Vec::new(),
            shown: Vec::new(),
            edges: Vec::new(),
            tree: Vec::new(),
            tables: Vec::new(),
            bloat_due: true,
            statements: None,
            statements_fixed: false,
            sessions_grid,
            tables_grid,
            statements_grid,
            filter,
            show_idle: false,
            show_background: false,
            th: Thresholds::default(),
            sel_pid: None,
            sel_table: None,
            sel_stmt: None,
            details,
            refreshes: 0,
            redraws: 0,
            renders: 0,
            detached: false,
            generation: 0,
        }
    }

    pub fn rebind(&mut self, conn: Arc<Connection>, cx: &mut Context<Self>) {
        self.db = conn.database().to_string();
        self.conn = conn;
        self.generation += 1;
        self.inflight = false;
        self.again = false;
        self.probed = false;
        self.avail = [Avail::Unknown; 4];
        self.sessions.clear();
        self.edges.clear();
        self.tree.clear();
        self.tables.clear();
        self.statements = None;
        self.statements_fixed = false;
        self.bloat_due = true;
        (self.sel_pid, self.sel_table, self.sel_stmt) = (None, None, None);
        self.rebuild_sessions(cx);
        self.rebuild_tables(cx);
        self.rebuild_statements(cx);
        if self.visible {
            self.refresh(cx);
        }
        cx.notify();
    }

    pub fn title(&self) -> SharedString {
        format!("Activity · {}", self.conn.profile.name).into()
    }

    pub fn focus_target(&self, cx: &App) -> FocusHandle {
        match self.section {
            Section::Sessions => self.sessions_grid.focus_handle(cx),
            Section::Locks => self.locks_focus.clone(),
            Section::Tables => self.tables_grid.focus_handle(cx),
            Section::Statements if matches!(self.statements, Some(Statements::Rows(_))) => self.statements_grid.focus_handle(cx),
            Section::Statements => self.focus.clone(),
        }
    }

    pub fn set_visible(&mut self, on: bool, cx: &mut Context<Self>) {
        if on == self.visible {
            return;
        }
        self.visible = on;
        if on {
            self.refresh(cx);
        }
        self.schedule(cx);
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    #[cfg(feature = "checks")]
    pub fn ticking(&self) -> bool {
        self.ticker.is_some()
    }

    pub fn set_every(&mut self, every: Every, cx: &mut Context<Self>) {
        if self.every != every {
            self.every = every;
            self.schedule(cx);
            cx.notify();
        }
    }

    fn schedule(&mut self, cx: &mut Context<Self>) {
        self.ticker = None;
        let Some(every) = self.every.duration().filter(|_| self.visible) else { return };
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(every).await;
                if this.update(cx, |m, cx| m.refresh(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    pub fn refresh_now(&mut self, cx: &mut Context<Self>) {
        self.bloat_due = true;
        if self.inflight {
            self.again = true;
        } else {
            self.refresh(cx);
        }
    }

    /// Each read runs on the core's threads; nothing waits on the UI thread.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.inflight {
            return;
        }
        let probe = !self.probed;
        let s = self.section;
        let want = |x: Section| probe || s == x;
        let (conn, db) = (self.conn.clone(), self.db.clone());
        let sessions = (want(Section::Sessions) || want(Section::Locks)).then(|| conn.activity_sessions(&db));
        let locks = want(Section::Locks).then(|| conn.activity_locks(&db));
        let bloat = std::mem::take(&mut self.bloat_due) || probe;
        let tables = want(Section::Tables).then(|| conn.activity_tables(&db, bloat));
        let statements = (want(Section::Statements) && !self.statements_fixed).then(|| conn.activity_statements(&db));
        self.inflight = true;
        self.refreshes += 1;
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let sessions = match sessions {
                Some(t) => Some(t.await),
                None => None,
            };
            let locks = match locks {
                Some(t) => Some(t.await),
                None => None,
            };
            let tables = match tables {
                Some(t) => Some(t.await),
                None => None,
            };
            let statements = match statements {
                Some(t) => Some(t.await),
                None => None,
            };
            let _ = this.update(cx, |m, cx| {
                if m.generation != generation {
                    return;
                }
                m.inflight = false;
                m.apply(Read { probe, sessions, locks, tables, statements, bloat }, cx);
                if std::mem::take(&mut m.again) {
                    m.refresh(cx);
                }
            });
        })
        .detach();
    }

    fn apply(&mut self, r: Read, cx: &mut Context<Self>) {
        let lar = self.conn.info.is_lar;
        let mut changed = false;
        let mut sessions_changed = false;
        let mut details_changed = false;
        if let Some(res) = r.sessions {
            match res {
                Ok(rows) => {
                    changed |= self.set_avail(Section::Sessions, Avail::Yes);
                    let q = self.filter.read(cx).text().trim().to_lowercase();
                    let shown = filter_sessions(&rows, self.show_idle, self.show_background, &q);
                    let th = self.th;
                    let seen = |v: &[SessionRow]| v.iter().map(|s| on_screen(s, &th)).collect::<Vec<_>>();
                    let find = |v: &[SessionRow], pid: i32| v.iter().find(|s| s.pid == pid).map(|s| on_screen(s, &th));
                    sessions_changed = seen(&shown) != seen(&self.shown);
                    changed |= rows.len() != self.sessions.len()
                        || (self.section == Section::Locks && self.tree.iter().any(|t| find(&rows, t.pid) != find(&self.sessions, t.pid)));
                    details_changed = self.sel_pid.is_some_and(|p| rows.iter().find(|s| s.pid == p) != self.sessions.iter().find(|s| s.pid == p));
                    self.sessions = rows;
                }
                Err(_) => changed |= self.set_avail(Section::Sessions, Avail::No),
            }
        }
        if let Some(res) = r.locks {
            match res {
                Ok(edges) => {
                    changed |= self.set_avail(Section::Locks, Avail::Yes);
                    if edges != self.edges {
                        self.tree = act::blocking_tree(&edges);
                        self.edges = edges;
                        changed = true;
                    }
                }
                Err(_) => changed |= self.set_avail(Section::Locks, Avail::No),
            }
        }
        if let Some(res) = r.tables {
            match res {
                // LAR answers `pg_stat_user_tables` with no rows for its tables: not reported.
                Ok(rows) if lar && rows.is_empty() => changed |= self.set_avail(Section::Tables, Avail::No),
                Ok(mut rows) => {
                    changed |= self.set_avail(Section::Tables, Avail::Yes);
                    if !r.bloat {
                        let old: HashMap<(&str, &str), (Option<i64>, Option<f64>)> =
                            self.tables.iter().map(|t| ((t.schema.as_str(), t.name.as_str()), (t.bloat_bytes, t.bloat_pct))).collect();
                        for t in &mut rows {
                            if let Some(&(b, p)) = old.get(&(t.schema.as_str(), t.name.as_str())) {
                                (t.bloat_bytes, t.bloat_pct) = (b, p);
                            }
                        }
                    }
                    if rows != self.tables {
                        self.tables = rows;
                        self.rebuild_tables(cx);
                        changed = true;
                    }
                }
                Err(_) => changed |= self.set_avail(Section::Tables, Avail::No),
            }
        }
        if let Some(res) = r.statements.filter(|_| !self.statements_fixed) {
            match res {
                // LAR has no extensions to create: the section is not offered.
                Ok(_) if lar => changed |= self.set_avail(Section::Statements, Avail::No),
                Ok(st) => {
                    changed |= self.set_avail(Section::Statements, Avail::Yes);
                    if self.statements.as_ref() != Some(&st) {
                        self.statements = Some(st);
                        self.rebuild_statements(cx);
                        changed = true;
                    }
                }
                Err(_) => changed |= self.set_avail(Section::Statements, Avail::No),
            }
        }
        if sessions_changed {
            self.rebuild_sessions(cx);
            changed = true;
        }
        if r.probe {
            self.probed = true;
            if self.avail[self.section.ix()] != Avail::Yes
                && let Some(s) = SECTIONS.into_iter().find(|s| self.avail[s.ix()] == Avail::Yes)
            {
                self.section = s;
            }
            changed = true;
        }
        if changed || details_changed {
            self.details.update(cx, |_, cx| cx.notify());
        }
        if changed {
            self.redraws += 1;
            cx.notify();
        }
    }

    fn set_avail(&mut self, s: Section, a: Avail) -> bool {
        let cur = self.avail[s.ix()];
        if cur == a || (a == Avail::No && cur == Avail::Yes) {
            return false;
        }
        self.avail[s.ix()] = a;
        true
    }

    fn owns(&self, s: &SessionRow) -> bool {
        s.is_self || self.conn.owns_backend(s.pid)
    }

    fn rebuild_sessions(&mut self, cx: &mut Context<Self>) {
        let q = self.filter.read(cx).text().trim().to_lowercase();
        self.shown = filter_sessions(&self.sessions, self.show_idle, self.show_background, &q);
        let conn = self.conn.clone();
        let own = move |s: &SessionRow| s.is_self || conn.owns_backend(s.pid);
        let (rs, decor) = sessions_result(&self.shown, &own, &self.th);
        let pid = self.sel_pid;
        let grid = self.sessions_grid.clone();
        self.swap_grid(&grid, rs, decor, move |snap, r| matches!(snap.cell(r, 0), Cell::Int(p) if Some(p as i32) == pid), cx);
    }

    fn rebuild_tables(&mut self, cx: &mut Context<Self>) {
        let (rs, decor) = tables_result(&self.tables);
        let sel = self.sel_table.clone();
        let grid = self.tables_grid.clone();
        self.swap_grid(
            &grid,
            rs,
            decor,
            move |snap, r| sel.as_ref().is_some_and(|(s, n)| snap.text(r, 0).as_deref() == Some(s.as_str()) && snap.text(r, 1).as_deref() == Some(n.as_str())),
            cx,
        );
    }

    fn rebuild_statements(&mut self, cx: &mut Context<Self>) {
        let rows: &[StatementRow] = match &self.statements {
            Some(Statements::Rows(r)) => r,
            _ => &[],
        };
        let (rs, decor) = statements_result(rows);
        let keys: Vec<String> = rows.iter().map(stmt_key).collect();
        let sel = self.sel_stmt.clone();
        let grid = self.statements_grid.clone();
        self.swap_grid(&grid, rs, decor, move |snap, r| sel.as_ref() == keys.get(snap.source_row(r)), cx);
    }

    fn swap_grid(
        &mut self,
        grid: &Entity<ResultGrid>,
        rs: Arc<ResultSet>,
        decor: Decorations,
        is_sel: impl Fn(&dataman_core::result::Snapshot, usize) -> bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(sort) = grid.read(cx).result().and_then(|r| r.sort_state()) {
            rs.sort(Some(sort));
        }
        let snap = rs.snapshot();
        let row = (0..snap.rows()).find(|&r| is_sel(&snap, r));
        let rt = self.conn.handle().clone();
        grid.update(cx, |g, cx| {
            g.replace_result(rs, row, rt, cx);
            g.set_decorations(Some(Arc::new(decor)), cx);
        });
    }

    fn on_grid(&mut self, cx: &mut Context<Self>) {
        let source = |g: &Entity<ResultGrid>, cx: &App| -> Option<usize> {
            let g = g.read(cx);
            let row = g.active_row()?;
            let snap = g.result()?.snapshot();
            (row < snap.rows()).then(|| snap.source_row(row))
        };
        let before = (self.sel_pid, self.sel_table.clone(), self.sel_stmt.clone());
        match self.section {
            Section::Sessions => self.sel_pid = source(&self.sessions_grid, cx).and_then(|i| self.shown.get(i)).map(|s| s.pid),
            Section::Tables => self.sel_table = source(&self.tables_grid, cx).and_then(|i| self.tables.get(i)).map(|t| (t.schema.clone(), t.name.clone())),
            Section::Statements => {
                if let Some(Statements::Rows(rows)) = &self.statements {
                    self.sel_stmt = source(&self.statements_grid, cx).and_then(|i| rows.get(i)).map(stmt_key);
                }
            }
            Section::Locks => {}
        }
        if before != (self.sel_pid, self.sel_table.clone(), self.sel_stmt.clone()) {
            self.details.update(cx, |_, cx| cx.notify());
            cx.notify();
        }
    }

    pub fn set_section(&mut self, s: Section, window: &mut Window, cx: &mut Context<Self>) {
        if self.section == s {
            return;
        }
        self.section = s;
        match s {
            Section::Tables => self.bloat_due = true,
            Section::Sessions => self.rebuild_sessions(cx),
            _ => {}
        }
        if self.visible {
            self.refresh_now(cx);
        }
        let f = self.focus_target(cx);
        window.focus(&f, cx);
        self.details.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    pub fn set_filters(&mut self, idle: bool, background: bool, cx: &mut Context<Self>) {
        self.show_idle = idle;
        self.show_background = background;
        self.rebuild_sessions(cx);
        cx.notify();
    }

    pub fn session(&self, pid: i32) -> Option<&SessionRow> {
        self.sessions.iter().find(|s| s.pid == pid)
    }

    pub fn selected_session(&self) -> Option<&SessionRow> {
        self.session(self.sel_pid?)
    }

    pub fn selected_statement(&self) -> Option<&StatementRow> {
        let key = self.sel_stmt.as_ref()?;
        match &self.statements {
            Some(Statements::Rows(rows)) => rows.iter().find(|r| stmt_key(r) == *key),
            _ => None,
        }
    }

    fn selected_table(&self) -> Option<&TableRow> {
        let (s, n) = self.sel_table.as_ref()?;
        self.tables.iter().find(|t| &t.schema == s && &t.name == n)
    }

    fn selected_query(&self) -> Option<String> {
        match self.section {
            Section::Sessions | Section::Locks => self.selected_session()?.visible_query().map(str::to_string),
            Section::Statements => Some(self.selected_statement()?.query.clone()),
            Section::Tables => None,
        }
    }

    pub fn select_pid(&mut self, pid: Option<i32>, cx: &mut Context<Self>) {
        self.sel_pid = pid;
        if self.section == Section::Sessions {
            self.rebuild_sessions(cx);
        }
        self.details.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    pub fn request_signal(&mut self, signal: Signal, cx: &mut Context<Self>) {
        let Some(s) = self.selected_session().cloned() else { return };
        let own = self.owns(&s);
        cx.emit(ActivityEvent::Confirm(Pending::Signal { signal, session: s, own }));
    }

    pub fn request_reset(&mut self, cx: &mut Context<Self>) {
        if matches!(self.statements, Some(Statements::Rows(_))) {
            cx.emit(ActivityEvent::Confirm(Pending::Reset));
        }
    }

    pub fn run_pending(&mut self, p: Pending, cx: &mut Context<Self>) {
        match p {
            Pending::Signal { signal, session, .. } => {
                let pid = session.pid;
                let task = self.conn.signal_backend(&self.db, pid, signal);
                let verb = match signal {
                    Signal::Cancel => "Cancelled",
                    Signal::Terminate => "Terminated",
                };
                self.then_notice(
                    async move {
                        match task.await {
                            Ok(true) => format!("{verb} pid {pid}"),
                            Ok(false) => format!("pid {pid} is gone"),
                            Err(e) => format!("{e:#}"),
                        }
                    },
                    cx,
                );
            }
            Pending::Reset => {
                let task = self.conn.reset_statements(&self.db);
                self.then_notice(
                    async move {
                        match task.await {
                            Ok(()) => "pg_stat_statements reset".to_string(),
                            Err(e) => format!("{e:#}"),
                        }
                    },
                    cx,
                );
            }
        }
    }

    fn then_notice(&mut self, f: impl std::future::Future<Output = String> + 'static, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let msg = f.await;
            let _ = this.update(cx, |m, cx| {
                cx.emit(ActivityEvent::Notice(msg));
                m.refresh_now(cx);
            });
        })
        .detach();
    }

    fn copy_query(&mut self, cx: &mut Context<Self>) {
        if let Some(q) = self.selected_query() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(q));
            cx.emit(ActivityEvent::Notice("Query copied".into()));
        }
    }

    /// ⌘L there only when it has no parameters: a normalized statement's `$1` has no value to give.
    fn open_query(&mut self, explain: bool, cx: &mut Context<Self>) {
        if let Some(q) = self.selected_query() {
            let explain = explain && dataman_core::params::detect(&q).is_none();
            let sql = format!("{};", q.trim().trim_end_matches(';'));
            cx.emit(ActivityEvent::OpenSql { sql, explain });
        }
    }

    fn maintenance(&mut self, m: Maintenance, cx: &mut Context<Self>) {
        if let Some(t) = self.selected_table() {
            let sql = act::maintenance_template(&t.schema, &t.name, m);
            cx.emit(ActivityEvent::OpenSql { sql, explain: false });
        }
    }

    fn move_lock(&mut self, down: bool, cx: &mut Context<Self>) {
        if self.tree.is_empty() {
            return;
        }
        let cur = self.sel_pid.and_then(|p| self.tree.iter().position(|r| r.pid == p));
        let n = self.tree.len();
        let next = match cur {
            Some(i) if down => (i + 1).min(n - 1),
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.locks_scroll.scroll_to_item(next);
        self.select_pid(Some(self.tree[next].pid), cx);
    }

    #[cfg(feature = "checks")]
    pub fn set_thresholds(&mut self, th: Thresholds, cx: &mut Context<Self>) {
        self.th = th;
        self.rebuild_sessions(cx);
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub fn demo_statements(&mut self, rows: Vec<StatementRow>, cx: &mut Context<Self>) {
        self.statements_fixed = true;
        self.avail[Section::Statements.ix()] = Avail::Yes;
        self.statements = Some(Statements::Rows(rows));
        self.rebuild_statements(cx);
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub fn tree(&self) -> &[TreeRow] {
        &self.tree
    }

    #[cfg(feature = "checks")]
    pub fn shown_sessions(&self) -> &[SessionRow] {
        &self.shown
    }

    #[cfg(feature = "checks")]
    pub fn statements_state(&self) -> Option<&Statements> {
        self.statements.as_ref()
    }

    #[cfg(feature = "checks")]
    pub fn tables(&self) -> &[TableRow] {
        &self.tables
    }
}

struct Read {
    probe: bool,
    sessions: Option<anyhow::Result<Vec<SessionRow>>>,
    locks: Option<anyhow::Result<Vec<LockEdge>>>,
    tables: Option<anyhow::Result<Vec<TableRow>>>,
    statements: Option<anyhow::Result<Statements>>,
    bloat: bool,
}

fn on_screen(s: &SessionRow, th: &Thresholds) -> (SessionRow, Option<Severity>) {
    let mut c = s.clone();
    c.backend_age = None;
    c.state_age = None;
    (c, s.severity(th))
}

fn stmt_key(r: &StatementRow) -> String {
    match &r.queryid {
        Some(id) => format!("{id}\u{1}{}\u{1}{}", r.user.as_deref().unwrap_or(""), r.db.as_deref().unwrap_or("")),
        None => r.query.clone(),
    }
}

pub(crate) fn filter_sessions(rows: &[SessionRow], idle: bool, background: bool, q: &str) -> Vec<SessionRow> {
    rows.iter()
        .filter(|r| (idle || !r.is_idle()) && (background || !r.is_background()) && (q.is_empty() || r.haystack().contains(q)))
        .cloned()
        .collect()
}

pub(crate) fn offered(avail: &[Avail; 4]) -> Vec<Section> {
    SECTIONS.into_iter().filter(|s| avail[s.ix()] == Avail::Yes).collect()
}

fn info(name: &str, ty: &str, kind: Kind) -> ColumnInfo {
    ColumnInfo { name: name.into(), type_name: ty.into(), type_oid: 0, kind }
}

fn push_int(b: &mut ChunkBuilder, c: usize, v: Option<i64>) {
    match v {
        Some(v) => b.push_i64(c, v),
        None => b.push_null(c),
    }
}

fn push_float(b: &mut ChunkBuilder, c: usize, v: Option<f64>) {
    match v {
        Some(v) => b.push_f64(c, v),
        None => b.push_null(c),
    }
}

fn shown_as(text: String) -> CellDecor {
    CellDecor { text: Some(Some(text)), ..Default::default() }
}

pub(crate) fn state_tone(s: State) -> Tone {
    match s {
        State::Active => Tone::Ok,
        State::IdleInTx => Tone::Warn,
        State::Aborted => Tone::Err,
        State::FastPath => Tone::Info,
        State::Idle | State::Disabled | State::None => Tone::Muted,
    }
}

fn wait_text(s: &SessionRow) -> String {
    match (s.wait_type.as_deref(), s.wait.as_deref()) {
        (Some(t), Some(w)) => format!("{t} · {w}"),
        (a, b) => a.or(b).unwrap_or("").to_string(),
    }
}

pub(crate) const S_APP: usize = 3;
pub(crate) const S_STATE: usize = 5;
const S_QUERY: usize = 11;

pub(crate) fn sessions_result(rows: &[SessionRow], own: &dyn Fn(&SessionRow) -> bool, th: &Thresholds) -> (Arc<ResultSet>, Decorations) {
    let cols = vec![
        info("pid", "int4", Kind::Int),
        info("user", "name", Kind::Text),
        info("database", "name", Kind::Text),
        info("application", "text", Kind::Text),
        info("client", "inet", Kind::Text),
        info("state", "text", Kind::Text),
        info("wait event", "text", Kind::Text),
        info("xact age", "interval", Kind::Int),
        info("query age", "interval", Kind::Int),
        info("backend", "text", Kind::Text),
        info("blocked by", "int4[]", Kind::Text),
        info("query", "text", Kind::Text),
    ];
    let mut b = ChunkBuilder::new(&cols);
    let mut d = Decorations::default();
    for (i, r) in rows.iter().enumerate() {
        b.push_i64(0, r.pid as i64);
        b.push_text(1, r.user.as_deref().unwrap_or(""));
        b.push_text(2, r.db.as_deref().unwrap_or(""));
        b.push_text(3, r.app.as_deref().unwrap_or(""));
        b.push_text(4, r.client.as_deref().unwrap_or(""));
        b.push_text(5, r.state.as_deref().unwrap_or(""));
        b.push_text(6, &wait_text(r));
        push_int(&mut b, 7, r.xact_age);
        push_int(&mut b, 8, r.query_age);
        b.push_text(9, r.backend_type.as_deref().unwrap_or(""));
        b.push_text(10, &r.blocked_by.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "));
        b.push_text(11, r.query.as_deref().map(str::trim).unwrap_or(""));
        b.end_row();
        d.cells.insert((i, S_STATE), CellDecor { color: Some(state_tone(r.state_kind())), ..Default::default() });
        d.cells.insert((i, 7), shown_as(r.xact_age.map(act::fmt_age).unwrap_or_default()));
        d.cells.insert((i, 8), shown_as(r.query_age.map(act::fmt_age).unwrap_or_default()));
        if own(r) {
            d.cells.insert((i, S_APP), CellDecor { text: Some(Some("this app".into())), color: Some(Tone::Acc), ..Default::default() });
        }
        if r.query.as_deref().is_some_and(act::is_withheld) {
            d.cells.insert((i, S_QUERY), CellDecor { ghost: true, ..Default::default() });
        }
        if let Some(sev) = r.severity(th) {
            let tone = if sev == Severity::Err { Tone::Err } else { Tone::Warn };
            d.rows.insert(i, RowDecor { tint: Some(tone), ..Default::default() });
        }
    }
    (ResultSet::from_chunk("pg_stat_activity", cols, b.finish()), d)
}

pub(crate) fn tables_result(rows: &[TableRow]) -> (Arc<ResultSet>, Decorations) {
    let cols = vec![
        info("schema", "name", Kind::Text),
        info("table", "name", Kind::Text),
        info("live", "int8", Kind::Int),
        info("dead", "int8", Kind::Int),
        info("dead %", "float8", Kind::Float),
        info("last vacuum", "interval", Kind::Int),
        info("last autovacuum", "interval", Kind::Int),
        info("last analyze", "interval", Kind::Int),
        info("last autoanalyze", "interval", Kind::Int),
        info("seq scan %", "float8", Kind::Float),
        info("table size", "int8", Kind::Int),
        info("index size", "int8", Kind::Int),
        info("mod since analyze", "int8", Kind::Int),
        info("~bloat", "int8", Kind::Int),
        info("~bloat %", "float8", Kind::Float),
    ];
    let mut b = ChunkBuilder::new(&cols);
    let mut d = Decorations::default();
    let size = |v: Option<i64>| v.map(dataman_core::schema::objects::human_size).unwrap_or_default();
    for (i, t) in rows.iter().enumerate() {
        b.push_text(0, &t.schema);
        b.push_text(1, &t.name);
        push_int(&mut b, 2, t.live);
        push_int(&mut b, 3, t.dead);
        push_float(&mut b, 4, t.dead_ratio().map(|x| x * 100.));
        for (c, age) in [(5, t.vacuum_age), (6, t.autovacuum_age), (7, t.analyze_age), (8, t.autoanalyze_age)] {
            push_int(&mut b, c, age);
            d.cells.insert(
                (i, c),
                match age {
                    Some(a) => shown_as(act::fmt_age_coarse(a)),
                    None => CellDecor { text: Some(Some("never".into())), ghost: true, ..Default::default() },
                },
            );
        }
        push_float(&mut b, 9, t.seq_ratio().map(|x| x * 100.));
        push_int(&mut b, 10, t.table_bytes);
        push_int(&mut b, 11, t.index_bytes);
        push_int(&mut b, 12, t.mod_since_analyze);
        push_int(&mut b, 13, t.bloat_bytes);
        push_float(&mut b, 14, t.bloat_pct);
        b.end_row();
        let dead = t.dead_ratio();
        d.cells.insert(
            (i, 4),
            CellDecor { text: Some(Some(dead.map(act::fmt_pct).unwrap_or_default())), color: dead.filter(|x| *x >= 0.2).map(|_| Tone::Warn), ..Default::default() },
        );
        d.cells.insert((i, 9), shown_as(t.seq_ratio().map(act::fmt_pct).unwrap_or_default()));
        d.cells.insert((i, 10), shown_as(size(t.table_bytes)));
        d.cells.insert((i, 11), shown_as(size(t.index_bytes)));
        d.cells.insert((i, 13), shown_as(t.bloat_bytes.map(|v| format!("~{}", dataman_core::schema::objects::human_size(v))).unwrap_or_default()));
        d.cells.insert(
            (i, 14),
            CellDecor {
                text: Some(Some(t.bloat_pct.map(|p| format!("~{}", act::fmt_pct(p / 100.))).unwrap_or_default())),
                color: t.bloat_pct.filter(|p| *p >= 20.).map(|_| Tone::Warn),
                ..Default::default()
            },
        );
    }
    (ResultSet::from_chunk("pg_stat_user_tables", cols, b.finish()), d)
}

pub(crate) fn statements_result(rows: &[StatementRow]) -> (Arc<ResultSet>, Decorations) {
    let cols = vec![
        info("query", "text", Kind::Text),
        info("calls", "int8", Kind::Int),
        info("total", "float8", Kind::Float),
        info("mean", "float8", Kind::Float),
        info("rows", "int8", Kind::Int),
        info("hit %", "float8", Kind::Float),
        info("temp blocks", "int8", Kind::Int),
        info("user", "name", Kind::Text),
        info("database", "name", Kind::Text),
    ];
    let mut b = ChunkBuilder::new(&cols);
    let mut d = Decorations::default();
    for (i, r) in rows.iter().enumerate() {
        b.push_text(0, r.query.trim());
        b.push_i64(1, r.calls);
        b.push_f64(2, r.total_ms);
        b.push_f64(3, r.mean_ms);
        b.push_i64(4, r.rows);
        push_float(&mut b, 5, r.hit_ratio().map(|x| x * 100.));
        b.push_i64(6, r.temp_blocks);
        b.push_text(7, r.user.as_deref().unwrap_or(""));
        b.push_text(8, r.db.as_deref().unwrap_or(""));
        b.end_row();
        d.cells.insert((i, 2), shown_as(act::fmt_ms(r.total_ms)));
        d.cells.insert((i, 3), shown_as(act::fmt_ms(r.mean_ms)));
        d.cells.insert((i, 5), shown_as(r.hit_ratio().map(act::fmt_pct).unwrap_or_default()));
        if r.query.trim().is_empty() || act::is_withheld(&r.query) {
            d.cells.insert((i, 0), CellDecor { ghost: true, ..Default::default() });
        }
    }
    (ResultSet::from_chunk("pg_stat_statements", cols, b.finish()), d)
}

fn small(b: gpui::Stateful<gpui::Div>) -> gpui::Stateful<gpui::Div> {
    b.h(px(24.)).px(px(8.)).gap(px(6.)).text_size(px(11.5))
}

fn chip(t: &Theme, id: &'static str, label: &'static str, on: bool) -> gpui::Stateful<gpui::Div> {
    let hov = t.hov;
    div()
        .id(id)
        .flex_none()
        .h(px(22.))
        .px(px(8.))
        .flex()
        .items_center()
        .border_1()
        .text_size(px(11.))
        .when(on, |d| d.bg(t.prs).text_color(t.tx).border_color(t.line2))
        .when(!on, |d| d.text_color(t.tx3).border_color(t.line).hover(move |s| s.bg(hov)))
        .child(label)
}

pub(crate) fn own_tag(t: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .h(px(17.))
        .px(px(6.))
        .flex()
        .items_center()
        .bg(t.acc_s)
        .text_color(t.acc_t)
        .text_size(px(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .child("this app")
}

impl ActivityView {
    fn render_header(&self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let offered = offered(&self.avail);
        let count = |s: Section| -> Option<String> {
            match s {
                Section::Sessions => Some(self.shown.len().to_string()),
                Section::Locks => {
                    let n = self.tree.iter().filter(|r| r.depth > 0 && !r.again).count();
                    (n > 0).then(|| n.to_string())
                }
                Section::Tables => Some(self.tables.len().to_string()),
                Section::Statements => match &self.statements {
                    Some(Statements::Rows(r)) => Some(r.len().to_string()),
                    _ => None,
                },
            }
        };
        let tabs = (offered.len() > 0).then(|| {
            ui::segmented(t).flex_none().h(px(28.)).children(offered.iter().map(|&s| {
                let on = self.section == s;
                let warn = s == Section::Locks && self.tree.iter().any(|r| r.depth > 0);
                ui::segment(t, ("act-section", s.ix()), s.label(), on)
                    .flex_none()
                    .gap(px(6.))
                    .children(count(s).map(|n| div().text_size(px(11.)).text_color(if warn { t.warn } else if on { t.tx3 } else { t.tx4 }).child(n)))
                    .on_click(cx.listener(move |m, _: &ClickEvent, window, cx| m.set_section(s, window, cx)))
            }))
        });
        let filter_shown = self.section == Section::Sessions && self.avail[0] == Avail::Yes;
        let filters = filter_shown.then(|| {
            let focused = self.filter.focus_handle(cx).is_focused(window);
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(90.))
                        .max_w(px(220.))
                        .h(px(26.))
                        .px(px(8.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(7.))
                        .relative()
                        .border_1()
                        .border_color(if focused { t.acc } else { t.field_b })
                        .when(focused, |d| d.child(crate::ui::halo_ring(t.acc_s)))
                        .child(icon(Icon::Search).size(px(13.)).text_color(t.tx3))
                        .child(div().flex_1().min_w_0().text_color(t.tx).child(self.filter.clone())),
                )
                .child(chip(t, "act-idle", "idle", self.show_idle).on_click(cx.listener(|m, _: &ClickEvent, _, cx| {
                    let (i, b) = (!m.show_idle, m.show_background);
                    m.set_filters(i, b, cx)
                })))
                .child(chip(t, "act-bg", "background", self.show_background).on_click(cx.listener(|m, _: &ClickEvent, _, cx| {
                    let (i, b) = (m.show_idle, !m.show_background);
                    m.set_filters(i, b, cx)
                })))
        });
        let next = EVERY[(EVERY.iter().position(|e| *e == self.every).unwrap_or(0) + 1) % EVERY.len()];
        let every = chip(t, "act-every", self.every.label(), self.every != Every::Off)
            .on_click(cx.listener(move |m, _: &ClickEvent, _, cx| m.set_every(next, cx)));
        div()
            .h(px(metrics::TOOLBAR_H))
            .flex_none()
            .pl(px(10.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .border_b_1()
            .border_color(t.line)
            .text_color(t.tx2)
            .overflow_hidden()
            .children(tabs)
            .children(filters)
            .when(!filter_shown, |d| d.child(div().flex_1()))
            .child(every.flex_none())
            .child(
                ui::icon_button(t, "act-refresh", Icon::Reload, 26., 14.)
                    .on_click(cx.listener(|m, _: &ClickEvent, _, cx| m.refresh_now(cx))),
            )
    }

    fn render_locks(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.tree.is_empty() {
            return div().size_full().flex().items_center().justify_center().child(ui::empty_state(t, "[ NO WAITS ]", 15., None)).into_any_element();
        }
        let by_pid: HashMap<i32, &SessionRow> = self.sessions.iter().map(|s| (s.pid, s)).collect();
        let held: HashMap<i32, &LockEdge> = self.tree.iter().filter_map(|r| r.edge.as_ref()).fold(HashMap::new(), |mut m, e| {
            m.entry(e.blocker).or_insert(e);
            m
        });
        const W_PID: f32 = 150.;
        const W_USER: f32 = 100.;
        const W_STATE: f32 = 150.;
        const W_LOCK: f32 = 210.;
        const W_REL: f32 = 170.;
        const W_WAIT: f32 = 76.;
        let cell = |w: f32| div().w(px(w)).flex_none().pr(px(10.)).overflow_hidden().whitespace_nowrap().text_ellipsis();
        let head = div()
            .h(px(28.))
            .px(px(12.))
            .flex()
            .flex_row()
            .items_center()
            .text_size(px(10.5))
            .text_color(t.tx3)
            .border_b_1()
            .border_color(t.line)
            .child(cell(W_PID).child("pid"))
            .child(cell(W_USER).child("user"))
            .child(cell(W_STATE).child("state"))
            .child(cell(W_LOCK).child("lock"))
            .child(cell(W_REL).child("relation"))
            .child(cell(W_WAIT).child("age"))
            .child(div().flex_1().child("query"));
        let rows = self.tree.iter().enumerate().map(|(i, r)| {
            let s = by_pid.get(&r.pid).copied();
            let sel = self.sel_pid == Some(r.pid) && self.tree.iter().position(|x| x.pid == r.pid) == Some(i);
            let own = s.is_some_and(|s| self.owns(s));
            let top = r.depth == 0;
            let (lock, rel, wait) = match &r.edge {
                Some(e) => (
                    format!("{}{}", e.mode.as_deref().unwrap_or(""), e.locktype.as_deref().filter(|l| *l != "relation").map(|l| format!(" · {l}")).unwrap_or_default()),
                    e.relation.clone().unwrap_or_default(),
                    e.wait.map(act::fmt_age).unwrap_or_default(),
                ),
                None => {
                    let e = held.get(&r.pid);
                    (
                        e.and_then(|e| e.held.clone()).unwrap_or_default(),
                        e.and_then(|e| e.relation.clone()).unwrap_or_default(),
                        s.and_then(|s| s.xact_age).map(act::fmt_age).unwrap_or_default(),
                    )
                }
            };
            let dim = r.again;
            let fg = if dim { t.tx3 } else { t.tx };
            let glyph = if r.cycle && r.again {
                "↻"
            } else if top {
                "▪"
            } else {
                "└"
            };
            let state = s.map(|s| s.state_kind()).unwrap_or(State::None);
            let hov = t.hov;
            let pid = r.pid;
            let group: SharedString = format!("lock-row-{i}").into();
            div()
                .id(("lock-row", i))
                .group(group.clone())
                .h(px(26.))
                .px(px(12.))
                .flex()
                .flex_row()
                .items_center()
                .text_size(px(12.))
                .when(sel, |d| d.bg(t.sel))
                .when(!sel, |d| d.hover(move |s| s.bg(hov)))
                .on_click(cx.listener(move |m, ev: &ClickEvent, window, cx| {
                    window.focus(&m.locks_focus, cx);
                    m.select_pid(Some(pid), cx);
                    if ev.click_count() == 2 {
                        cx.emit(ActivityEvent::Details { toggle: false });
                    }
                }))
                .child(
                    cell(W_PID)
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .pl(px(16. * r.depth as f32))
                        .child(div().flex_none().text_color(if r.cycle { t.err } else if top { t.warn } else { t.tx4 }).child(glyph))
                        .child(div().flex_none().text_color(fg).when(top, |d| d.font_weight(FontWeight::SEMIBOLD)).child(pid.to_string()))
                        .when(top && r.behind > 0, |d| d.child(div().flex_none().text_size(px(10.5)).text_color(t.warn).child(format!("+{}", r.behind))))
                        .when(own, |d| d.child(div().flex_none().size(px(6.)).rounded(px(3.)).bg(t.acc))),
                )
                .child(cell(W_USER).text_color(if dim { t.tx3 } else { t.tx2 }).child(s.and_then(|s| s.user.clone()).unwrap_or_default()))
                .child(cell(W_STATE).text_color(tone_strong(t, state_tone(state))).child(s.and_then(|s| s.state.clone()).unwrap_or_default()))
                .child(cell(W_LOCK).text_color(fg).child(lock))
                .child(cell(W_REL).text_color(if dim { t.tx3 } else { t.tx2 }).child(rel))
                .child(cell(W_WAIT).text_color(fg).child(wait))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(t.tx3)
                        .child(s.and_then(|s| s.query.as_deref().map(|q| q.split_whitespace().collect::<Vec<_>>().join(" "))).unwrap_or_default()),
                )
                .when(top, |d| {
                    let shown = sel;
                    d.child(
                        div()
                            .flex_none()
                            .flex()
                            .flex_row()
                            .gap(px(4.))
                            .ml(px(8.))
                            .when(!shown, |d| d.invisible().group_hover(group.clone(), |s| s.visible()))
                            .child(small(ui::button(t, ("lock-cancel", i), Btn::Ghost)).child("Cancel").on_click(cx.listener(move |m, _: &ClickEvent, _, cx| {
                                m.select_pid(Some(pid), cx);
                                m.request_signal(Signal::Cancel, cx);
                            })))
                            .child(small(ui::button(t, ("lock-terminate", i), Btn::Ghost)).text_color(t.err).child("Terminate").on_click(cx.listener(
                                move |m, _: &ClickEvent, _, cx| {
                                    m.select_pid(Some(pid), cx);
                                    m.request_signal(Signal::Terminate, cx);
                                },
                            ))),
                    )
                })
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context(LOCKS_CONTEXT)
            .track_focus(&self.locks_focus)
            .on_action(cx.listener(|m, _: &LockUp, _, cx| m.move_lock(false, cx)))
            .on_action(cx.listener(|m, _: &LockDown, _, cx| m.move_lock(true, cx)))
            .child(head)
            .child(div().id("lock-rows").flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.locks_scroll).children(rows))
            .into_any_element()
    }

    fn render_statements_state(&self, t: &Theme, st: &Statements, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (label, color) = match st {
            Statements::NotInstalled => ("not installed".to_string(), t.tx3),
            Statements::NotLoaded => ("not loaded".to_string(), t.warn),
            Statements::Failed(e) => (e.lines().next().unwrap_or("").to_string(), t.err),
            Statements::Rows(_) => (String::new(), t.tx3),
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .child(div().text_color(t.tx2).child("pg_stat_statements"))
                    .child(div().text_color(color).child(label))
                    .child(
                        ui::button(t, "act-create-pgss", Btn::Secondary)
                            .h(px(26.))
                            .child(act::CREATE_PGSS.trim_end_matches(';').replace(" IF NOT EXISTS", ""))
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(ActivityEvent::OpenSql { sql: act::CREATE_PGSS.to_string(), explain: false }))),
                    ),
            )
            .into_any_element()
    }

    fn render_bar(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let btn = |id: &'static str, label: &'static str, on: bool| {
            small(ui::button(t, id, Btn::Ghost)).when(!on, |d| d.text_color(t.tx4)).child(label)
        };
        let mut left: Vec<gpui::AnyElement> = Vec::new();
        let mut right: Vec<gpui::AnyElement> = Vec::new();
        let details = || ui::icon_button(t, "act-details", Icon::PanelRight, 24., 14.).on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(ActivityEvent::Details { toggle: true })));
        match self.section {
            Section::Sessions | Section::Locks => {
                let count = if self.section == Section::Sessions {
                    format!("{} of {}", self.shown.len(), self.sessions.len())
                } else {
                    let blockers = self.tree.iter().filter(|r| r.depth == 0).count();
                    let waiting = self.tree.iter().filter(|r| r.depth > 0 && !r.again).count();
                    if blockers == 0 { String::new() } else { format!("{waiting} waiting · {blockers} blocking") }
                };
                left.push(div().text_color(t.tx3).child(count).into_any_element());
                let sel = self.selected_session();
                if let Some(s) = sel {
                    left.push(div().text_color(t.tx2).child(format!("pid {}", s.pid)).into_any_element());
                    if self.owns(s) {
                        left.push(own_tag(t).into_any_element());
                    }
                }
                let on = sel.is_some();
                let q = sel.is_some_and(|s| s.visible_query().is_some());
                right.push(btn("act-cancel", "Cancel", on).on_click(cx.listener(|m, _: &ClickEvent, _, cx| m.request_signal(Signal::Cancel, cx))).into_any_element());
                right.push(
                    btn("act-terminate", "Terminate", on)
                        .when(on, |d| d.text_color(t.err))
                        .on_click(cx.listener(|m, _: &ClickEvent, _, cx| m.request_signal(Signal::Terminate, cx)))
                        .into_any_element(),
                );
                right.push(ui::vsep(t, 16.).mx(px(4.)).into_any_element());
                right.push(btn("act-copy", "Copy query", q).on_click(cx.listener(|m, _: &ClickEvent, _, cx| m.copy_query(cx))).into_any_element());
                right.push(btn("act-explain", "Explain", q).on_click(cx.listener(|m, _: &ClickEvent, _, cx| m.open_query(true, cx))).into_any_element());
                right.push(btn("act-open", "Open in editor", q).on_click(cx.listener(|m, _: &ClickEvent, _, cx| m.open_query(false, cx))).into_any_element());
                right.push(details().into_any_element());
            }
            Section::Tables => {
                left.push(div().text_color(t.tx3).child(format!("{} tables", self.tables.len())).into_any_element());
                if let Some(tb) = self.selected_table() {
                    left.push(div().text_color(t.tx2).child(format!("{}.{}", tb.schema, tb.name)).into_any_element());
                }
                let on = self.selected_table().is_some();
                for (id, label, m) in [
                    ("act-vacuum", "VACUUM", Maintenance::Vacuum),
                    ("act-analyze", "ANALYZE", Maintenance::Analyze),
                    ("act-vacuum-analyze", "VACUUM ANALYZE", Maintenance::VacuumAnalyze),
                ] {
                    right.push(btn(id, label, on).on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.maintenance(m, cx))).into_any_element());
                }
            }
            Section::Statements if !matches!(self.statements, Some(Statements::Rows(_))) => {}
            Section::Statements => {
                if let Some(Statements::Rows(r)) = &self.statements {
                    left.push(div().text_color(t.tx3).child(format!("{} statements", r.len())).into_any_element());
                }
                let on = self.selected_statement().is_some();
                right.push(btn("act-copy", "Copy query", on).on_click(cx.listener(|m, _: &ClickEvent, _, cx| m.copy_query(cx))).into_any_element());
                right.push(btn("act-explain", "Explain", on).on_click(cx.listener(|m, _: &ClickEvent, _, cx| m.open_query(true, cx))).into_any_element());
                right.push(btn("act-open", "Open in editor", on).on_click(cx.listener(|m, _: &ClickEvent, _, cx| m.open_query(false, cx))).into_any_element());
                right.push(ui::vsep(t, 16.).mx(px(4.)).into_any_element());
                right.push(
                    btn("act-reset", "Reset…", true)
                        .text_color(t.err)
                        .on_click(cx.listener(|m, _: &ClickEvent, _, cx| m.request_reset(cx)))
                        .into_any_element(),
                );
                right.push(details().into_any_element());
            }
        }
        div()
            .h(px(metrics::RESULT_BAR_H))
            .flex_none()
            .pl(px(12.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .border_t_1()
            .border_color(t.line)
            .text_size(px(11.5))
            .whitespace_nowrap()
            .overflow_hidden()
            .children(left)
            .child(div().flex_1())
            .child(div().flex().flex_row().items_center().gap(px(2.)).children(right))
    }
}

impl Render for ActivityView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.renders += 1;
        let t = cx.theme();
        let offered = offered(&self.avail);
        let content: gpui::AnyElement = if self.detached {
            div().size_full().flex().items_center().justify_center().child(ui::empty_state(&t, "[ NOT CONNECTED ]", 15., None)).into_any_element()
        } else if self.probed && offered.is_empty() {
            div().size_full().flex().items_center().justify_center().child(ui::empty_state(&t, "[ NOT REPORTED ]", 15., None)).into_any_element()
        } else if !self.probed {
            div().size_full().into_any_element()
        } else {
            match self.section {
                Section::Sessions => self.sessions_grid.clone().into_any_element(),
                Section::Locks => self.render_locks(&t, cx),
                Section::Tables => self.tables_grid.clone().into_any_element(),
                Section::Statements => match self.statements.clone() {
                    Some(Statements::Rows(_)) => self.statements_grid.clone().into_any_element(),
                    Some(st) => self.render_statements_state(&t, &st, cx),
                    None => div().size_full().into_any_element(),
                },
            }
        };
        let bar = (self.probed && !offered.is_empty() && !self.detached).then(|| self.render_bar(&t, cx));
        div()
            .size_full()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .font_family(mono())
            .text_size(px(12.))
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|m, _: &Refresh, _, cx| m.refresh_now(cx)))
            .child(self.render_header(&t, window, cx))
            .child(div().flex_1().min_h_0().relative().overflow_hidden().child(content))
            .children(bar)
    }
}

pub enum DetailsEvent {
    Close,
}

pub struct ActivityDetails {
    monitor: WeakEntity<ActivityView>,
    scroll: ScrollHandle,
    focus: FocusHandle,
}

impl EventEmitter<DetailsEvent> for ActivityDetails {}

impl Focusable for ActivityDetails {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn kv(t: &Theme, label: &str, value: impl Into<SharedString>) -> gpui::Div {
    kv_in(t, label, value, t.tx)
}

fn kv_in(t: &Theme, label: &str, value: impl Into<SharedString>, color: gpui::Hsla) -> gpui::Div {
    div()
        .min_h(px(22.))
        .flex()
        .flex_row()
        .items_start()
        .gap(px(10.))
        .child(div().w(px(104.)).flex_none().text_color(t.tx3).child(label.to_string()))
        .child(div().flex_1().min_w_0().text_color(color).child(value.into()))
}

fn query_block(t: &Theme, sql: &str) -> gpui::Div {
    let lines = crate::editor::highlighted_lines(t, sql.trim());
    div()
        .px(px(12.))
        .py(px(10.))
        .bg(t.card)
        .border_1()
        .border_color(t.line)
        .flex()
        .flex_col()
        .text_size(px(12.))
        .line_height(px(18.))
        .children(lines.into_iter().map(|(text, runs)| {
            if text.is_empty() {
                div().h(px(18.)).into_any_element()
            } else {
                div().child(StyledText::new(text).with_runs(runs)).into_any_element()
            }
        }))
}

impl Render for ActivityDetails {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let Some(m) = self.monitor.upgrade() else { return div().size_full() };
        let m = m.read(cx);
        let mut title: Vec<gpui::AnyElement> = Vec::new();
        let mut body: Vec<gpui::AnyElement> = Vec::new();
        let caps = match m.section {
            Section::Statements => "STATEMENT",
            _ => "SESSION",
        };
        match m.section {
            Section::Sessions | Section::Locks => {
                if let Some(s) = m.selected_session() {
                    title.push(div().text_color(t.tx).child(format!("pid {}", s.pid)).into_any_element());
                    if m.owns(s) {
                        title.push(own_tag(&t).into_any_element());
                    }
                    let age = |v: Option<i64>| v.map(act::fmt_age).unwrap_or_default();
                    let mut rows = vec![
                        kv_in(&t, "state", s.state.clone().unwrap_or_default(), tone_strong(&t, state_tone(s.state_kind()))),
                        kv(&t, "user", s.user.clone().unwrap_or_default()),
                        kv(&t, "database", s.db.clone().unwrap_or_default()),
                        kv(&t, "application", s.app.clone().unwrap_or_default()),
                        kv(&t, "client", s.client.clone().unwrap_or_default()),
                        kv(&t, "backend", s.backend_type.clone().unwrap_or_default()),
                        kv(&t, "wait event", wait_text(s)),
                        kv(&t, "connected", age(s.backend_age)),
                        kv(&t, "xact age", age(s.xact_age)),
                        kv(&t, "query age", age(s.query_age)),
                        kv(&t, "state age", age(s.state_age)),
                    ];
                    if !s.blocked_by.is_empty() {
                        rows.push(kv_in(&t, "blocked by", s.blocked_by.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "), t.warn));
                    }
                    body.push(div().flex().flex_col().children(rows).into_any_element());
                    match s.query.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
                        Some(q) if act::is_withheld(q) => body.push(div().italic().text_color(t.tx3).child(q.to_string()).into_any_element()),
                        Some(q) => body.push(query_block(&t, q).into_any_element()),
                        None => {}
                    }
                }
            }
            Section::Statements => {
                if let Some(r) = m.selected_statement() {
                    title.push(div().text_color(t.tx2).child(r.queryid.clone().unwrap_or_default()).into_any_element());
                    let ms = |v: Option<f64>| v.map(act::fmt_ms).unwrap_or_default();
                    let mut rows = vec![
                        kv(&t, "calls", act::fmt_count(r.calls)),
                        kv(&t, "total", act::fmt_ms(r.total_ms)),
                        kv(&t, "mean", act::fmt_ms(r.mean_ms)),
                        kv(&t, "min", ms(r.min_ms)),
                        kv(&t, "max", ms(r.max_ms)),
                        kv(&t, "stddev", ms(r.stddev_ms)),
                        kv(&t, "rows", act::fmt_count(r.rows)),
                        kv(&t, "rows / call", if r.calls > 0 { format!("{:.1}", r.rows as f64 / r.calls as f64) } else { String::new() }),
                        kv(&t, "hit %", r.hit_ratio().map(act::fmt_pct).unwrap_or_default()),
                        kv(&t, "shared hit", act::fmt_count(r.shared_hit)),
                        kv(&t, "shared read", act::fmt_count(r.shared_read)),
                        kv(&t, "temp blocks", act::fmt_count(r.temp_blocks)),
                    ];
                    if let Some(p) = r.plans.filter(|p| *p > 0) {
                        rows.push(kv(&t, "plans", act::fmt_count(p)));
                        rows.push(kv(&t, "planning", ms(r.plan_ms)));
                    }
                    rows.push(kv(&t, "user", r.user.clone().unwrap_or_default()));
                    rows.push(kv(&t, "database", r.db.clone().unwrap_or_default()));
                    body.push(div().flex().flex_col().children(rows).into_any_element());
                    if !r.query.trim().is_empty() {
                        body.push(query_block(&t, &r.query).into_any_element());
                    }
                }
            }
            Section::Tables => {}
        }
        let header = div()
            .h(px(40.))
            .flex_none()
            .pl(px(14.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .border_b_1()
            .border_color(t.line)
            .child(ui::caps(&t, caps))
            .children(title)
            .child(div().flex_1())
            .child(ui::icon_button(&t, "act-details-close", Icon::Close, 24., 13.).on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(DetailsEvent::Close))));
        let content: gpui::AnyElement = if body.is_empty() {
            div().flex_1().flex().items_center().justify_center().child(ui::empty_state(&t, "[ NO SELECTION ]", 13., None)).into_any_element()
        } else {
            div()
                .id("act-details-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .px(px(14.))
                .py(px(12.))
                .flex()
                .flex_col()
                .gap(px(14.))
                .children(body)
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .font_family(mono())
            .text_size(px(12.))
            .child(header)
            .child(content)
    }
}

impl crate::app::Workspace {
    pub(crate) fn open_activity(&mut self, conn_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.store.read(cx).connection(conn_id) else {
            self.notice("Connect first", cx);
            return;
        };
        if !conn.caps().activity {
            return;
        }
        if let Some(ix) = self.tabs.iter().position(|t| t.read(cx).monitor.as_ref().is_some_and(|m| m.read(cx).conn.profile.id == conn_id)) {
            self.activate(ix, window, cx);
            return;
        }
        self.current_conn = Some(conn_id.to_string());
        let tab = self.new_tab(window, cx);
        let db = conn.database().to_string();
        let m = cx.new(|cx| ActivityView::new(conn.clone(), db, cx));
        tab.update(cx, |t, cx| {
            t.set_connection(Some(conn), cx);
            t.attach_monitor(m.clone(), cx);
        });
        let tab_id = tab.entity_id();
        cx.subscribe_in(&m, window, move |w, m, ev: &ActivityEvent, window, cx| match ev {
            ActivityEvent::OpenSql { sql, explain } => {
                let (conn, db) = {
                    let m = m.read(cx);
                    (m.conn.profile.id.clone(), m.db.clone())
                };
                if let Some(tab) = w.open_sql_tab(&conn, Some(&db), sql, false, window, cx)
                    && *explain
                {
                    tab.update(cx, |t, cx| t.explain(false, cx));
                }
            }
            ActivityEvent::Notice(msg) => w.notice(msg.clone(), cx),
            ActivityEvent::Details { toggle } => {
                let open = matches!(&w.side, Some(crate::app::Side::Feature("activity", _, crate::app::Bound::Tab(id))) if *id == tab_id);
                if open && *toggle {
                    w.side = None;
                } else if !open {
                    let view = m.read(cx).details.clone();
                    w.side_w = w.side_w.max(400.);
                    w.side = Some(crate::app::Side::Feature("activity", view.into(), crate::app::Bound::Tab(tab_id)));
                }
                cx.notify();
            }
            ActivityEvent::Confirm(_) => {}
        })
        .detach();
        let details = m.read(cx).details.clone();
        cx.subscribe(&details, |w, _, _: &DetailsEvent, cx| {
            if matches!(w.side, Some(crate::app::Side::Feature("activity", ..))) {
                w.side = None;
                cx.notify();
            }
        })
        .detach();
        self.sync_monitors(window, cx);
        let f = m.read(cx).focus_target(cx);
        window.focus(&f, cx);
        cx.notify();
    }

    pub(crate) fn sync_monitors(&mut self, window: &Window, cx: &mut Context<Self>) {
        let active_window = self.assume_active.unwrap_or_else(|| window.is_window_active());
        let first_run = self.first_run(cx);
        for (i, tab) in self.tabs.clone().into_iter().enumerate() {
            let Some(m) = tab.read(cx).monitor.clone() else { continue };
            let conn = tab.read(cx).connection().cloned();
            match &conn {
                Some(c) if !Arc::ptr_eq(c, &m.read(cx).conn) => m.update(cx, |m, cx| m.rebind(c.clone(), cx)),
                _ => {}
            }
            if m.read(cx).detached != conn.is_none() {
                m.update(cx, |m, cx| {
                    m.detached = conn.is_none();
                    cx.notify();
                });
            }
            let on = i == self.active && active_window && conn.is_some() && !first_run;
            if m.read(cx).is_visible() != on {
                m.update(cx, |m, cx| m.set_visible(on, cx));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(pid: i32, state: &str, backend: &str) -> SessionRow {
        SessionRow {
            pid,
            user: Some("app".into()),
            db: Some("shop".into()),
            app: Some("psql".into()),
            state: (!state.is_empty()).then(|| state.into()),
            backend_type: Some(backend.into()),
            query: Some(format!("SELECT {pid}")),
            ..Default::default()
        }
    }

    #[test]
    fn default_filter_hides_idle_and_background() {
        let rows = vec![
            session(1, "active", "client backend"),
            session(2, "idle", "client backend"),
            session(3, "", "checkpointer"),
            session(4, "idle in transaction", "client backend"),
        ];
        let pids = |v: Vec<SessionRow>| v.iter().map(|s| s.pid).collect::<Vec<_>>();
        assert_eq!(pids(filter_sessions(&rows, false, false, "")), vec![1, 4]);
        assert_eq!(pids(filter_sessions(&rows, true, false, "")), vec![1, 2, 4]);
        assert_eq!(pids(filter_sessions(&rows, true, true, "")), vec![1, 2, 3, 4]);
        assert_eq!(pids(filter_sessions(&rows, true, true, "select 2")), vec![2]);
        assert_eq!(pids(filter_sessions(&rows, false, false, "select 2")), Vec::<i32>::new(), "the toggles still apply");
    }

    #[test]
    fn sessions_grid_marks_and_tints() {
        let mut long = session(10, "active", "client backend");
        long.query_age = Some(400);
        let mut idle_tx = session(11, "idle in transaction", "client backend");
        idle_tx.state_age = Some(45);
        let mut mine = session(12, "idle", "client backend");
        mine.app = Some("Dataman".into());
        let mut hidden = session(13, "active", "client backend");
        hidden.query = Some("<insufficient privilege>".into());
        let rows = vec![long, idle_tx, mine, hidden];
        let (rs, d) = sessions_result(&rows, &|s: &SessionRow| s.pid == 12, &Thresholds::default());
        let snap = rs.snapshot();
        assert_eq!(snap.rows(), 4);
        assert_eq!(snap.columns.len(), 12);
        assert_eq!(snap.text(0, 0).as_deref(), Some("10"));
        assert_eq!(snap.text(0, 8).as_deref(), Some("400"), "the age sorts as seconds");
        assert_eq!(d.cells[&(0, 8)].text, Some(Some("6m 40s".into())), "and reads as an age");
        assert_eq!(d.cells[&(0, 7)].text, Some(Some(String::new())), "no transaction: nothing");
        assert_eq!(d.rows[&0].tint, Some(Tone::Err), "active 400 s");
        assert_eq!(d.rows[&1].tint, Some(Tone::Warn), "idle in transaction 45 s");
        assert!(!d.rows.contains_key(&2) && !d.rows.contains_key(&3));
        assert_eq!(d.cells[&(0, S_STATE)].color, Some(Tone::Ok));
        assert_eq!(d.cells[&(1, S_STATE)].color, Some(Tone::Warn));
        assert_eq!(d.cells[&(2, S_STATE)].color, Some(Tone::Muted));
        assert_eq!(d.cells[&(2, S_APP)].text, Some(Some("this app".into())), "this process's backend");
        assert!(!d.cells.contains_key(&(0, S_APP)));
        assert!(d.cells[&(3, S_QUERY)].ghost, "a withheld query is muted, shown as the server sent it");
        assert_eq!(snap.text(3, S_QUERY).as_deref(), Some("<insufficient privilege>"));
    }

    #[test]
    fn tables_and_statements_grids() {
        let t = TableRow {
            schema: "public".into(),
            name: "orders".into(),
            live: Some(700),
            dead: Some(300),
            autovacuum_age: Some(7200),
            table_bytes: Some(8 * 1024 * 1024),
            bloat_bytes: Some(2 * 1024 * 1024),
            bloat_pct: Some(25.),
            ..Default::default()
        };
        let (rs, d) = tables_result(&[t]);
        let snap = rs.snapshot();
        assert_eq!(snap.columns[13].name, "~bloat", "the estimate says so");
        assert_eq!(d.cells[&(0, 4)].text, Some(Some("30%".into())));
        assert_eq!(d.cells[&(0, 4)].color, Some(Tone::Warn));
        assert_eq!(d.cells[&(0, 5)].text, Some(Some("never".into())));
        assert!(d.cells[&(0, 5)].ghost);
        assert_eq!(d.cells[&(0, 6)].text, Some(Some("2h".into())));
        assert_eq!(d.cells[&(0, 10)].text, Some(Some("8 MB".into())));
        assert_eq!(d.cells[&(0, 13)].text, Some(Some("~2 MB".into())));
        assert_eq!(d.cells[&(0, 14)].text, Some(Some("~25%".into())));
        assert_eq!(snap.text(0, 10).as_deref(), Some("8388608"), "sizes sort as bytes");

        let s = StatementRow { query: "SELECT $1".into(), calls: 3, total_ms: 1500., mean_ms: 500., rows: 3, shared_hit: 9, shared_read: 1, ..Default::default() };
        let (rs, d) = statements_result(&[s]);
        assert_eq!(rs.snapshot().text(0, 1).as_deref(), Some("3"));
        assert_eq!(d.cells[&(0, 2)].text, Some(Some("1.50 s".into())));
        assert_eq!(d.cells[&(0, 5)].text, Some(Some("90%".into())));
    }

    #[test]
    fn sections_offered() {
        let mut a = [Avail::Unknown; 4];
        assert!(offered(&a).is_empty());
        a = [Avail::Yes, Avail::Yes, Avail::No, Avail::Yes];
        assert_eq!(offered(&a), vec![Section::Sessions, Section::Locks, Section::Statements]);
        assert!(offered(&[Avail::No; 4]).is_empty());
        assert_eq!(Every::S2.duration(), Some(Duration::from_secs(2)));
        assert_eq!(Every::Off.duration(), None);
    }
}
