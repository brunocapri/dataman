//! The MCP server inside the app (`dataman_mcp`): one hub for the whole process, made on first
//! use, with the app as its [`Front`] (a human to ask, agent tabs to work in); the local socket
//! (`app.sock`) through which `dataman-mcp` hands its calls to the app while it runs
//! (started off the UI thread, after the first frame); the approval sheet; the log of tool
//! calls in the side panel. Running the tools here means the Keychain is read by the executable
//! that wrote it: no prompt. Idle, nothing runs: accepts on the socket and a channel receiver,
//! waiting.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Local;
use gpui::{
    AnyWindowHandle, App, AppContext, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Global, KeyBinding, SharedString,
    StyledText, WeakEntity, Window, actions, div, prelude::*, uniform_list,
};
use crate::theme::px;
use dataman_core::{Core, Dirs, SecretStore};
use dataman_mcp::audit::{AllowedBy, Decision, Entry, Outcome};
use dataman_mcp::{Approval, Front, Hub, Registry};
use tokio::sync::{mpsc, oneshot};

use crate::app::{Bound, Side, Workspace};
use crate::assets::Icon;
use crate::theme::{ActiveTheme, Theme};
use crate::ui::{self, Btn};

actions!(mcp, [Allow, AlwaysAllow, Deny, ReloadLog, CloseLog]);

const SHEET: &str = "McpSheet";
const LOG: &str = "McpLog";
/// A log row's height.
const ROW_H: f32 = 46.;
/// A log row's context menu.
const MENU: [&str; 2] = ["Copy Query", "Open in Tab"];

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-enter", Allow, Some(SHEET)),
        KeyBinding::new("cmd-alt-enter", AlwaysAllow, Some(SHEET)),
        KeyBinding::new("escape", Deny, Some(SHEET)),
        KeyBinding::new("cmd-r", ReloadLog, Some(LOG)),
        KeyBinding::new("escape", CloseLog, Some(LOG)),
    ]);
}

/// An agent's write waiting for the user.
pub struct Pending {
    pub id: u64,
    pub req: Approval,
    reply: Option<oneshot::Sender<bool>>,
}

/// What the tools (on the hub's runtime) send the UI.
pub(crate) enum FrontMsg {
    Approve(Pending),
    /// The question went away unanswered (the agent cancelled, the hub's timeout).
    Withdraw(u64),
    /// Agent tabs (`agent`).
    Tab(crate::agent::TabMsg),
    /// The titles of the tabs with a transaction open on a connection.
    OpenTx { connection_id: String, reply: oneshot::Sender<Vec<String>> },
    /// The saved connections changed: show them now.
    ConnectionsChanged,
}

/// The app's side of the tools: messages to the UI, answered there.
pub(crate) struct AppFront {
    tx: mpsc::UnboundedSender<FrontMsg>,
    next: AtomicU64,
}

/// Withdraws a question whose answer nobody waits for any more.
struct Asked {
    id: u64,
    tx: mpsc::UnboundedSender<FrontMsg>,
    done: bool,
}

impl Drop for Asked {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.tx.send(FrontMsg::Withdraw(self.id));
        }
    }
}

impl AppFront {
    pub(crate) fn send(&self, m: FrontMsg) -> bool {
        self.tx.send(m).is_ok()
    }
}

impl Front for AppFront {
    fn approve(&self, req: Approval) -> futures_util::future::BoxFuture<'static, bool> {
        let (reply, rx) = oneshot::channel();
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let sent = self.tx.send(FrontMsg::Approve(Pending { id, req, reply: Some(reply) })).is_ok();
        let asked = Asked { id, tx: self.tx.clone(), done: false };
        Box::pin(async move {
            // The whole guard lives in the future: dropped unanswered, it withdraws the question.
            let mut asked = asked;
            let r = sent && matches!(rx.await, Ok(true));
            asked.done = true;
            r
        })
    }

    fn tabs(&self) -> bool {
        crate::agent::tabs_ready()
    }

    fn run_read(&self, r: dataman_mcp::TabRead) -> futures_util::future::BoxFuture<'static, Result<dataman_core::bounded::Outcome, dataman_core::bounded::Stop>> {
        crate::agent::run_read(self, r)
    }

    fn show_write(&self, w: dataman_mcp::TabWrite) -> futures_util::future::BoxFuture<'static, Option<u64>> {
        crate::agent::show_write(self, w)
    }

    fn write_done(&self, tab: u64, sql: String, shown: dataman_mcp::WriteShown) {
        crate::agent::write_done(self, tab, sql, shown)
    }

    fn draft(&self, d: dataman_mcp::TabDraft) -> futures_util::future::BoxFuture<'static, Result<dataman_mcp::Drafted, String>> {
        crate::agent::draft(self, d)
    }

    fn open_transactions(&self, connection_id: &str) -> futures_util::future::BoxFuture<'static, Vec<String>> {
        let (reply, rx) = oneshot::channel();
        let sent = self.tx.send(FrontMsg::OpenTx { connection_id: connection_id.to_string(), reply }).is_ok();
        Box::pin(async move { if sent { rx.await.unwrap_or_default() } else { Vec::new() } })
    }

    fn connections_changed(&self) {
        let _ = self.tx.send(FrontMsg::ConnectionsChanged);
    }
}

