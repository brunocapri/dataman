//! Running the tools in the app means the Keychain is read by the executable that wrote it: no prompt.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(feature = "checks")]
use gpui::AppContext;
use gpui::{AnyWindowHandle, App, Context, FontWeight, Global, SharedString, StyledText, WeakEntity, Window, actions, div, prelude::*};
use crate::theme::px;
use dataman_core::{Core, Dirs, SecretStore};
#[cfg(feature = "checks")]
use dataman_mcp::audit::{AllowedBy, Decision, Entry, Outcome};
use dataman_mcp::{Approval, Front, Hub, Registry};
use tokio::sync::{mpsc, oneshot};

use crate::app::Workspace;
#[cfg(feature = "checks")]
use crate::app::{Bound, Side};
use crate::theme::{ActiveTheme, Theme};
use crate::ui::{self, Btn};

actions!(mcp, [Allow, AlwaysAllow, Deny]);

const SHEET: &str = "McpSheet";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        crate::keymap::kb("cmd-enter", Allow, Some(SHEET)),
        crate::keymap::kb("cmd-alt-enter", AlwaysAllow, Some(SHEET)),
        crate::keymap::kb("escape", Deny, Some(SHEET)),
    ]);
}

pub struct Pending {
    pub id: u64,
    pub req: Approval,
    reply: Option<oneshot::Sender<bool>>,
    /// When the sheet first showed it, and whether a redraw is due when Allow becomes active.
    shown: std::cell::Cell<Option<std::time::Instant>>,
    waking: std::cell::Cell<bool>,
}

/// A sheet that just appeared (the one before it withdrawn, answered, or ⌘↩ held down) takes
/// no Allow yet: the click or key meant for what was there before cannot approve it.
pub const ALLOW_AFTER: std::time::Duration = std::time::Duration::from_millis(600);

impl Pending {
    fn new(id: u64, req: Approval, reply: Option<oneshot::Sender<bool>>) -> Pending {
        Pending { id, req, reply, shown: Default::default(), waking: Default::default() }
    }
}

pub(crate) fn allow_ready(cx: &App) -> bool {
    cx.try_global::<McpHost>().and_then(|h| h.pending.front()).and_then(|p| p.shown.get()).is_some_and(|t| t.elapsed() >= ALLOW_AFTER)
}

pub(crate) enum FrontMsg {
    Approve(Pending),
    Withdraw(u64),
    Tab(crate::agent::TabMsg),
    OpenTx { connection_id: String, reply: oneshot::Sender<Vec<String>> },
    ConnectionsChanged,
}

pub(crate) struct AppFront {
    tx: mpsc::UnboundedSender<FrontMsg>,
    next: AtomicU64,
}

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
        let sent = self.tx.send(FrontMsg::Approve(Pending::new(id, req, Some(reply)))).is_ok();
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

    fn show_doc(&self, w: dataman_mcp::TabWrite) -> futures_util::future::BoxFuture<'static, Option<u64>> {
        crate::agent::show_doc(self, w)
    }

    fn doc_done(&self, tab: u64, ran: Result<dataman_core::nosql::Ran, String>) {
        crate::agent::doc_done(self, tab, ran)
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

/// The hub is never dropped: its runtime must not end from one of its own tasks.
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

pub fn register(ws: WeakEntity<Workspace>, window: AnyWindowHandle, cx: &mut App) {
    cx.default_global::<McpHost>().workspace = Some((ws, window));
    cx.on_app_quit(|cx| {
        let ipc = cx.default_global::<McpHost>().ipc.take();
        drop(ipc);
        async {}
    })
    .detach();
}

pub(crate) fn workspace(cx: &App) -> Option<(WeakEntity<Workspace>, AnyWindowHandle)> {
    cx.try_global::<McpHost>()?.workspace.clone()
}

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

/// What the agent received in an audited call, while this app keeps it (in memory only; a
/// call the headless server answered is not here).
pub(crate) fn reply_of(id: &str, cx: &App) -> Option<Arc<str>> {
    let s = cx.try_global::<McpHost>()?.shared.clone()?;
    let g = s.hub.lock().unwrap();
    g.as_ref()?.0.reply(id)
}

#[cfg(feature = "checks")]
pub(crate) fn keep_reply(core: &Arc<Core>, id: &str, text: &str, cx: &mut App) {
    if let Ok((hub, _)) = shared(core, cx).hub() {
        hub.keep_reply(id, text);
    }
}

pub fn stdio_binary() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("dataman-mcp")))
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "dataman-mcp".into())
}