/// What the socket serves with: one hub, made on first use (never dropped: its runtime must not
/// end from one of its own tasks), and the app's front.
pub(crate) struct Shared {
    dirs: Dirs,
    secrets: Arc<dyn SecretStore>,
    pub(crate) front: Arc<AppFront>,
    hub: Mutex<Option<(Arc<Hub>, Arc<Registry>)>>,
}

impl Shared {
    pub(crate) fn hub(&self) -> anyhow::Result<(Arc<Hub>, Arc<Registry>)> {
        let mut g = self.hub.lock().unwrap();
        if let Some(x) = &*g {
            return Ok(x.clone());
        }
        let hub = Hub::new(self.dirs.clone(), self.secrets.clone())?;
        std::mem::forget(hub.clone());
        hub.set_front(Some(self.front.clone()));
        let reg = dataman_mcp::registry(&hub.dirs);
        *g = Some((hub.clone(), reg.clone()));
        Ok((hub, reg))
    }
}

/// The app's MCP state (a global).
pub struct McpHost {
    shared: Option<Arc<Shared>>,
    ipc: Option<dataman_mcp::ipc::IpcServer>,
    ipc_starting: bool,
    pub pending: VecDeque<Pending>,
    workspace: Option<(WeakEntity<Workspace>, AnyWindowHandle)>,
}

impl Global for McpHost {}

impl Default for McpHost {
    fn default() -> Self {
        McpHost {
            shared: None,
            ipc: None,
            ipc_starting: false,
            pending: VecDeque::new(),
            workspace: None,
        }
    }
}

/// The window whose workspace shows the approval sheet and the agent tabs; the socket goes
/// when the app quits.
pub fn register(ws: WeakEntity<Workspace>, window: AnyWindowHandle, cx: &mut App) {
    cx.default_global::<McpHost>().workspace = Some((ws, window));
    cx.on_app_quit(|cx| {
        let ipc = cx.default_global::<McpHost>().ipc.take();
        drop(ipc);
        async {}
    })
    .detach();
}

/// The workspace and its window, for what the tools send.
pub(crate) fn workspace(cx: &App) -> Option<(WeakEntity<Workspace>, AnyWindowHandle)> {
    cx.try_global::<McpHost>()?.workspace.clone()
}

/// The shared hub's cell and the UI's receiver, made once.
pub(crate) fn shared(core: &Arc<Core>, cx: &mut App) -> Arc<Shared> {
    if let Some(s) = &cx.default_global::<McpHost>().shared {
        return s.clone();
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<FrontMsg>();
    cx.spawn(async move |cx| {
        while let Some(m) = rx.recv().await {
            cx.update(|cx| arrived(m, cx));
        }
    })
    .detach();
    let s = Arc::new(Shared {
        dirs: core.dirs.clone(),
        secrets: core.secrets.clone(),
        front: Arc::new(AppFront { tx, next: AtomicU64::new(0) }),
        hub: Mutex::new(None),
    });
    cx.default_global::<McpHost>().shared = Some(s.clone());
    s
}

/// The `dataman-mcp` next to this executable (a cargo build, an app bundle), or the name.
pub fn stdio_binary() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("dataman-mcp")))
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "dataman-mcp".into())
}

pub fn stdio_command() -> String {
    dataman_mcp::claude_stdio_command(&stdio_binary())
}

/// The socket for `dataman-mcp`, while agent access is on (Settings › MCP › Agent access).
pub fn apply(core: &Arc<Core>, cx: &mut App) {
    let want = core.settings.get().mcp_enabled;
    let shared = shared(core, cx);
    let host = cx.default_global::<McpHost>();
    if !want {
        host.ipc = None;
        return;
    }
    if host.ipc.is_some() || host.ipc_starting {
        return;
    }
    host.ipc_starting = true;
    let dirs = core.dirs.clone();
    let (tx, rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("mcp-ipc-bind".into())
        .spawn(move || {
            let provide: dataman_mcp::ipc::Provide = Arc::new(move || shared.hub());
            let _ = tx.send(dataman_mcp::ipc::serve(&dirs, provide));
        })
        .expect("thread");
    let core = core.clone();
    cx.spawn(async move |cx| {
        let r = rx.await;
        cx.update(|cx| {
            let host = cx.default_global::<McpHost>();
            host.ipc_starting = false;
            match r {
                Ok(Ok(srv)) if core.settings.get().mcp_enabled => host.ipc = Some(srv),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => log::warn!("mcp socket: {e}"),
                Err(_) => {}
            }
        });
    })
    .detach();
}

/// The settings file still has the keys of the MCP HTTP server the app once had: saving it
/// drops them, once, off the UI thread. Its Keychain token stays (deleting an item an older
/// build wrote makes macOS ask; nothing reads it).
pub fn forget_http_token(core: &Arc<Core>) {
    if !core.settings.legacy_http() {
        return;
    }
    let core = core.clone();
    let _ = std::thread::Builder::new().name("mcp-old-settings".into()).spawn(move || {
        if let Err(e) = core.settings.update(|_| {}) {
            log::warn!("saving the settings: {e:#}");
        }
    });
}

/// A message from the tools.
fn arrived(m: FrontMsg, cx: &mut App) {
    match m {
        FrontMsg::Approve(p) => {
            let tab = p.req.tab;
            let host = cx.default_global::<McpHost>();
            host.pending.push_back(p);
            let Some((ws, window)) = host.workspace.clone() else {
                answer(false, cx);
                return;
            };
            // The sheet over the agent tab that shows the write, when one does.
            let _ = window.update(cx, |_, window, cx| {
                let _ = ws.update(cx, |w, cx| {
                    if let Some(t) = tab.and_then(|id| w.tabs.iter().find(|t| t.entity_id().as_u64() == id).cloned()) {
                        w.activate_tab(&t, window, cx);
                    }
                    w.mcp_sheet_opened(window, cx)
                });
            });
        }
        FrontMsg::Withdraw(id) => {
            let host = cx.default_global::<McpHost>();
            let before = host.pending.len();
            host.pending.retain(|p| p.id != id);
            if host.pending.len() == before {
                return;
            }
            let empty = host.pending.is_empty();
            if let Some((ws, window)) = host.workspace.clone() {
                let _ = window.update(cx, |_, window, cx| {
                    let _ = ws.update(cx, |w, cx| {
                        if empty {
                            w.mcp_sheet_closed(window, cx);
                        }
                        cx.notify();
                    });
                });
            }
            cx.refresh_windows();
        }
        FrontMsg::Tab(t) => crate::agent::arrived(t, cx),
        FrontMsg::OpenTx { connection_id, reply } => {
            let titles = workspace(cx)
                .and_then(|(ws, _)| ws.upgrade())
                .map(|ws| {
                    ws.read(cx)
                        .tabs
                        .iter()
                        .filter(|t| t.read(cx).tx_open() && t.read(cx).connection().is_some_and(|c| c.profile.id == connection_id))
                        .map(|t| t.read(cx).title(cx).to_string())
                        .collect()
                })
                .unwrap_or_default();
            let _ = reply.send(titles);
        }
        FrontMsg::ConnectionsChanged => {
            if let Some((ws, window)) = workspace(cx) {
                let _ = window.update(cx, |_, window, cx| {
                    let _ = ws.update(cx, |w, cx| w.connections_on_disk(window, cx));
                });
            }
        }
    }
}

/// Answers the oldest waiting write.
pub fn answer(allow: bool, cx: &mut App) {
    if let Some(mut p) = cx.default_global::<McpHost>().pending.pop_front()
        && let Some(tx) = p.reply.take()
    {
        let _ = tx.send(allow);
    }
    cx.refresh_windows();
}

/// Why a question was asked, as a tag's color: destructive and production in red, a row count
/// in amber.
fn reason_color(t: &Theme, req: &Approval, r: &str) -> gpui::Hsla {
    if r.contains(" rows (limit ") {
        t.warn
    } else if r == "production" || req.destructive {
        t.err
    } else {
        t.warn
    }
}

impl Workspace {
    pub(crate) fn mcp_sheet_opened(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.mcp_focus.is_focused(window) {
            if self.mcp_return.is_none() {
                self.mcp_return = window.focused(cx);
            }
            window.focus(&self.mcp_focus, cx);
        }
        cx.notify();
    }

    /// The last question went (answered or withdrawn): the keys go back.
    pub(crate) fn mcp_sheet_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.mcp_return.take() {
            Some(f) => window.focus(&f, cx),
            None => self.focus_editor(window, cx),
        }
    }

    fn mcp_answer(&mut self, allow: bool, window: &mut Window, cx: &mut Context<Self>) {
        answer(allow, cx);
        let more = cx.try_global::<McpHost>().is_some_and(|h| !h.pending.is_empty());
        if !more {
            self.mcp_sheet_closed(window, cx);
        }
        cx.notify();
    }

    /// Allows the oldest waiting write and adds its statements to the connection's allowed
    /// queries (MCP › Advanced in its sheet): the same statements run without asking from now
    /// on. Saved without reconnecting (the write held in its transaction goes on).
    pub(crate) fn mcp_always_allow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, stmts)) = cx.try_global::<McpHost>().and_then(|h| h.pending.front()).map(|p| (p.req.connection_id.clone(), p.req.allowable.clone())) else { return };
        if !stmts.is_empty() {
            let core = self.store.read(cx).core.clone();
            core.config.refresh();
            if let Some(mut p) = core.config.get(&id)
                && !p.mcp_allow(&stmts).is_empty()
                && let Err(e) = core.config.upsert(p)
            {
                log::warn!("saving the allowed queries: {e:#}");
            }
            self.connections_on_disk(window, cx);
        }
        self.mcp_answer(true, window, cx);
    }

    /// The approval sheet for the oldest waiting write: where (the connection's color, its
    /// database and environment), who asks, the exact SQL, why it asks, what it changed in the
    /// transaction held for the answer, Deny / Allow, and Always Allow (its statements into the
    /// connection's allowed queries).
    pub(crate) fn render_mcp_sheet(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let host = cx.try_global::<McpHost>()?;
        let p = host.pending.front()?;
        let more = host.pending.len() - 1;
        let t = cx.theme();
        let req = p.req.clone();
        let prod = req.env.is_production();
        let color = self.store.read(cx).profile(&req.connection_id).map(|p| t.profile_color(p)).unwrap_or(t.tx4);
        let btn = |id: &'static str, kind: Btn, label: &'static str, key: &'static str| ui::button(&t, id, kind).h(px(28.)).child(label).child(ui::key_hint(key));
        let lines = crate::editor::highlighted_lines(&t, req.sql.trim());
        let sql = div()
            .id("mcp-sql")
            .max_h(px(240.))
            .overflow_scroll()
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
                    div().whitespace_nowrap().child(StyledText::new(text).with_runs(runs)).into_any_element()
                }
            }));
        let tags = div().flex().flex_row().flex_wrap().items_center().gap(px(6.)).children(req.reasons.iter().map(|r| {
            let c = reason_color(&t, &req, r);
            div().h(px(20.)).px(px(7.)).flex().items_center().bg(gpui::Hsla { a: 0.14, ..c }).text_size(px(11.)).text_color(c).child(r.clone())
        }));
        // The count, unless a reason says it already (over the limit).
        let counted = req.reasons.iter().any(|r| r.contains(" rows (limit "));
        let rows = req.affected.filter(|_| !counted).map(|n| if n == 1 { "1 row".to_string() } else { format!("{} rows", crate::grid::ui_count(n as usize)) });
        let preview = req.preview.clone().map(|(cols, data)| {
            let n = cols.len().min(6);
            let cell = |s: String, c: gpui::Hsla| div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(c).child(s);
            div()
                .border_1()
                .border_color(t.line)
                .px(px(10.))
                .py(px(6.))
                .flex()
                .flex_col()
                .gap(px(3.))
                .text_size(px(11.))
                .child(div().flex().flex_row().gap(px(10.)).children(cols.into_iter().take(n).map(|c| cell(c, t.tx3))))
                .children(data.into_iter().map(|r| div().flex().flex_row().gap(px(10.)).children(r.into_iter().take(n).map(|v| cell(v, t.tx2)))))
        });
        Some(
            div()
                .id("mcp-sheet")
                .absolute()
                .inset_0()
                .bg(t.scrim)
                .occlude()
                .key_context(format!("{SHEET} Modal").as_str())
                .track_focus(&self.mcp_focus)
                .on_action(cx.listener(|w, _: &Allow, window, cx| w.mcp_answer(true, window, cx)))
                // Always Allow fills the allowed queries: off with them (`MCP_ADVANCED`).
                .when(dataman_core::MCP_ADVANCED && !req.allowable.is_empty(), |d| d.on_action(cx.listener(|w, _: &AlwaysAllow, window, cx| w.mcp_always_allow(window, cx))))
                .on_action(cx.listener(|w, _: &Deny, window, cx| w.mcp_answer(false, window, cx)))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    ui::modal(&t, "mcp-card")
                        .w(px(600.))
                        .child(ui::modal_header_with(
                            &t,
                            Some(ui::chip(color, 8.).into_any_element()),
                            req.connection.clone(),
                            Some(
                                div()
                                    .flex_none()
                                    .flex()
                                    .flex_row()
                                    .gap(px(10.))
                                    .text_size(px(11.5))
                                    .child(div().text_color(t.tx2).child(req.database.clone()))
                                    .when(!req.env.is_none(), |d| d.child(div().text_color(if prod { t.err } else { t.tx3 }).child(req.env.label())))
                                    .child(div().w(px(1.)).h(px(12.)).bg(t.line))
                                    .child(div().text_color(t.tx3).child(req.client.clone()))
                                    .into_any_element(),
                            ),
                            ui::modal_close(&t, "mcp-close").on_click(cx.listener(|w, _, window, cx| w.mcp_answer(false, window, cx))),
                        ))
                        .child(
                            div()
                                .px(px(20.))
                                .py(px(16.))
                                .flex()
                                .flex_col()
                                .gap(px(12.))
                                .child(sql)
                                .child(div().flex().flex_row().items_center().gap(px(10.)).child(div().flex_1().child(tags)).children(rows.map(|r| div().flex_none().text_size(px(11.5)).text_color(t.tx2).child(r))))
                                .children(preview),
                        )
                        .child(
                            ui::modal_footer(&t)
                                .text_size(px(11.))
                                .child(div().text_color(t.tx3).child(req.tool.clone()))
                                .when(more > 0, |d| d.child(div().text_color(t.tx3).child(format!("+{more}"))))
                                .child(div().flex_1())
                                // Always Allow: a quieter button, apart from Deny / Allow (off with the
                                // allowed queries, `MCP_ADVANCED`).
                                .when(dataman_core::MCP_ADVANCED && !req.allowable.is_empty(), |d| {
                                    let what = if req.allowable.len() == 1 { "this statement" } else { "these statements" };
                                    let tip = format!("Allow, and from now on let agents run {what} on {} without asking (connection › MCP › Advanced › Allowed queries)", req.connection);
                                    d.child(
                                        btn("mcp-always", Btn::Ghost, "Always Allow", "⌥⌘↩")
                                            .tooltip(ui::text_tooltip(tip))
                                            .on_click(cx.listener(|w, _, window, cx| w.mcp_always_allow(window, cx))),
                                    )
                                })
                                .child({
                                    let bounds = self.mcp_buttons.clone();
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap(px(8.))
                                        .on_children_prepainted(move |b, _, _| *bounds.borrow_mut() = b)
                                        .child(btn("mcp-deny", Btn::Secondary, "Deny", "esc").on_click(cx.listener(|w, _, window, cx| w.mcp_answer(false, window, cx))))
                                        .child(
                                            btn("mcp-allow", if prod || req.destructive { Btn::Danger } else { Btn::Primary }, "Allow", "⌘↩")
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .on_click(cx.listener(|w, _, window, cx| w.mcp_answer(true, window, cx))),
                                        )
                                }),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The MCP log in the side panel (toggles).
    pub(crate) fn toggle_mcp_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.side, Some(Side::Feature("mcp-log", ..))) {
            self.side = None;
            cx.notify();
            return;
        }
        let path = self.store.read(cx).core.dirs.mcp_audit_file();
        let view = cx.new(|cx| McpLog::new(path, cx));
        cx.subscribe_in(&view, window, |w, _, ev: &LogEvent, window, cx| match ev {
            LogEvent::Close => {
                if matches!(w.side, Some(Side::Feature("mcp-log", ..))) {
                    w.side = None;
                }
                w.focus_editor(window, cx);
                cx.notify();
            }
            LogEvent::Open { sql, connection, database } => w.open_logged(sql, connection.as_deref(), database.as_deref(), window, cx),
        })
        .detach();
        window.focus(&view.focus_handle(cx), cx);
        self.side_w = self.side_w.max(400.);
        self.side = Some(Side::Feature("mcp-log", view.into(), Bound::Explorer));
        cx.notify();
    }

    /// The MCP log, while it is in the side panel.
    pub(crate) fn mcp_log(&self) -> Option<gpui::Entity<McpLog>> {
        match &self.side {
            Some(Side::Feature("mcp-log", v, _)) => v.clone().downcast::<McpLog>().ok(),
            _ => None,
        }
    }

    /// A logged statement in a new tab, not run: on the call's connection (its name in the
    /// log; an id works too) and database, connecting it when it is not yet; a connection
    /// since deleted leaves the tab on the default one.
    fn open_logged(&mut self, sql: &str, connection: Option<&str>, database: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let id = connection.and_then(|c| {
            let s = self.store.read(cx);
            s.profile(c).or_else(|| s.profiles.iter().find(|p| p.name == c)).map(|p| p.id.clone())
        });
        let conn = id.as_deref().and_then(|id| self.store.read(cx).connection(id));
        let text = format!("{};", sql.trim_end().trim_end_matches(';'));
        let database = database.map(str::to_string);
        let tab = self.new_tab(window, cx);
        tab.update(cx, |t, cx| {
            match (&id, conn.clone()) {
                (Some(_), Some(c)) => {
                    t.set_connection(Some(c), cx);
                    if let Some(db) = database {
                        t.set_database(db, cx);
                    }
                }
                // It takes the connection once it is up (`on_conn`), as a restored tab does.
                (Some(id), None) => {
                    t.set_connection(None, cx);
                    t.want = Some((id.clone(), database));
                }
                (None, _) => {}
            }
            t.editor.update(cx, |e, cx| e.set_text(&text, cx));
        });
        if let (Some(id), None) = (id, conn) {
            self.store.update(cx, |s, cx| s.connect(&id, None, cx));
        }
        cx.notify();
    }
}