pub fn stdio_command() -> String {
    let argv = dataman_mcp::stable_server_argv(&stdio_binary());
    dataman_mcp::claude_stdio_command_with(&argv.iter().map(String::as_str).collect::<Vec<_>>())
}

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

pub fn answer(allow: bool, cx: &mut App) {
    if let Some(mut p) = cx.default_global::<McpHost>().pending.pop_front()
        && let Some(tx) = p.reply.take()
    {
        let _ = tx.send(allow);
    }
    cx.refresh_windows();
}

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

    pub(crate) fn mcp_sheet_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.mcp_return.take() {
            Some(f) => window.focus(&f, cx),
            None => self.focus_editor(window, cx),
        }
    }

    fn mcp_answer(&mut self, allow: bool, window: &mut Window, cx: &mut Context<Self>) {
        if allow && !allow_ready(cx) {
            return;
        }
        answer(allow, cx);
        let more = cx.try_global::<McpHost>().is_some_and(|h| !h.pending.is_empty());
        if !more {
            self.mcp_sheet_closed(window, cx);
        }
        cx.notify();
    }

    /// Saved without reconnecting: the write held in its transaction goes on.
    pub(crate) fn mcp_always_allow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !allow_ready(cx) {
            return;
        }
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

    pub(crate) fn render_mcp_sheet(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let host = cx.try_global::<McpHost>()?;
        let p = host.pending.front()?;
        let more = host.pending.len() - 1;
        let shown = *p.shown.get().get_or_insert_with(std::time::Instant::now);
        p.shown.set(Some(shown));
        let wait = ALLOW_AFTER.saturating_sub(shown.elapsed());
        let ready = wait.is_zero();
        if !ready && !p.waking.replace(true) {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            })
            .detach();
        }
        let t = cx.theme();
        let req = p.req.clone();
        let prod = req.env.is_production();
        // About this Mac (files to read), not a connection.
        let local = req.connection_id.is_empty();
        let color = self.store.read(cx).profile(&req.connection_id).map(|p| t.profile_color(p)).unwrap_or(t.tx4);
        let btn = |id: &'static str, kind: Btn, label: &'static str, key: &'static str| ui::button(&t, id, kind).h(px(28.)).child(label).child(ui::key_hint(key));
        let shown = dataman_mcp::shown_text(&req.sql);
        // A statement is highlighted; a list of files or changes is plain text.
        let lines = if req.connection_id.is_empty() {
            let run = |len| gpui::TextRun { len, font: crate::editor::mono(), color: t.tx, background_color: None, underline: None, strikethrough: None };
            shown.lines().map(|l| (SharedString::from(l.to_string()), vec![run(l.len())])).collect()
        } else {
            crate::editor::highlighted_lines(&t, &shown)
        };
        let tall = (lines.len() > 12).then(|| format!("{} lines", lines.len()));
        let sql = div()
            .id("mcp-sql")
            .max_h(px(240.))
            .overflow_y_scroll()
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
            }));
        let tags = div().flex().flex_row().flex_wrap().items_center().gap(px(6.)).children(req.reasons.iter().map(|r| {
            let c = reason_color(&t, &req, r);
            div().h(px(20.)).px(px(7.)).flex().items_center().bg(gpui::Hsla { a: 0.14, ..c }).text_size(px(11.)).text_color(c).child(r.clone())
        }));
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
                            (!local).then(|| ui::chip(color, 8.).into_any_element()),
                            req.connection.clone(),
                            Some(
                                div()
                                    .flex_none()
                                    .flex()
                                    .flex_row()
                                    .gap(px(10.))
                                    .text_size(px(11.5))
                                    .when(!local, |d| d.child(div().text_color(t.tx2).child(req.database.clone())))
                                    .when(!req.env.is_none(), |d| d.child(div().text_color(if prod { t.err } else { t.tx3 }).child(req.env.label())))
                                    .when(!local, |d| d.child(div().w(px(1.)).h(px(12.)).bg(t.line)))
                                    .child(div().text_color(t.tx3).child(dataman_mcp::shown_client(&req.client)))
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
                                .children(preview)
                                .when(req.counted, |d| {
                                    d.child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(t.tx3)
                                            .child("Counted a moment ago and rolled back, so nothing is locked while you decide. Allow runs it again, and never with more rows than these."),
                                    )
                                }),
                        )
                        .child(
                            ui::modal_footer(&t)
                                .text_size(px(11.))
                                .child(div().text_color(t.tx3).child(req.tool.clone()))
                                .children(tall.map(|n| div().text_color(t.tx3).child(n)))
                                .when(more > 0, |d| d.child(div().text_color(t.tx3).child(format!("+{more}"))))
                                .child(div().flex_1())
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
                                                .when(!ready, |b| b.opacity(0.5))
                                                .on_click(cx.listener(|w, _, window, cx| w.mcp_answer(true, window, cx))),
                                        )
                                }),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The MCP log is the Agents area's Audit page; a second time back to the explorer.
    pub(crate) fn toggle_mcp_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == dataman_core::layout::NavView::Agents && self.agents.read(cx).page == crate::agents::Page::Audit {
            self.show_nav(dataman_core::layout::NavView::Explorer, window, cx);
            self.focus_editor(window, cx);
            return;
        }
        self.show_agents(crate::agents::Page::Audit, window, cx);
    }

    #[cfg(feature = "checks")]
    pub(crate) fn mcp_log(&self) -> Option<gpui::Entity<crate::agents::AgentsPage>> {
        (self.view == dataman_core::layout::NavView::Agents).then(|| self.agents.clone())
    }

    /// A plain side panel for the layout checks (its docking, sliding and the result's zoom).
    #[cfg(feature = "checks")]
    pub(crate) fn demo_toggle_side(&mut self, cx: &mut Context<Self>) {
        if matches!(self.side, Some(Side::Feature("demo-side", ..))) {
            self.side = None;
        } else {
            let view = cx.new(|_| DemoSide);
            self.side_w = self.side_w.max(400.);
            self.side = Some(Side::Feature("demo-side", view.into(), Bound::Explorer));
        }
        cx.notify();
    }

    pub(crate) fn open_logged(&mut self, sql: &str, connection: Option<&str>, database: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let id = connection.and_then(|c| {
            let s = self.store.read(cx);
            s.profile(c).or_else(|| s.profiles.iter().find(|p| p.name == c)).map(|p| p.id.clone())
        });
        let conn = id.as_deref().and_then(|id| self.store.read(cx).connection(id));
        let engine = id.as_deref().and_then(|id| self.store.read(cx).profile(id)).map(|p| p.engine);
        let text = crate::app::statement_text(sql, engine);
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

#[cfg(feature = "checks")]
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

#[cfg(feature = "checks")]
pub(crate) struct DemoSide;

#[cfg(feature = "checks")]
impl Render for DemoSide {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div().size_full().flex().items_center().justify_center().text_color(t.tx4).child("side panel")
    }
}

#[cfg(feature = "checks")]
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
            id: None,
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
            rewritten: None,
            seen: None,
        });
    }
    if approval {
        let host = cx.default_global::<McpHost>();
        let id = core.config.connections().into_iter().find(|p| p.name == "shop").map(|p| p.id).unwrap_or_default();
        host.pending.push_back(Pending::new(
            0,
            Approval {
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
                counted: true,
            },
            None,
        ));
    }
}