// ---------------------------------------------------------------------------------------------
// The log view

/// What a log row says came of the call: the rows, `draft`, `ok`, the decision or the outcome.
pub(crate) fn row_what(e: &Entry) -> String {
    match (e.outcome, e.rows, e.approval.as_ref().map(|a| a.decision)) {
        (Outcome::Ok, _, _) if e.draft => "draft".to_string(),
        (Outcome::Ok, Some(1), _) => "1 row".to_string(),
        (Outcome::Ok, Some(n), _) => format!("{} rows", crate::grid::ui_count(n as usize)),
        (Outcome::Ok, None, _) => "ok".to_string(),
        (_, _, Some(d)) => d.label().to_string(),
        (o, _, _) => o.label().to_string(),
    }
}

pub enum LogEvent {
    Close,
    /// A call's statement in a new tab (not run), on its connection (a name, or an id) and
    /// database.
    Open { sql: String, connection: Option<String>, database: Option<String> },
}

pub struct McpLog {
    path: PathBuf,
    focus: FocusHandle,
    entries: Vec<Entry>,
    selected: Option<usize>,
    loaded: bool,
    /// A row's context menu (right-click, ⌃-click): the entry and where it opened.
    menu: Option<(usize, gpui::Point<gpui::Pixels>)>,
    /// The list's bounds as last painted (`mcp-check` finds its rows by them).
    list_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
}

impl EventEmitter<LogEvent> for McpLog {}

impl Focusable for McpLog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl McpLog {
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut v = McpLog { path, focus: cx.focus_handle(), entries: Vec::new(), selected: None, loaded: false, menu: None, list_bounds: Default::default() };
        v.reload(cx);
        v
    }

    /// Reads the file's tail off the UI thread, newest first.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let task = cx.background_spawn(async move { dataman_mcp::audit::tail(&path, 500) });
        cx.spawn(async move |this, cx| {
            let mut entries = task.await;
            entries.reverse();
            let _ = this.update(cx, |v, cx| {
                v.entries = entries;
                v.loaded = true;
                v.selected = v.selected.filter(|i| *i < v.entries.len());
                v.menu = None;
                cx.notify();
            });
        })
        .detach();
    }

    fn render_row(&self, i: usize, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let e = &self.entries[i];
        let sel = self.selected == Some(i);
        let decision = e.approval.as_ref().map(|a| a.decision);
        let color = match (e.outcome, decision) {
            (Outcome::Ok, _) => t.ok,
            (_, Some(Decision::Denied | Decision::Timeout)) => t.err,
            (Outcome::Refused | Outcome::Invalid, _) => t.warn,
            _ => t.err,
        };
        let time = e.ts.with_timezone(&Local).format("%H:%M:%S").to_string();
        let what = row_what(e);
        let line: SharedString = e.sql.clone().or_else(|| e.error.clone()).unwrap_or_default().into();
        let hov = t.hov;
        div()
            .id(("mcp-log", i))
            .h(px(ROW_H))
            .mx(px(6.))
            .px(px(8.))
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(3.))
            .when(sel, |d| d.bg(t.prs))
            .when(!sel, |d| d.hover(move |s| s.bg(hov)))
            .on_click(cx.listener(move |v, ev: &gpui::ClickEvent, window, cx| {
                if ev.modifiers().control {
                    return;
                }
                window.focus(&v.focus, cx);
                if ev.click_count() >= 2 {
                    v.selected = Some(i);
                    v.open(i, cx);
                } else {
                    v.selected = if v.selected == Some(i) { None } else { Some(i) };
                }
                cx.notify();
            }))
            // Right-click, and ⌃-click (the Mac convention): Copy Query, Open in Tab.
            .on_mouse_down(gpui::MouseButton::Right, cx.listener(move |v, ev: &gpui::MouseDownEvent, _, cx| {
                cx.stop_propagation();
                v.menu = Some((i, ev.position));
                cx.notify();
            }))
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |v, ev: &gpui::MouseDownEvent, _, cx| {
                if ev.modifiers.control {
                    cx.stop_propagation();
                    v.menu = Some((i, ev.position));
                    cx.notify();
                }
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(11.))
                    .child(ui::chip(color, 6.))
                    .child(div().text_color(t.tx3).child(time))
                    .child(div().text_color(t.tx).child(e.tool.clone()))
                    .children(e.connection.clone().map(|c| div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx2).child(c)))
                    .child(div().flex_1())
                    .child(div().text_color(t.tx3).child(dataman_core::activity::fmt_ms(e.ms)))
                    // Let through without a human: by an allowed query (amber) or YOLO (red).
                    .children(e.allowed_by.as_ref().map(|b| {
                        let c = if matches!(b, AllowedBy::Yolo) { t.err } else { t.warn };
                        div().h(px(15.)).px(px(4.)).flex().items_center().border_1().border_color(c).text_color(c).text_size(px(9.5)).child(b.label())
                    }))
                    .child(div().text_color(if e.outcome == Outcome::Ok { t.tx3 } else { color }).child(what)),
            )
            .child(div().pl(px(14.)).text_size(px(11.)).text_color(t.tx3).whitespace_nowrap().overflow_hidden().text_ellipsis().child(line))
            .into_any_element()
    }

    /// Entry `i`'s statement in a new tab (the workspace opens it).
    fn open(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(e) = self.entries.get(i) else { return };
        let Some(sql) = e.statement() else { return };
        cx.emit(LogEvent::Open { sql: sql.to_string(), connection: e.connection.clone(), database: e.database.clone() });
    }

    fn copy(&mut self, i: usize, cx: &mut Context<Self>) {
        if let Some(sql) = self.entries.get(i).and_then(|e| e.statement()) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(sql.to_string()));
        }
    }

    /// A row's context menu: Copy Query, Open in Tab (dimmed for calls without SQL).
    fn render_menu(&self, t: &Theme, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let (i, at) = self.menu?;
        let has_sql = self.entries.get(i).and_then(|e| e.statement()).is_some();
        let item = |key: &'static str, label: &'static str, f: fn(&mut Self, usize, &mut Context<Self>)| {
            ui::menu_item(t, key, label, None, has_sql).when(has_sql, |d| {
                d.on_click(cx.listener(move |v, _, _, cx| {
                    v.menu = None;
                    f(v, i, cx);
                    cx.notify();
                }))
            })
        };
        let menu = ui::popover(t)
            .w(px(160.))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|v, _, _, cx| {
                v.menu = None;
                cx.notify();
            }))
            .child(item("ml-copy", MENU[0], Self::copy))
            .child(item("ml-open", MENU[1], Self::open));
        Some(gpui::deferred(gpui::anchored().position(at).snap_to_window_with_margin(px(8.)).child(menu)).priority(3))
    }

    // `mcp-check` (`--demo`): where the rows are, what a click cannot report back.

    /// A point inside row `i` of the list, in window coordinates (`None` before it is painted).
    pub(crate) fn demo_row_point(&self, i: usize) -> Option<gpui::Point<gpui::Pixels>> {
        let b = self.list_bounds.get()?;
        Some(gpui::point(b.origin.x + px(40.), b.origin.y + px(ROW_H * i as f32 + ROW_H / 2.)))
    }

    pub(crate) fn demo_menu_open(&self) -> bool {
        self.menu.is_some()
    }

    /// The open menu's items and whether each is enabled.
    pub(crate) fn demo_menu_items(&self) -> Vec<(&'static str, bool)> {
        let Some((i, _)) = self.menu else { return Vec::new() };
        let on = self.entries.get(i).and_then(|e| e.statement()).is_some();
        MENU.iter().map(|l| (*l, on)).collect()
    }

    pub(crate) fn demo_entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Opens row `i`'s menu as a right-click there would (screenshots).
    pub(crate) fn demo_open_menu(&mut self, i: usize, cx: &mut Context<Self>) {
        self.menu = self.demo_row_point(i).map(|p| (i, p));
        cx.notify();
    }

    fn render_detail(&self, t: &Theme) -> Option<gpui::AnyElement> {
        let e = self.entries.get(self.selected?)?;
        let facts: Vec<String> = [Some(e.client.clone()), Some(e.transport.clone()), e.database.clone()].into_iter().flatten().filter(|s| !s.is_empty()).collect();
        let approval = e.approval.as_ref().map(|a| {
            let mut parts = vec![a.decision.label().to_string()];
            parts.extend(a.by.clone());
            parts.extend(a.reasons.iter().cloned());
            parts.join(" · ")
        });
        let allowed = e.allowed_by.as_ref().map(|b| match b {
            AllowedBy::Yolo => "ran without asking: YOLO".to_string(),
            AllowedBy::Allowlist { entries } => format!("ran without asking: allowed {}", entries.join(" · ")),
        });
        Some(
            div()
                .flex_none()
                .px(px(12.))
                .py(px(10.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .border_t_1()
                .border_color(t.line)
                .child(div().text_size(px(11.)).text_color(t.tx3).child(facts.join(" · ")))
                .children(approval.map(|a| div().text_size(px(11.)).text_color(t.tx2).child(a)))
                .children(allowed.map(|a| div().text_size(px(11.)).text_color(if matches!(e.allowed_by, Some(AllowedBy::Yolo)) { t.err } else { t.warn }).child(a)))
                .children(e.sql.as_ref().map(|s| crate::chrome::code_block(t, s, None)))
                .children(e.error.as_ref().map(|err| div().text_size(px(11.5)).line_height(px(17.)).text_color(t.err).child(err.clone())))
                .into_any_element(),
        )
    }
}

impl gpui::Render for McpLog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let body: gpui::AnyElement = if self.entries.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .when(self.loaded, |d| d.child(ui::empty_state(&t, "[ NO CALLS ]", 20., None)))
                .into_any_element()
        } else {
            let theme = t.clone();
            uniform_list("mcp-log-list", self.entries.len(), cx.processor(move |v, range: std::ops::Range<usize>, _window, cx| {
                range.map(|i| v.render_row(i, &theme, cx)).collect::<Vec<_>>()
            }))
            .flex_1()
            .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .text_size(px(12.))
            .key_context(LOG)
            .track_focus(&self.focus)
            .on_action(cx.listener(|v, _: &ReloadLog, _, cx| v.reload(cx)))
            .on_action(cx.listener(|v, _: &CloseLog, _, cx| {
                // esc closes the menu first.
                if v.menu.take().is_some() {
                    cx.notify();
                } else {
                    cx.emit(LogEvent::Close);
                }
            }))
            .child(
                div()
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
                    .child(ui::caps(&t, "MCP LOG"))
                    .child(div().text_color(t.tx3).child(self.entries.len().to_string()))
                    .child(div().flex_1())
                    .child(ui::icon_button(&t, "mcp-log-reload", Icon::Reload, 24., 13.).on_click(cx.listener(|v, _, _, cx| v.reload(cx))))
                    .child(ui::icon_button(&t, "mcp-log-close", Icon::Close, 24., 13.).on_click(cx.listener(|_, _, _, cx| cx.emit(LogEvent::Close)))),
            )
            .child({
                let bounds = self.list_bounds.clone();
                div().flex_1().min_h_0().pt(px(4.)).flex().flex_col().on_children_prepainted(move |b, _, _| bounds.set(b.first().copied())).child(body)
            })
            .children(self.render_detail(&t))
            .children(self.render_menu(&t, cx))
    }
}

/// Seeds the log and the host for the screenshots (`--demo`): a few calls of each outcome and,
/// with `approval`, a waiting write.
pub fn demo_seed(core: &Arc<Core>, approval: bool, cx: &mut App) {
    for (name, env, mcp) in [
        ("shop", dataman_core::Environment::Staging, Some(dataman_core::McpAccess::Full)),
        ("shop-live", dataman_core::Environment::Production, None),
    ] {
        if !core.config.connections().iter().any(|p| p.name == name) {
            let mut p = dataman_core::ConnectionProfile::new(name);
            (p.host, p.database, p.env, p.mcp) = (format!("{}.db.internal", env.label()), "shop".into(), env, mcp);
            if name == "shop" {
                p.mcp_allow(&["UPDATE orders SET status = ? WHERE id = ?", "TRUNCATE cart_items_tmp"]);
            }
            let _ = core.config.upsert(p);
        }
    }
    let audit = dataman_mcp::audit::Audit::new(core.dirs.mcp_audit_file());
    let now = chrono::Utc::now();
    let note = |decision, reasons: &[&str], rows| Some(dataman_mcp::audit::ApprovalNote { decision, by: Some("bruno".into()), reasons: reasons.iter().map(|r| r.to_string()).collect(), rows });
    let rows: [(&str, Option<&str>, Option<&str>, Option<u64>, f64, Outcome, Option<&str>, Option<dataman_mcp::audit::ApprovalNote>); 10] = [
        ("list_connections", None, None, None, 0.4, Outcome::Ok, None, None),
        ("list_tables", Some("local pg"), None, Some(12), 8.1, Outcome::Ok, None, None),
        ("describe_table", Some("local pg"), None, None, 14.6, Outcome::Ok, None, None),
        ("open_query", Some("local pg"), Some("SELECT status, count(*) FROM orders WHERE created_at > ? GROUP BY status"), Some(4), 23.9, Outcome::Ok, None, None),
        ("run_query", Some("lar"), Some("DELETE FROM orders WHERE id = ?"), None, 0.2, Outcome::Refused, Some("refused: DELETE is not a read"), None),
        ("explain_analyze", Some("local pg"), Some("SELECT * FROM orders WHERE customer = $1"), None, 41.3, Outcome::Ok, None, None),
        ("run_query", Some("local pg"), Some("SELECT pg_sleep(?)"), None, 30001.0, Outcome::Timeout, Some("timed out after 30.0 s"), None),
        ("execute", Some("shop"), Some("UPDATE orders SET status = ? WHERE id = ?"), Some(1), 12.2, Outcome::Ok, None, None),
        ("execute", Some("shop"), Some("DELETE FROM orders"), None, 48211.0, Outcome::Refused, Some("refused: denied in Dataman (DELETE without WHERE); rolled back"), note(Decision::Denied, &["DELETE without WHERE"], Some(1204))),
        ("execute", Some("shop-live"), Some("TRUNCATE orders"), None, 0.1, Outcome::Refused, Some("refused: MCP access to this connection is off"), None),
    ];
    // As the agent sent them (the log shows them redacted).
    let sent: [Option<&str>; 10] = [
        None,
        None,
        None,
        Some("SELECT status, count(*) FROM orders WHERE created_at > 1727740800 GROUP BY status"),
        Some("DELETE FROM orders WHERE id = 7"),
        None,
        Some("SELECT pg_sleep(60)"),
        Some("UPDATE orders SET status = 'paid' WHERE id = 42"),
        None,
        None,
    ];
    for (i, (tool, conn, sql, n, ms, outcome, err, approval)) in rows.into_iter().enumerate() {
        audit.write(&Entry {
            ts: now - chrono::Duration::seconds(((10 - i) * 37) as i64),
            client: "claude-code".into(),
            transport: "stdio".into(),
            tool: tool.into(),
            connection: conn.map(str::to_string),
            database: conn.map(|_| "dataman".to_string()),
            sql: sql.map(str::to_string),
            query: sent[i].map(str::to_string),
            rows: n,
            ms,
            outcome,
            error: err.map(str::to_string),
            approval,
            draft: false,
            favorite: None,
            allowed_by: (i == 7).then(|| AllowedBy::Allowlist { entries: vec!["UPDATE orders SET status = ? WHERE id = ?".into()] }),
        });
    }
    if approval {
        let host = cx.default_global::<McpHost>();
        let id = core.config.connections().into_iter().find(|p| p.name == "shop").map(|p| p.id).unwrap_or_default();
        host.pending.push_back(Pending {
            id: 0,
            req: Approval {
                client: "claude-code".into(),
                tool: "execute".into(),
                connection: "shop".into(),
                connection_id: id,
                database: "shop".into(),
                env: dataman_core::Environment::Staging,
                sql: "-- clear the abandoned carts\nDELETE FROM orders\n RETURNING id, status, total".into(),
                reasons: vec!["DELETE without WHERE".into(), "1204 rows (limit 100)".into()],
                destructive: true,
                affected: Some(1204),
                preview: Some((
                    vec!["id".into(), "status".into(), "total".into()],
                    vec![
                        vec!["1".into(), "pending".into(), "129.90".into()],
                        vec!["2".into(), "pending".into(), "54.00".into()],
                        vec!["3".into(), "paid".into(), "310.45".into()],
                        vec!["4".into(), "pending".into(), "12.99".into()],
                        vec!["5".into(), "cancelled".into(), "88.10".into()],
                    ],
                )),
                tab: None,
                allowable: vec!["DELETE FROM orders\n RETURNING id, status, total".into()],
            },
            reply: None,
        });
    }
}
