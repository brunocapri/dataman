use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyView, App, Context, Entity, EntityId, FocusHandle, Focusable, KeyBinding, MouseButton, MouseMoveEvent, Window,
    actions, div, prelude::*,
};
use crate::theme::{Pt, px};
use dataman_core::groups::Pane;
use dataman_core::layout::NavView;
use dataman_core::{Connection, ConnectionProfile, Core, Favorite, Translucency};

use crate::conn_dialog::{ConnDialog, DialogEvent};
use crate::conns::{ConnEvent, ConnState, ConnStore};
use crate::grid::GridEvent;
use crate::history::{HistoryEvent, HistoryPanel};
use crate::panels::PanelRegistry;
use crate::tab::{QueryTab, TabEvent};
use crate::theme::{self, ActiveTheme, metrics, mono};
use crate::tree::{SchemaTree, TreeEvent};
use crate::welcome::{Welcome, WelcomeEvent};

actions!(
    workspace,
    [
        NewTab,
        CloseTab,
        NextTab,
        PrevTab,
        Tab1,
        Tab2,
        Tab3,
        Tab4,
        Tab5,
        Tab6,
        Tab7,
        Tab8,
        Tab9,
        NewConnection,
        NewConnectionFromClipboard,
        ImportConnections,
        EditConnection,
        FavoriteQuery,
        SaveFavorite,
        SaveAs,
        ImportFavorites,
        Cancel,
        Refresh,
        ToggleExplorer,
        ToggleSidePanel,
        FocusExplorer,
        FocusGrid,
        FocusEditor,
        CycleFocus,
        CycleFocusBack,
        JumpToError,
        UseExplorerConnection,
        ShowShortcuts,
        CommandPalette,
        GoTo,
        OpenSettings,
        ExportResult,
        ImportData,
        ReviewChanges,
        DiscardChanges,
        OpenSqlFile,
        ToggleTheme,
        ThemeDark,
        ThemeLight,
        ThemeSystem,
        TranslucencyOn,
        TranslucencyOff,
        FontGeistMono,
        FontLilex,
        FontZedMono,
        FontJetBrainsMono,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        LarPanel1,
        Quit,
        CancelPendingRun,
        DiscardAndRun,
        NextResult,
        PrevResult,
        NextPage,
        PrevPage,
        CommitTx,
        RollbackTx,
        SheetConfirm,
        SheetRollback,
        SheetCancel,
        Explain,
        ExplainAnalyze,
        CopyPlanText,
        TogglePlanFlame,
        OpenActivity,
        ToggleMcpLog,
        CheckForUpdates,
        ShowUpdate,
        InstallUpdate,
        GoBack,
        GoForward,
        ToggleResultZoom,
        RunZoomed,
    ]
);

pub static RENDERS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub const SHEET_CONTEXT: &str = "TxSheet";

/// Off while a modal has the keys, so ⌘W, ⌘T… cannot act behind it.
pub const WORKSPACE_KEYS: &str = "Workspace && !Modal";

pub fn bind_keys(cx: &mut App) {
    let w = Some(WORKSPACE_KEYS);
    cx.bind_keys([
        KeyBinding::new("escape", FocusEditor, w),
        KeyBinding::new("escape", CancelPendingRun, Some("ConfirmDiscard")),
        KeyBinding::new("enter", CancelPendingRun, Some("ConfirmDiscard")),
        KeyBinding::new("cmd-backspace", DiscardAndRun, Some("ConfirmDiscard")),
        KeyBinding::new("cmd-enter", SheetConfirm, Some(SHEET_CONTEXT)),
        KeyBinding::new("cmd-backspace", SheetRollback, Some(SHEET_CONTEXT)),
        KeyBinding::new("escape", SheetCancel, Some(SHEET_CONTEXT)),
    ]);
    let grid = Some(crate::grid::GRID_KEYS);
    let run: Vec<KeyBinding> = crate::keymap::effective("run_all").iter().filter_map(|k| KeyBinding::load(k, Box::new(RunZoomed), grid.map(|c| gpui::KeyBindingContextPredicate::parse(c).unwrap().into()), false, None, &gpui::DummyKeyboardMapper).ok()).collect();
    cx.bind_keys(run);
}

#[derive(Clone)]
pub(crate) enum AfterTx {
    CloseTab(Entity<QueryTab>),
    CloseTabs(Vec<Entity<QueryTab>>),
    Disconnect(String),
    Reconnect(String),
    Retarget { tab: Entity<QueryTab>, conn: Arc<Connection>, db: Option<String>, run: Option<String> },
    SaveConnection(Entity<ConnDialog>),
    DeleteConnection(String),
    Quit,
}

impl AfterTx {
    pub(crate) fn verb(&self) -> &'static str {
        match self {
            AfterTx::CloseTab(_) | AfterTx::CloseTabs(_) => "close",
            AfterTx::Disconnect(_) => "disconnect",
            AfterTx::Reconnect(_) => "reconnect",
            AfterTx::Retarget { .. } => "switch",
            AfterTx::SaveConnection(_) => "save",
            AfterTx::DeleteConnection(_) => "delete",
            AfterTx::Quit => "quit",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Change<'a, K> {
    CloseTab(K),
    Retarget { tab: K, same_conn: bool, from_db: &'a str, to_db: &'a str },
    Disconnect(&'a str),
    Edit { id: &'a str, is_new: bool },
    Delete(&'a str),
    Quit,
}

pub(crate) fn ends_of<K: Copy>(change: Change<'_, K>) -> Option<Ends<'_, K>> {
    match change {
        Change::CloseTab(k) => Some(Ends::Tab(k)),
        Change::Retarget { tab, same_conn, from_db, to_db } => (!same_conn || from_db != to_db).then_some(Ends::Tab(tab)),
        Change::Disconnect(id) | Change::Delete(id) => Some(Ends::Conn(id)),
        Change::Edit { id, is_new } => (!is_new).then_some(Ends::Conn(id)),
        Change::Quit => Some(Ends::All),
    }
}

pub(crate) struct TxPrompt {
    pub tabs: Vec<Entity<QueryTab>>,
    pub then: AfterTx,
    pub busy: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ends<'a, K> {
    Tab(K),
    Tabs(&'a [K]),
    Conn(&'a str),
    All,
}

pub(crate) fn tabs_holding<K: PartialEq + Copy>(tabs: &[(K, Option<&str>, bool)], what: Ends<K>) -> Vec<K> {
    tabs.iter()
        .filter(|(k, conn, open)| {
            *open
                && match what {
                    Ends::Tab(t) => t == *k,
                    Ends::Tabs(ts) => ts.contains(k),
                    Ends::Conn(id) => *conn == Some(id),
                    Ends::All => true,
                }
        })
        .map(|(k, _, _)| *k)
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Drag {
    Dock(dataman_core::layout::Dock, usize),
    Split,
    Groups,
}

pub(crate) struct DraggedTab(pub EntityId);

#[derive(Clone)]
pub(crate) enum Bound {
    Conn(Arc<Connection>),
    Tab(EntityId),
    Explorer,
}

impl Bound {
    pub(crate) fn fits(&self, tab: Option<EntityId>, conn: Option<&Arc<Connection>>) -> bool {
        match self {
            Bound::Conn(c) => conn.is_some_and(|x| Arc::ptr_eq(x, c)),
            Bound::Tab(id) => tab == Some(*id),
            Bound::Explorer => true,
        }
    }
}

#[derive(Clone)]
pub(crate) enum Side {
    Server(&'static str, AnyView, Bound),
    Feature(&'static str, AnyView, Bound),
}

pub(crate) fn side_id(s: &Option<Side>) -> Option<&'static str> {
    match s {
        None => None,
        Some(Side::Server(n, ..) | Side::Feature(n, ..)) => Some(n),
    }
}

#[derive(Clone)]
pub(crate) struct SavedFlash {
    pub id: String,
    pub label: &'static str,
    pub at: Instant,
}

impl Side {
    fn bound(&self) -> Option<&Bound> {
        match self {
            Side::Server(_, _, b) | Side::Feature(_, _, b) => Some(b),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Region {
    Explorer,
    Editor,
    Grid,
}

pub struct Workspace {
    pub store: Entity<ConnStore>,
    pub tree: Entity<SchemaTree>,
    pub tabs: Vec<Entity<QueryTab>>,
    pub active: usize,
    next_id: usize,
    pub(crate) dialog: Option<Entity<ConnDialog>>,
    pub(crate) import: Option<Entity<crate::import_ui::ImportView>>,
    pub(crate) history: Entity<HistoryPanel>,
    pub(crate) view: dataman_core::layout::NavView,
    pub(crate) welcome: Entity<Welcome>,
    pub(crate) side: Option<Side>,
    pub(crate) last_side: Option<Side>,
    pub(crate) current_conn: Option<String>,
    pub(crate) explorer_open: bool,
    pub(crate) explorer_w: f32,
    explorer_tween: Option<crate::motion::WidthTween>,
    pub(crate) side_w: f32,
    side_tween: Option<crate::motion::WidthTween>,
    side_was: bool,
    pub(crate) side_seen: Option<Side>,
    pub(crate) layout: dataman_core::layout::PanelLayout,
    pub(crate) dock_moves: Vec<crate::dock::DockMove>,
    pub(crate) panel_drag: Option<crate::dock::PanelDrag>,
    pub(crate) frame_now: crate::dock::Frame,
    pub(crate) title_now: crate::dock::TitleFrame,
    pub(crate) explorer_blend: crate::dock::ExplorerBlend,
    pub(crate) panel_menu: Option<(dataman_core::layout::PanelId, gpui::Point<gpui::Pixels>)>,
    pub(crate) slot_bounds: std::rc::Rc<std::cell::RefCell<[Vec<Vec<gpui::Bounds<gpui::Pixels>>>; 2]>>,
    pub(crate) saved_flash: Option<SavedFlash>,
    saved_flash_task: Option<gpui::Task<()>>,
    pub(crate) rail_bounds: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<dataman_core::layout::NavView, gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) saved_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) logo_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) title_marks: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<&'static str, gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) editor_frac: Option<f32>,
    pub(crate) drag: Option<Drag>,
    pub(crate) tab_bounds: std::rc::Rc<std::cell::RefCell<Vec<gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) tab_drop: Option<usize>,
    pub(crate) tab_drop_pane: Pane,
    pub(crate) tab_bounds_right: std::rc::Rc<std::cell::RefCell<Vec<gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) strip_scroll: [gpui::ScrollHandle; 2],
    pub(crate) strip_front: [std::cell::Cell<Option<EntityId>>; 2],
    pub(crate) strip_parts: std::rc::Rc<std::cell::RefCell<[Vec<gpui::Bounds<gpui::Pixels>>; 2]>>,
    pub(crate) editor_drop: Option<(Pane, crate::split::DropZone)>,
    pub(crate) editor_drop_glide: Option<crate::split::DropGlide>,
    pub(crate) split: Option<crate::split::Split>,
    pub(crate) split_move: Option<crate::split::SplitMove>,
    pub(crate) split_now: Option<crate::split::SplitFrame>,
    pub(crate) open_in: Option<Pane>,
    pub(crate) alt_press: bool,
    pub(crate) pane_bounds: std::rc::Rc<std::cell::RefCell<Vec<gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) close_arm: Option<crate::split::CloseArm>,
    pub(crate) seam_line: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) tab_menu: Option<(EntityId, gpui::Point<gpui::Pixels>)>,
    pub(crate) tab_motion: crate::tab_motion::TabMotion,
    pub(crate) result_tab_bounds: std::rc::Rc<std::cell::RefCell<Vec<gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) messages: crate::messages::Messages,
    pub(crate) messages_right: crate::messages::Messages,
    pub(crate) conn_menu: bool,
    /// When a click outside shut the popup (the click on the button that follows must not
    /// open it again).
    pub(crate) conn_menu_shut: Option<Instant>,
    pub(crate) picker_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) toolbar_parts: std::rc::Rc<std::cell::RefCell<[Vec<gpui::Bounds<gpui::Pixels>>; 2]>>,
    pub(crate) title_drag: bool,
    pub(crate) focus_visible: bool,
    pub(crate) shortcuts_open: bool,
    pub(crate) copy_menu: bool,
    pub(crate) cancel_toast_hidden: Option<Instant>,
    pub(crate) lar_toast: Option<(String, Instant)>,
    pub(crate) flash_notice: Option<(String, Instant)>,
    pub(crate) palette: Option<Entity<crate::palette::Palette>>,
    pub(crate) settings_view: Option<Entity<crate::settings_view::SettingsView>>,
    pub(crate) modal_return: Option<FocusHandle>,
    pub(crate) export: Option<Entity<crate::export_ui::ExportView>>,
    pub(crate) data_import: Option<Entity<crate::data_import::DataImportView>>,
    pub(crate) value_view: Option<Entity<crate::value_view::ValueView>>,
    pub(crate) confirm_focus: FocusHandle,
    pub(crate) confirm_delete: Option<String>,
    pub(crate) confirm_delete_focus: FocusHandle,
    pub(crate) filter_pop: Option<(Entity<crate::table_view::FilterPopover>, (f32, f32))>,
    pub(crate) tx_prompt: Option<TxPrompt>,
    pub(crate) sheet_focus: FocusHandle,
    sheet_at: Option<Instant>,
    tx_tick: Option<(Instant, gpui::Task<()>)>,
    pub(crate) assume_active: Option<bool>,
    focus: FocusHandle,
    pub(crate) mcp_focus: FocusHandle,
    pub(crate) mcp_return: Option<FocusHandle>,
    pub(crate) mcp_buttons: std::rc::Rc<std::cell::RefCell<Vec<gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) crash: Option<dataman_core::crash::Pending>,
    pub(crate) crash_note: Option<&'static str>,
    update_on_quit: bool,
    pub(crate) update_sheet: bool,
    pub(crate) update_focus: FocusHandle,
    pub(crate) update_pill_bounds: std::rc::Rc<std::cell::RefCell<Vec<gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) update_button_bounds: std::rc::Rc<std::cell::RefCell<Vec<gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) session_task: Option<gpui::Task<()>>,
    session_fp: u64,
    pub(crate) session_off: bool,
    pub(crate) nav: crate::nav::Nav,
    pub(crate) zoom_tween: Option<(EntityId, crate::motion::ZoomTween)>,
    pub(crate) result_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    pub(crate) undo: crate::undo::UndoStack,
    pub(crate) seam_from: Option<crate::undo::Sizes>,
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Workspace {
    pub fn new(core: Arc<Core>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = cx.new(|_| ConnStore::new(core));
        let tree = cx.new(|cx| SchemaTree::new(store.clone(), cx));
        let history = cx.new(|cx| HistoryPanel::new(store.clone(), cx));
        let welcome = cx.new(Welcome::new);
        cx.subscribe_in(&tree, window, Self::on_tree).detach();
        cx.subscribe_in(&store, window, Self::on_conn).detach();
        cx.subscribe_in(&welcome, window, Self::on_welcome).detach();
        cx.subscribe_in(&history, window, |w, _, ev: &HistoryEvent, window, cx| match ev {
            HistoryEvent::Insert(sql) => {
                if let Some(tab) = w.tabs.get(w.active).cloned() {
                    let editor = tab.read(cx).editor.clone();
                    let sql = statement_text(sql, tab.read(cx).connection().map(|c| c.engine()));
                    editor.update(cx, |e, cx| e.insert_at_cursor(&sql, cx));
                    window.focus(&editor.focus_handle(cx), cx);
                }
            }
            HistoryEvent::OpenInTab(sql, conn, database) => {
                // Always on the entry's connection: a statement from staging's history must
                // never run on whatever tab happens to be active. A new tab always.
                let (sql, listed, database) = (sql.clone(), Some(conn.clone()), Some(database.clone()));
                let text = statement_text(&sql, w.store.read(cx).profile(conn).map(|p| p.engine));
                let Some(tab) = w.tab_for_history(listed, database, false, window, cx) else { return };
                tab.update(cx, |t, cx| {
                    t.editor.update(cx, |e, cx| e.set_text(&text, cx));
                });
            }
            HistoryEvent::Run(sql, conn, database) => {
                let (sql, listed, database) = (sql.clone(), Some(conn.clone()), Some(database.clone()));
                let text = statement_text(&sql, w.store.read(cx).profile(conn).map(|p| p.engine));
                let Some(tab) = w.tab_for_history(listed, database, true, window, cx) else { return };
                tab.update(cx, |t, cx| {
                    t.editor.update(cx, |e, cx| e.set_text(&text, cx));
                    t.run(vec![sql.trim_end().trim_end_matches(';').to_string()], cx);
                });
            }
            HistoryEvent::OpenFavorite(fav, run) => {
                w.open_in = w.alt_aside(window);
                w.open_favorite(fav.clone(), *run, window, cx);
                w.open_in = None;
            }
            HistoryEvent::ImportFavorites => w.import_favorites(cx),
            HistoryEvent::FavoritesChanged => w.sync_favorite_tabs(cx),
            HistoryEvent::Did(e) => w.undo.push(e.clone()),
            HistoryEvent::Close => {
                w.focus_editor(window, cx);
                cx.notify();
            }
        })
        .detach();
        cx.observe_window_appearance(window, |_, window, cx| theme::on_system_appearance(window, cx)).detach();
        // macOS's "Reduce motion" may have been flipped while the app was in the background.
        cx.observe_window_activation(window, |w: &mut Workspace, window, cx| {
            if window.is_window_active() {
                crate::motion::apply_reduce_motion(cx);
                crate::updates::on_activate(cx);
            }
            w.sync_monitors(window, cx);
        })
        .detach();
        let first = store.read(cx).profiles.first().map(|p| p.id.clone());
        let compact = window.viewport_size().width.pt() <= 1280.;
        let mut w = Workspace {
            store,
            tree,
            tabs: vec![],
            active: 0,
            next_id: 1,
            dialog: None,
            import: None,
            history,
            welcome,
            side: None,
            last_side: None,
            view: NavView::Explorer,
            current_conn: first,
            explorer_open: true,
            explorer_w: if compact { metrics::EXPLORER_COMPACT } else { metrics::EXPLORER_W },
            explorer_tween: None,
            side_w: metrics::SIDE_W,
            side_tween: None,
            side_was: false,
            side_seen: None,
            layout: Default::default(),
            dock_moves: Vec::new(),
            panel_drag: None,
            frame_now: Default::default(),
            title_now: Default::default(),
            explorer_blend: Default::default(),
            panel_menu: None,
            slot_bounds: Default::default(),
            saved_flash: None,
            saved_flash_task: None,
            saved_bounds: Default::default(),
            rail_bounds: Default::default(),
            logo_bounds: Default::default(),
            title_marks: Default::default(),
            editor_frac: None,
            drag: None,
            tab_bounds: Default::default(),
            tab_drop: None,
            tab_drop_pane: Pane::Left,
            tab_bounds_right: Default::default(),
            strip_scroll: Default::default(),
            strip_front: Default::default(),
            strip_parts: Default::default(),
            editor_drop: None,
            editor_drop_glide: None,
            split: None,
            split_move: None,
            split_now: None,
            open_in: None,
            alt_press: false,
            pane_bounds: Default::default(),
            close_arm: None,
            seam_line: Default::default(),
            tab_menu: None,
            tab_motion: Default::default(),
            result_tab_bounds: Default::default(),
            messages: crate::messages::Messages::new(),
            messages_right: crate::messages::Messages::new(),
            conn_menu: false,
            conn_menu_shut: None,
            picker_bounds: Default::default(),
            toolbar_parts: Default::default(),
            title_drag: false,
            focus_visible: false,
            shortcuts_open: false,
            copy_menu: false,
            cancel_toast_hidden: None,
            lar_toast: None,
            flash_notice: None,
            palette: None,
            settings_view: None,
            modal_return: None,
            export: None,
            value_view: None,
            data_import: None,
            confirm_focus: cx.focus_handle(),
            confirm_delete: None,
            confirm_delete_focus: cx.focus_handle(),
            filter_pop: None,
            tx_prompt: None,
            sheet_focus: cx.focus_handle(),
            sheet_at: None,
            tx_tick: None,
            assume_active: None,
            focus: cx.focus_handle(),
            mcp_focus: cx.focus_handle(),
            mcp_return: None,
            mcp_buttons: Default::default(),
            crash: None,
            crash_note: None,
            update_on_quit: false,
            update_sheet: false,
            update_focus: cx.focus_handle(),
            update_pill_bounds: Default::default(),
            update_button_bounds: Default::default(),
            session_task: None,
            session_fp: 0,
            session_off: false,
            nav: Default::default(),
            zoom_tween: None,
            result_bounds: Default::default(),
            undo: Default::default(),
            seam_from: None,
        };
        crate::mcp::register(cx.weak_entity(), window.window_handle(), cx);
        // Esc lets go of any drag (`dnd`), before anything else hears it.
        crate::dnd::cancel_on_escape(cx.weak_entity(), cx);
        let this = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            this.update(cx, |w, cx| {
                let close = w.request_quit(window, cx);
                if close {
                    w.save_session(true, cx);
                }
                close
            })
            .unwrap_or(true)
        });
        cx.on_app_quit(|w, cx| {
            w.save_session(true, cx);
            async {}
        })
        .detach();
        w.watch_connections(window, cx);
        if !w.restore_session(window, cx) {
            w.new_tab(window, cx);
        }
        w.nav.history.clear();
        w.session_fp = w.session_fingerprint(cx);
        if w.first_run(cx) {
            w.focus_editor(window, cx);
        }
        w
    }

    pub fn active_tab(&self) -> Option<&Entity<QueryTab>> {
        self.tabs.get(self.active)
    }

    pub fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<QueryTab> {
        let conn = self.current_conn.as_ref().and_then(|id| self.store.read(cx).connection(id));
        let id = self.next_id;
        self.next_id += 1;
        let tab = cx.new(|cx| QueryTab::new(id, conn, window, cx));
        cx.subscribe_in(&tab, window, |w, tab, ev: &TabEvent, window, cx| match ev {
            TabEvent::Quit => {
                if w.request_quit(window, cx) {
                    w.quit_now(cx);
                }
            }
            TabEvent::Close => {
                if let Some(ix) = w.tabs.iter().position(|t| t == tab) {
                    w.close_tab(ix, window, cx);
                }
            }
            TabEvent::Finished => {
                if w.view != NavView::Explorer {
                    w.history.update(cx, |h, cx| h.reload(cx));
                }
                cx.notify();
            }
            TabEvent::ConfirmDiscard => {
                if w.active_tab() == Some(tab) {
                    w.focus_confirm(window, cx);
                }
                cx.notify();
            }
            TabEvent::Confirm => {
                if w.active_tab() == Some(tab) {
                    w.focus_sheet(window, cx);
                }
                cx.notify();
            }
            TabEvent::Params => {
                if w.active_tab() == Some(tab) {
                    w.focus_params(window, cx);
                }
                cx.notify();
            }
            TabEvent::ParamsClosed => {
                if w.active_tab() == Some(tab) {
                    w.unfocus_sheet(window, cx);
                }
                cx.notify();
            }
            TabEvent::Definition { def, peek } => {
                let (def, peek) = (def.clone(), *peek);
                w.go_to_definition(tab.clone(), def, peek, window, cx);
            }
            TabEvent::Connect(id) => match w.store.read(cx).connection(id) {
                Some(conn) => tab.update(cx, |t, cx| t.adopt(conn, cx)),
                None => {
                    let id = id.clone();
                    w.store.update(cx, |s, cx| s.connect(&id, None, cx));
                }
            },
            TabEvent::TxEnded { result, statements } => {
                let n = *statements;
                let what = |verb: &str| format!("{verb} · {n} statement{}", if n == 1 { "" } else { "s" });
                let msg = match result {
                    Ok(dataman_core::TxEnd::Committed) => what("Committed"),
                    Ok(dataman_core::TxEnd::RolledBack) => what("Rolled back"),
                    Ok(dataman_core::TxEnd::NoTransaction) => "No transaction open".to_string(),
                    Err(e) => format!("COMMIT failed: {e}"),
                };
                w.notice(msg, cx);
            }
        })
        .detach();
        cx.observe(&tab, |_, _, cx| cx.notify()).detach();
        let grid = tab.read(cx).grid.clone();
        let from = tab.downgrade();
        cx.subscribe_in(&grid, window, move |w, _, ev: &GridEvent, window, cx| match ev {
            GridEvent::ViewValue { row, col } => {
                if let Some(cell) = from.upgrade().and_then(|t| t.read(cx).grid.read(cx).value_at(*row, *col)) {
                    w.open_value(cell, window, cx);
                }
            }
            GridEvent::FollowFk(j) => {
                if let Some(from) = from.upgrade() {
                    w.follow_reference(&from, j, window, cx);
                }
            }
            GridEvent::ServerSort { col, dir } => {
                if let Some(from) = from.upgrade() {
                    let (col, dir) = (*col, *dir);
                    from.update(cx, |t, cx| t.table_sort(col, dir, cx));
                }
            }
            GridEvent::Filter { col, at } => w.open_filter(*col, None, *at, window, cx),
            GridEvent::FilterValue { col, value } => {
                if let Some(from) = from.upgrade() {
                    let (col, value) = (*col, value.clone());
                    from.update(cx, |t, cx| t.filter_value(col, value, cx));
                }
            }
            _ => {}
        })
        .detach();
        cx.observe(&grid, |_, _, cx| cx.notify()).detach();
        let editor = tab.read(cx).editor.clone();
        cx.observe(&editor, |_, _, cx| cx.notify()).detach();
        let edit = tab.read(cx).edit.clone();
        cx.subscribe_in(&edit, window, Self::on_edit).detach();
        self.nav_record(Some(window), cx);
        let prev = self.active_tab().map(|t| t.entity_id());
        self.tabs.push(tab.clone());
        self.place_new_tab(tab.entity_id(), prev, cx);
        self.tab_motion.opened(tab.entity_id(), cx);
        self.active = self.tabs.len() - 1;
        self.sync_bound_views(window, cx);
        window.focus(&editor.focus_handle(cx), cx);
        cx.notify();
        tab
    }

    pub(crate) fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix).cloned() else { return };
        if tab.read(cx).edit.read(cx).pending_count() > 0 {
            self.activate(ix, window, cx);
            tab.update(cx, |t, cx| t.guard_edits_as("close", |_, cx| cx.emit(TabEvent::Close), cx));
            return;
        }
        if let Some(what) = ends_of(Change::CloseTab(tab.entity_id()))
            && self.ask_about_tx(what, AfterTx::CloseTab(tab), window, cx)
        {
            return;
        }
        self.close_tab_now(ix, window, cx);
    }

    fn close_tab_now(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.tabs.get(ix).map(|t| t.read(cx).title(cx).to_string());
        if let (Some(c), Some(title)) = (self.close_tab_quietly(ix, window, cx), title) {
            self.did(format!("Close tab '{title}'"), crate::undo::Op::Tabs { closed: vec![c], reopened: vec![] });
        }
    }

    pub(crate) fn close_tab_quietly(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> Option<crate::undo::ClosedTab> {
        if ix >= self.tabs.len() {
            return None;
        }
        let reopen = self.closing(ix, cx);
        let id = self.tabs[ix].entity_id();
        let pane = self.pane_of(id);
        let group: Vec<EntityId> = self.group(pane).into_iter().map(|i| self.tabs[i].entity_id()).collect();
        let next = dataman_core::groups::neighbour(&group, id);
        let mut front = self.active_tab().map(|t| t.entity_id());
        self.tab_folds_away(ix, &group, cx);
        let closed = self.tabs.remove(ix);
        let open: Vec<_> = self.tabs.iter().map(|t| t.entity_id()).collect();
        self.nav.history.retain(|p| open.contains(&p.tab));
        let mut emptied = false;
        if let Some(s) = &mut self.split {
            s.right.retain(|k| *k != id);
            match next {
                None => {
                    if front == Some(id) {
                        front = Some(s.other);
                    }
                    emptied = true;
                }
                Some(n) if s.other == id => s.other = n,
                Some(_) => {}
            }
        }
        if emptied {
            self.split_emptied(pane, Some(closed.downgrade()), cx);
        }
        if front == Some(id) {
            front = next;
        }
        if self.tabs.is_empty() {
            self.new_tab(window, cx);
        }
        self.active = front.and_then(|f| self.tabs.iter().position(|t| t.entity_id() == f)).unwrap_or(0).min(self.tabs.len() - 1);
        self.sync_bound_views(window, cx);
        self.focus_editor(window, cx);
        cx.notify();
        reopen
    }

    pub(crate) fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.tabs.len() {
            if ix != self.active {
                self.nav_record(Some(window), cx);
            }
            self.enter_group_of(ix);
            self.active = ix;
            if let Some(id) = self.tabs[ix].read(cx).connection().map(|c| c.profile.id.clone()) {
                self.current_conn = Some(id);
            }
            self.sync_bound_views(window, cx);
            self.focus_editor(window, cx);
            if self.tabs[ix].read(cx).pending_run.is_some() {
                self.focus_confirm(window, cx);
            }
            if self.tabs[ix].read(cx).confirm.is_some() {
                self.focus_sheet(window, cx);
            }
            if self.tabs[ix].read(cx).params.is_some() {
                self.focus_params(window, cx);
            }
            cx.notify();
        }
    }

    fn focus_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.confirm_focus.is_focused(window) {
            self.modal_return = window.focused(cx).filter(|f| *f != self.confirm_focus);
            window.focus(&self.confirm_focus, cx);
        }
    }

    pub(crate) fn resolve_pending_run(&mut self, discard: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab().cloned() else { return };
        if !discard {
            self.update_on_quit = false;
        }
        tab.update(cx, |t, cx| t.resolve_pending_run(discard, cx));
        match self.modal_return.take() {
            Some(f) => window.focus(&f, cx),
            None => self.focus_editor(window, cx),
        }
        cx.notify();
    }

    pub(crate) fn modal_open(&self, cx: &App) -> bool {
        self.palette.is_some()
            || self.settings_view.is_some()
            || self.dialog.is_some()
            || self.import.is_some()
            || self.export.as_ref().is_some_and(|e| e.read(cx).visible)
            || self.data_import.as_ref().is_some_and(|e| e.read(cx).visible)
            || self.value_view.is_some()
            || self.active_tab().is_some_and(|t| t.read(cx).pending_run.is_some() || t.read(cx).confirm.is_some() || t.read(cx).params.is_some())
            || self.confirm_delete.is_some()
            || self.tx_prompt.is_some()
            || self.update_sheet
            || cx.try_global::<crate::mcp::McpHost>().is_some_and(|h| !h.pending.is_empty())
    }

    pub(crate) fn focus_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.sheet_focus.is_focused(window) {
            self.modal_return = window.focused(cx).filter(|f| *f != self.sheet_focus);
            window.focus(&self.sheet_focus, cx);
        }
        self.sheet_at = Some(Instant::now());
    }

    pub(crate) fn focus_params(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prompt) = self.active_tab().and_then(|t| t.read(cx).params.as_ref().map(|p| p.prompt.clone())) else { return };
        if self.modal_return.is_none() {
            self.modal_return = window.focused(cx);
        }
        prompt.update(cx, |p, cx| p.focus_field(0, window, cx));
    }

    pub(crate) fn render_param_sheet(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let prompt = self.active_tab()?.read(cx).params.as_ref()?.prompt.clone();
        let t = cx.theme();
        let tab = self.active_tab()?.clone();
        Some(
            div()
                .id("param-sheet")
                .absolute()
                .inset_0()
                .bg(t.scrim)
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| tab.update(cx, |t, cx| t.resolve_params(false, cx)))
                .child(prompt)
                .into_any_element(),
        )
    }

    pub(crate) fn go_to_definition(&mut self, tab: Entity<QueryTab>, def: dataman_core::complete::Definition, peek: bool, window: &mut Window, cx: &mut Context<Self>) {
        use crate::tree_menu::{Act, Gen, Obj, Target};
        use dataman_core::complete::Definition;
        let (conn, db) = {
            let t = tab.read(cx);
            let Some(c) = t.connection() else { return };
            (c.clone(), t.database().to_string())
        };
        let id = conn.profile.id.clone();
        let snap = conn.schema.snapshot(&db);
        self.set_explorer_open(true, cx);
        let lar = conn.info.is_lar;
        let ro = conn.read_only_for(&db);
        match def {
            Definition::Relation { schema, name, column } => {
                let kind = snap.resolve(Some(&schema), &name).map(|i| snap.relations[i].kind).unwrap_or(dataman_core::schema::RelKind::Table);
                self.tree.update(cx, |t, cx| t.reveal_relation(&id, &db, &schema, &name, kind, column.as_deref(), cx));
                if peek {
                    let target = Target::Rel { conn: id, db, schema, name, kind, lar, ro };
                    let act = if kind.is_view() { Act::ShowDefinition } else { Act::Generate(Gen::CreateTable) };
                    self.on_row_action(act, target, false, window, cx);
                }
            }
            Definition::Function { schema, name, oid } => {
                self.tree.update(cx, |t, cx| t.reveal_function(&id, &db, &schema, oid, cx));
                if peek {
                    let procedure = snap.catalog.as_ref().and_then(|c| c.functions.iter().find(|f| f.oid == oid)).is_some_and(|f| f.kind == 'p');
                    let target = Target::Object { conn: id, db, schema, name, what: Obj::Function { oid, procedure } };
                    self.on_row_action(Act::ShowDefinition, target, false, window, cx);
                }
            }
            Definition::Local(_) => {}
        }
        cx.notify();
    }

    fn unfocus_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.modal_return.take() {
            Some(f) => window.focus(&f, cx),
            None => self.focus_editor(window, cx),
        }
    }

    fn ask_about_tx(&mut self, what: Ends<gpui::EntityId>, then: AfterTx, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let rows: Vec<(gpui::EntityId, Option<String>, bool)> = self
            .tabs
            .iter()
            .map(|t| {
                let r = t.read(cx);
                (t.entity_id(), r.connection().map(|c| c.profile.id.clone()), r.tx_open())
            })
            .collect();
        let keys: Vec<(gpui::EntityId, Option<&str>, bool)> = rows.iter().map(|(k, c, o)| (*k, c.as_deref(), *o)).collect();
        let held = tabs_holding(&keys, what);
        if held.is_empty() {
            return false;
        }
        if self.tx_prompt.is_some() {
            return true;
        }
        let tabs = self.tabs.iter().filter(|t| held.contains(&t.entity_id())).cloned().collect();
        self.tx_prompt = Some(TxPrompt { tabs, then, busy: false, error: None });
        self.focus_sheet(window, cx);
        cx.notify();
        true
    }

    pub(crate) fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        // Pending grid edits are not kept by the session: the discard sheet first, then the quit
        // again (`TabEvent::Quit`).
        if let Some(ix) = self.tabs.iter().position(|t| t.read(cx).edit.read(cx).pending_count() > 0) {
            let tab = self.tabs[ix].clone();
            self.activate(ix, window, cx);
            tab.update(cx, |t, cx| t.guard_edits_as("quit", |_, cx| cx.emit(TabEvent::Quit), cx));
            return false;
        }
        !ends_of::<EntityId>(Change::Quit).is_some_and(|what| self.ask_about_tx(what, AfterTx::Quit, window, cx))
    }

    fn quit_now(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.update_on_quit)
            && let Some(install) = crate::updates::swap(cx)
        {
            // The swap first, off the UI thread; the app quits and reopens only once the new copy is in
            // place, and stays open if it failed.
            cx.spawn(async move |this, cx| match install.await {
                Ok(app) => {
                    if cx.update(|cx| crate::updates::testing(cx)) {
                        let _ = this.update(cx, |w, cx| {
                            w.update_sheet = false;
                            w.notice(format!("Installed {}", app.display()), cx);
                        });
                        return;
                    }
                    if let Err(e) = dataman_core::update::relaunch_after_exit(&app) {
                        log::warn!("relaunch: {e:#}");
                    }
                    let _ = this.update(cx, |w, cx| {
                        w.store.read(cx).close_all();
                        cx.quit();
                    });
                }
                Err(_) => {
                    let _ = this.update(cx, |_, cx| cx.notify());
                }
            })
            .detach();
            return;
        }
        self.store.read(cx).close_all();
        cx.quit();
    }

    pub(crate) fn install_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(crate::updates::state(cx), crate::updates::State::Ready { .. }) {
            return;
        }
        self.update_on_quit = true;
        if self.request_quit(window, cx) {
            self.quit_now(cx);
        } else {
            self.update_sheet = false;
            cx.notify();
        }
    }

    pub(crate) fn open_urls(&mut self, urls: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        for url in urls {
            let Some(path) = file_url_path(&url) else { continue };
            match dataman_core::sql::read_script(&path) {
                Ok(text) => {
                    let tab = self.new_tab(window, cx);
                    tab.update(cx, |t, cx| t.editor.update(cx, |e, cx| e.set_text(&text, cx)));
                }
                Err(e) => self.notice(format!("cannot read {}: {e}", path.display()), cx),
            }
        }
    }

    pub(crate) fn request_disconnect(&mut self, id: String, reconnect: bool, window: &mut Window, cx: &mut Context<Self>) {
        let then = if reconnect { AfterTx::Reconnect(id.clone()) } else { AfterTx::Disconnect(id.clone()) };
        if let Some(what) = ends_of::<EntityId>(Change::Disconnect(&id))
            && self.ask_about_tx(what, then.clone(), window, cx)
        {
            return;
        }
        self.after_tx(then, window, cx);
    }

    fn after_tx(&mut self, then: AfterTx, window: &mut Window, cx: &mut Context<Self>) {
        match then {
            AfterTx::CloseTab(tab) => {
                if let Some(ix) = self.tabs.iter().position(|t| *t == tab) {
                    self.close_tab_now(ix, window, cx);
                }
            }
            AfterTx::CloseTabs(tabs) => self.close_tabs_now(tabs, window, cx),
            AfterTx::Disconnect(id) => self.disconnect(id, cx),
            AfterTx::Reconnect(id) => {
                self.disconnect(id.clone(), cx);
                self.store.update(cx, |s, cx| s.connect(&id, None, cx));
            }
            AfterTx::Retarget { tab, conn, db, run } => self.retarget_now(&tab, conn, db, run, window, cx),
            AfterTx::SaveConnection(d) => d.update(cx, |d, cx| d.save_now(cx)),
            AfterTx::DeleteConnection(id) => self.delete_connection_now(&id, window, cx),
            AfterTx::Quit => self.quit_now(cx),
        }
    }

    pub(crate) fn retarget(
        &mut self,
        tab: &Entity<QueryTab>,
        conn: Arc<Connection>,
        db: Option<String>,
        run: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let t = tab.read(cx);
        let same_conn = t.connection().is_some_and(|c| Arc::ptr_eq(c, &conn));
        let from_db = t.database().to_string();
        let to_db = db.clone().unwrap_or_else(|| if same_conn { from_db.clone() } else { conn.database().to_string() });
        let change = Change::Retarget { tab: tab.entity_id(), same_conn, from_db: &from_db, to_db: &to_db };
        if let Some(what) = ends_of(change) {
            let then = AfterTx::Retarget { tab: tab.clone(), conn: conn.clone(), db: db.clone(), run: run.clone() };
            if self.ask_about_tx(what, then, window, cx) {
                return;
            }
        }
        self.retarget_now(tab, conn, db, run, window, cx);
    }

    fn retarget_now(
        &mut self,
        tab: &Entity<QueryTab>,
        conn: Arc<Connection>,
        db: Option<String>,
        run: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tab.update(cx, |t, cx| {
            t.set_connection(Some(conn), cx);
            if let Some(db) = db {
                t.set_database(db, cx);
            }
            if let Some(sql) = run {
                t.editor.update(cx, |e, cx| e.set_text(&sql, cx));
                t.run(vec![sql.trim_end_matches(';').to_string()], cx);
            }
        });
        if self.active_tab() == Some(tab) {
            self.focus_editor(window, cx);
        }
        cx.notify();
    }

    fn request_save_connection(&mut self, d: Entity<ConnDialog>, window: &mut Window, cx: &mut Context<Self>) {
        let (id, is_new) = {
            let d = d.read(cx);
            (d.profile_id().to_string(), d.is_new())
        };
        if let Some(what) = ends_of::<EntityId>(Change::Edit { id: &id, is_new })
            && self.ask_about_tx(what, AfterTx::SaveConnection(d.clone()), window, cx)
        {
            return;
        }
        d.update(cx, |d, cx| d.save_now(cx));
    }

    pub(crate) fn request_delete_connection(&mut self, id: String, confirmed: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !confirmed && self.modal_open(cx) {
            return;
        }
        if let Some(what) = ends_of::<EntityId>(Change::Delete(&id))
            && self.ask_about_tx(what, AfterTx::DeleteConnection(id.clone()), window, cx)
        {
            return;
        }
        if confirmed {
            self.delete_connection_now(&id, window, cx);
        } else {
            self.ask_delete_connection(id, window, cx);
        }
    }

    fn delete_connection_now(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.delete_connection(id, cx);
        if self.dialog.as_ref().is_some_and(|d| d.read(cx).profile_id() == id) {
            self.dialog = None;
            self.focus_editor(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn resolve_tx_prompt(&mut self, choice: crate::tab::ConfirmChoice, window: &mut Window, cx: &mut Context<Self>) {
        use crate::tab::ConfirmChoice;
        let Some(p) = &mut self.tx_prompt else { return };
        if choice == ConfirmChoice::Cancel {
            self.tx_prompt = None;
            self.update_on_quit = false;
            self.unfocus_sheet(window, cx);
            cx.notify();
            return;
        }
        if p.busy {
            return;
        }
        p.busy = true;
        p.error = None;
        let commit = choice == ConfirmChoice::Run;
        let tabs = p.tabs.clone();
        let tasks: Vec<_> = tabs.iter().map(|t| (t.clone(), t.update(cx, |t, cx| t.end_tx(commit, cx)))).collect();
        cx.spawn_in(window, async move |this, cx| {
            let mut failed = Vec::new();
            for (tab, task) in tasks {
                if let Err(e) = task.await {
                    failed.push((tab, e));
                }
            }
            let _ = this.update_in(cx, |w, window, cx| {
                let Some(p) = &mut w.tx_prompt else { return };
                p.busy = false;
                if !commit || failed.is_empty() {
                    let then = p.then.clone();
                    w.tx_prompt = None;
                    w.unfocus_sheet(window, cx);
                    w.after_tx(then, window, cx);
                } else {
                    p.error = failed.first().map(|(_, e)| e.clone());
                    p.tabs = failed.into_iter().map(|(t, _)| t).collect();
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn sheet_answer(&mut self, choice: crate::tab::ConfirmChoice, window: &mut Window, cx: &mut Context<Self>) {
        use crate::tab::ConfirmChoice;
        // A ⌘↩ held from the run that raised the sheet does not confirm it.
        if choice != ConfirmChoice::Cancel && self.sheet_at.is_some_and(|t| t.elapsed() < Duration::from_millis(350)) {
            return;
        }
        if self.tx_prompt.is_some() {
            return self.resolve_tx_prompt(choice, window, cx);
        }
        let Some(tab) = self.active_tab().cloned() else { return };
        if tab.update(cx, |t, cx| t.resolve_confirm(choice, cx)) {
            self.unfocus_sheet(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn end_active_tx(&mut self, commit: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab().cloned() else { return };
        let (open, busy) = {
            let t = tab.read(cx);
            (t.tx_open(), t.is_running() || t.ending_tx())
        };
        if !open || busy {
            return;
        }
        tab.update(cx, |t, cx| t.end_tx(commit, cx)).detach();
    }

    pub(crate) fn schedule_tx_tick(&mut self, since: Option<Instant>, cx: &mut Context<Self>) {
        let Some(since) = since else {
            self.tx_tick = None;
            return;
        };
        let age = since.elapsed();
        let next = since + Duration::from_secs((age.as_secs() / 60 + 1) * 60);
        if self.tx_tick.as_ref().is_some_and(|(at, _)| *at == next) {
            return;
        }
        let wait = next.saturating_duration_since(Instant::now());
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        });
        self.tx_tick = Some((next, task));
    }

    pub(crate) fn activate_tab(&mut self, tab: &Entity<QueryTab>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.tabs.iter().position(|t| t == tab) {
            self.activate(ix, window, cx);
        }
    }

    pub(crate) fn focus_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.first_run(cx) {
            let f = self.welcome.focus_handle(cx);
            window.focus(&f, cx);
            return;
        }
        if let Some(t) = self.active_tab() {
            let f = match &t.read(cx).monitor {
                Some(m) => m.read(cx).focus_target(cx),
                None if t.read(cx).zoomed && t.read(cx).has_run => match &t.read(cx).doc {
                    Some(d) => d.read(cx).list_focus(),
                    None => t.read(cx).grid.focus_handle(cx),
                },
                None => t.read(cx).editor.focus_handle(cx),
            };
            window.focus(&f, cx);
        }
    }

    fn tab_for_history(
        &mut self, listed: Option<String>, database: Option<String>, run: bool, window: &mut Window, cx: &mut Context<Self>,
    ) -> Option<Entity<QueryTab>> {
        let on = |t: &Entity<QueryTab>, cx: &App| t.read(cx).connection().map(|c| c.profile.id.clone()) == listed;
        if run {
            if let Some(t) = self.active_tab().filter(|t| on(t, cx)).cloned() {
                return Some(t);
            }
            if let Some(t) = self.tabs.iter().find(|t| on(t, cx)).cloned() {
                self.activate_tab(&t, window, cx);
                return Some(t);
            }
        }
        let conn = match &listed {
            Some(id) => match self.store.read(cx).connection(id) {
                Some(c) => Some(c),
                None => {
                    let name = self.store.read(cx).profile(id).map(|p| p.name.clone()).unwrap_or_else(|| id.clone());
                    self.notice(format!("Connect to {name} first: this is its history"), cx);
                    return None;
                }
            },
            None => None,
        };
        let has_conn = conn.is_some();
        let tab = self.new_tab(window, cx);
        tab.update(cx, |t, cx| {
            t.set_connection(conn, cx);
            if has_conn {
                if let Some(db) = database {
                    t.set_database(db, cx);
                }
            }
        });
        Some(tab)
    }

    pub fn close_dialog(&mut self, cx: &mut Context<Self>) {
        self.dialog = None;
        cx.notify();
    }

    pub fn open_dialog(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let profile = id.and_then(|id| self.store.read(cx).profile(&id).cloned());
        self.open_dialog_with(profile, false, None, window, cx);
    }

    pub fn open_dialog_with(
        &mut self,
        profile: Option<ConnectionProfile>,
        is_new: bool,
        password: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open(cx) {
            return;
        }
        let store = self.store.clone();
        let d = cx.new(|cx| {
            let mut d = ConnDialog::new(store, profile, is_new, window, cx);
            if let Some(pw) = &password {
                d.set_password(pw, cx);
            }
            d
        });
        cx.subscribe_in(&d, window, |w, d, ev: &DialogEvent, window, cx| {
            match ev {
                DialogEvent::Close => {}
                DialogEvent::Saved { id, password } => {
                    w.current_conn = Some(id.clone());
                    let (id, pw) = (id.clone(), password.clone());
                    w.store.update(cx, |s, cx| s.connect(&id, pw, cx));
                }
                DialogEvent::Save => return w.request_save_connection(d.clone(), window, cx),
                DialogEvent::Delete(id) => return w.request_delete_connection(id.clone(), true, window, cx),
            }
            w.dialog = None;
            w.focus_editor(window, cx);
            cx.notify();
        })
        .detach();
        self.dialog = Some(d);
        cx.notify();
    }

    pub(crate) fn new_connection_from_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default();
        match dataman_core::connstr::parse_profile(&text) {
            Some((p, pw)) => self.open_dialog_with(Some(p), true, pw, window, cx),
            None => self.notice("No connection string on the clipboard", cx),
        }
    }

    pub(crate) fn open_import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open(cx) {
            return;
        }
        self.modal_return = window.focused(cx);
        let store = self.store.clone();
        let v = cx.new(|cx| crate::import_ui::ImportView::new(store, window, cx));
        cx.subscribe_in(&v, window, |w, _, ev: &crate::import_ui::ImportEvent, window, cx| {
            if let crate::import_ui::ImportEvent::Imported(ids) = ev
                && let Some(first) = ids.first()
            {
                w.current_conn = Some(first.clone());
            }
            w.import = None;
            match w.modal_return.take() {
                Some(f) => window.focus(&f, cx),
                None => w.focus_editor(window, cx),
            }
            cx.notify();
        })
        .detach();
        self.import = Some(v);
        cx.notify();
    }

    pub(crate) fn open_value(&mut self, cell: crate::value_view::ValueCell, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open(cx) {
            return;
        }
        self.modal_return = window.focused(cx);
        let v = cx.new(|cx| crate::value_view::ValueView::new(cell, window, cx));
        cx.subscribe_in(&v, window, |w, _, ev: &crate::value_view::ValueEvent, window, cx| match ev {
            crate::value_view::ValueEvent::Close => {
                w.value_view = None;
                match w.modal_return.take() {
                    Some(f) => window.focus(&f, cx),
                    None => w.focus_editor(window, cx),
                }
                cx.notify();
            }
        })
        .detach();
        self.value_view = Some(v);
        cx.notify();
    }

    fn on_welcome(&mut self, _: &Entity<Welcome>, ev: &WelcomeEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            WelcomeEvent::NewConnection => self.open_dialog(None, window, cx),
            WelcomeEvent::AgentSetup => self.open_settings_at("mcp", window, cx),
            WelcomeEvent::Prefill(p, pw) => self.open_dialog_with(Some(p.clone()), true, pw.clone(), window, cx),
            WelcomeEvent::OpenFile => self.open_sql_file(window, cx),
            WelcomeEvent::Shortcuts => {
                self.shortcuts_open = true;
                cx.notify();
            }
        }
    }

    fn on_tree(&mut self, _: &Entity<SchemaTree>, ev: &TreeEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            TreeEvent::Selected(id) => {
                self.current_conn = Some(id.clone());
                let conn = self.store.read(cx).connection(id);
                if let (Some(tab), Some(conn)) = (self.active_tab().cloned(), conn) {
                    tab.update(cx, |t, cx| {
                        if t.connection().is_none() {
                            t.set_connection(Some(conn), cx)
                        }
                    });
                }
            }
            TreeEvent::OpenRelation { conn, db, schema, name } => {
                self.open_in = self.alt_aside(window);
                self.open_relation(conn, db, schema, name, window, cx);
                self.open_in = None;
            }
            TreeEvent::OpenCollection { conn, db, coll } => {
                self.open_in = self.alt_aside(window);
                self.open_collection(conn, db, coll, window, cx);
                self.open_in = None;
            }
            TreeEvent::EditConnection(id) => self.open_dialog(Some(id.clone()), window, cx),
            TreeEvent::NewConnection => self.open_dialog(None, window, cx),
            TreeEvent::Disconnect(id) => self.request_disconnect(id.clone(), false, window, cx),
            TreeEvent::FocusEditor => self.focus_editor(window, cx),
            TreeEvent::RowAction { act, target, alt } => self.on_row_action(*act, target.clone(), *alt, window, cx),
            TreeEvent::Notice(msg) => self.notice(msg.clone(), cx),
            TreeEvent::Did(e) => self.undo.push(e.clone()),
        }
        cx.notify();
    }

    pub(crate) fn open_relation(&mut self, conn: &str, db: &str, schema: &str, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.store.read(cx).connection(conn) else {
            self.notice("Connect first", cx);
            return;
        };
        self.current_conn = Some(conn.to_string());
        let tab = self.new_tab(window, cx);
        tab.update(cx, |t, cx| {
            t.set_connection(Some(c.clone()), cx);
            t.set_database(db.to_string(), cx);
        });
        let page = self.store.read(cx).core.settings.get().page_size.max(1) as u64;
        let task = c.primary_key(db, schema, name);
        let (schema, name) = (schema.to_string(), name.to_string());
        cx.spawn_in(window, async move |_, cx| {
            let key: Vec<String> = task.await.unwrap_or_default();
            let _ = tab.update(cx, |t, cx| t.open_table(dataman_core::dataview::TableView::new(schema, name, key, page), cx));
        })
        .detach();
    }

    pub(crate) fn follow_reference(&mut self, from: &Entity<QueryTab>, j: &dataman_core::fknav::FkJump, window: &mut Window, cx: &mut Context<Self>) {
        let (conn, db) = {
            let t = from.read(cx);
            (t.connection().map(|c| c.profile.id.clone()), t.database().to_string())
        };
        let Some(conn) = conn else { return };
        if let Some(tab) = self.open_sql_tab(&conn, Some(&db), &j.sql, true, window, cx) {
            tab.update(cx, |t, cx| t.set_title(j.title.clone(), j.sql.clone(), cx));
            let grid = tab.read(cx).grid.clone();
            grid.update(cx, |g, _| g.land_on(Some(j.key.clone()).filter(|k| !k.is_empty())));
        }
    }

    fn on_edit(&mut self, edit: &Entity<crate::edit_ui::GridEdit>, ev: &crate::edit_ui::EditEvent, _: &mut Window, cx: &mut Context<Self>) {
        use crate::edit_ui::EditEvent;
        match ev {
            EditEvent::OpenReview => {
                let tab = self.active_tab().filter(|t| t.read(cx).edit == *edit).map(|t| t.entity_id());
                match tab {
                    Some(id) => {
                        let view = cx.new(|cx| crate::edit_ui::ReviewPanel::new(edit.clone(), cx));
                        edit.update(cx, |e, _| e.review_open = true);
                        self.side_w = self.side_w.max(420.);
                        self.side = Some(Side::Feature("review", view.into(), Bound::Tab(id)));
                    }
                    None => edit.update(cx, |e, _| e.review_open = false),
                }
            }
            EditEvent::CloseReview => {
                edit.update(cx, |e, _| e.review_open = false);
                if let Some(Side::Feature("review", v, _)) = &self.side
                    && v.clone().downcast::<crate::edit_ui::ReviewPanel>().is_ok_and(|r| r.read(cx).edit == *edit)
                {
                    self.side = None;
                }
            }
            EditEvent::Committed { rows } => {
                self.notice(format!("Committed {rows} statement{} · reloading the result", if *rows == 1 { "" } else { "s" }), cx)
            }
            EditEvent::Changed => {}
        }
        cx.notify();
    }

    fn active_edit(&self, cx: &App) -> Option<Entity<crate::edit_ui::GridEdit>> {
        self.active_tab().map(|t| t.read(cx).edit.clone())
    }

    pub(crate) fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_view.take().is_some() {
            match self.modal_return.take() {
                Some(f) => window.focus(&f, cx),
                None => self.focus_editor(window, cx),
            }
            cx.notify();
            return;
        }
        if self.modal_open(cx) {
            return;
        }
        self.modal_return = window.focused(cx);
        let core = self.store.read(cx).core.clone();
        let v = cx.new(|cx| crate::settings_view::SettingsView::new(core, window, cx));
        cx.subscribe_in(&v, window, |w, _, ev: &crate::settings_view::SettingsEvent, window, cx| match ev {
            crate::settings_view::SettingsEvent::Close => {
                if w.settings_view.is_some() {
                    w.toggle_settings(window, cx);
                }
            }
            crate::settings_view::SettingsEvent::ConnectionsChanged => w.store.update(cx, |s, cx| s.reload(cx)),
        })
        .detach();
        self.settings_view = Some(v);
        cx.notify();
    }

    pub(crate) fn open_settings_at(&mut self, section: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_view.is_none() {
            self.toggle_settings(window, cx);
        }
        if let Some(v) = self.settings_view.clone() {
            v.update(cx, |v, cx| v.show_section(section, cx));
        }
    }

    pub(crate) fn disconnect(&mut self, id: String, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| s.disconnect(&id, cx));
        for t in self.tabs.clone() {
            t.update(cx, |t, cx| {
                if t.connection().is_some_and(|c| c.profile.id == id) {
                    t.set_connection(None, cx);
                }
            });
        }
    }

    fn on_conn(&mut self, _: &Entity<ConnStore>, ev: &ConnEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            ConnEvent::Connected(id) => {
                self.current_conn = Some(id.clone());
                let conn = self.store.read(cx).connection(id);
                for tab in self.tabs.clone() {
                    let conn = conn.clone();
                    tab.update(cx, |t, cx| {
                        if let Some((want, _)) = &t.want {
                            if want == id
                                && let Some(c) = conn
                            {
                                t.adopt(c, cx);
                            }
                            return;
                        }
                        let stale = !t.tx_open()
                            && !t.is_running()
                            && t.connection().is_some_and(|c| c.profile.id == *id && conn.as_ref().is_some_and(|n| !Arc::ptr_eq(c, n)));
                        if t.connection().is_none() || stale {
                            t.set_connection(conn, cx);
                        }
                    });
                }
                if let Some(c) = &conn
                    && c.info.is_lar
                {
                    self.show_lar_toast(c.profile.name.clone(), cx);
                }
                let cid = format!("c:{id}");
                self.tree.update(cx, |t, cx| {
                    t.expand_id(&cid, cx);
                });
            }
            ConnEvent::Failed { id, ssh: Some(err), .. } => {
                if self.dialog.is_none() {
                    self.open_dialog(Some(id.clone()), window, cx);
                    if let Some(d) = self.dialog.clone() {
                        d.update(cx, |d, cx| match err {
                            dataman_core::SshError::UnknownHostKey(p) => d.set_host_key(p.clone(), cx),
                            e => d.set_ssh_failed(e.to_string(), true, window, cx),
                        });
                    }
                }
            }
            ConnEvent::Failed { id, needs_password, .. } => {
                if *needs_password && self.dialog.is_none() {
                    self.open_dialog(Some(id.clone()), window, cx);
                    if let Some(d) = self.dialog.clone() {
                        d.update(cx, |d, cx| d.set_password_needed(cx));
                        let f = d.read(cx).password_focus(cx);
                        window.focus(&f, cx);
                    }
                }
            }
        }
        cx.notify();
    }

    fn show_lar_toast(&mut self, name: String, cx: &mut Context<Self>) {
        let at = Instant::now();
        self.lar_toast = Some((name, at));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(6)).await;
            let _ = this.update(cx, |w, cx| {
                if w.lar_toast.as_ref().is_some_and(|(_, t)| *t == at) {
                    w.lar_toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn notice(&mut self, msg: impl Into<String>, cx: &mut Context<Self>) {
        let at = Instant::now();
        self.flash_notice = Some((msg.into(), at));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(4)).await;
            let _ = this.update(cx, |w, cx| {
                if w.flash_notice.as_ref().is_some_and(|(_, t)| *t == at) {
                    w.flash_notice = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn toggle_server_panel(&mut self, id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.active_tab().and_then(|t| t.read(cx).connection().cloned()) else { return };
        let Some(panel) = cx.try_global::<PanelRegistry>().and_then(|r| r.find(&conn.info, id)) else { return };
        if matches!(&self.side, Some(Side::Server(pid, _, b)) if *pid == id && b.fits(None, Some(&conn))) {
            self.last_side = self.side.take();
        } else {
            if let Some(w) = panel.width() {
                self.side_w = w.clamp(metrics::SIDE_MIN, metrics::SIDE_MAX);
            }
            let view = panel.build(conn.clone(), window, cx);
            self.side = Some(Side::Server(id, view, Bound::Conn(conn)));
        }
        cx.notify();
    }

    pub(crate) fn sync_bound_views(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.active_tab().cloned();
        let tab_id = tab.as_ref().map(|t| t.entity_id());
        let conn = tab.as_ref().and_then(|t| t.read(cx).connection().cloned());
        let fits = |b: &Bound| b.fits(tab_id, conn.as_ref());
        if let Some(side) = self.side.clone()
            && let Some(b) = side.bound()
            && !fits(b)
        {
            let rebuilt = match (&side, b, &conn) {
                (Side::Server(id, ..), Bound::Conn(_), Some(c)) => cx
                    .try_global::<PanelRegistry>()
                    .and_then(|r| r.find(&c.info, id))
                    .map(|p| (*id, p.build(c.clone(), window, cx), c.clone())),
                _ => None,
            };
            if let Side::Feature("review", v, _) = &side
                && let Ok(r) = v.clone().downcast::<crate::edit_ui::ReviewPanel>()
            {
                let edit = r.read(cx).edit.clone();
                edit.update(cx, |e, _| e.review_open = false);
            }
            self.side = rebuilt.map(|(id, v, c)| Side::Server(id, v, Bound::Conn(c)));
        }
        if self.last_side.as_ref().and_then(|s| s.bound()).is_some_and(|b| !fits(b)) {
            self.last_side = None;
        }
    }

    pub(crate) fn open_sql_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = crate::sql_picker::pick_sql_files(true, cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(paths) = paths.await else { return };
            let _ = this.update_in(cx, |w, window, cx| w.open_sql_paths(&paths, window, cx));
        })
        .detach();
    }

    pub(crate) fn open_sql_paths(&mut self, paths: &[std::path::PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        for path in paths {
            let text = match dataman_core::sql::read_script(path) {
                Ok(t) => t,
                Err(e) => {
                    self.notice(format!("cannot read {}: {e}", path.display()), cx);
                    continue;
                }
            };
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("query.sql").to_string();
            let tab = self.new_tab(window, cx);
            tab.update(cx, |t, cx| {
                t.editor.update(cx, |e, cx| e.set_text(&text, cx));
                let sql = t.editor.read(cx).text().to_string();
                t.set_title(name, sql, cx);
            });
        }
    }

    pub(crate) fn focused_region(&self, window: &Window, cx: &App) -> Option<Region> {
        if self.tree.read(cx).contains_focus(window, cx) {
            return Some(Region::Explorer);
        }
        if let Some(t) = self.active_tab() {
            let t = t.read(cx);
            if t.editor.focus_handle(cx).contains_focused(window, cx) || t.editor.read(cx).find_has_focus(window, cx) {
                return Some(Region::Editor);
            }
            if t.grid.focus_handle(cx).contains_focused(window, cx)
                || t.grid.read(cx).find_has_focus(window, cx)
                || t.plan.focus_handle(cx).contains_focused(window, cx)
                || t.doc.as_ref().is_some_and(|d| d.read(cx).list_focus().contains_focused(window, cx))
            {
                return Some(Region::Grid);
            }
        }
        if self.explorer_open && self.view != NavView::Explorer && self.history.focus_handle(cx).contains_focused(window, cx) {
            return Some(Region::Explorer);
        }
        None
    }

    pub(crate) fn set_explorer_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if open == self.explorer_open {
            return;
        }
        let from = self.explorer_now();
        self.explorer_open = open;
        let to = self.explorer_target();
        self.explorer_tween = (!cx.reduce_motion()).then(|| crate::motion::WidthTween::panel(from, to, metrics::EXPLORER_W));
        cx.notify();
    }

    pub(crate) fn toggle_result_zoom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab().cloned() else { return };
        if !tab.read(cx).has_run || tab.read(cx).monitor.is_some() {
            return;
        }
        let from = self.zoom_now(&tab, cx);
        let on = !tab.read(cx).zoomed;
        let side = side_id(&self.side);
        tab.update(cx, |t, _| {
            t.zoomed = on;
            t.zoom_side = side;
        });
        self.zoom_tween = (!cx.reduce_motion()).then(|| (tab.entity_id(), crate::motion::ZoomTween::new(from)));
        if on {
            let g = match &tab.read(cx).doc {
                Some(d) => d.read(cx).list_focus(),
                None => tab.read(cx).grid.focus_handle(cx),
            };
            window.focus(&g, cx);
        }
        cx.notify();
    }

    pub(crate) fn zoom_now(&self, tab: &Entity<QueryTab>, cx: &App) -> f32 {
        let target = if tab.read(cx).zoomed { 1. } else { 0. };
        match self.zoom_tween {
            Some((id, z)) if id == tab.entity_id() => z.at(target, Instant::now()).unwrap_or(target),
            _ => target,
        }
    }

    pub(crate) fn zoom_frame(&mut self, tab: &Entity<QueryTab>, window: &mut Window, cx: &App) -> f32 {
        let target = if tab.read(cx).zoomed { 1. } else { 0. };
        if let Some((id, z)) = self.zoom_tween {
            match z.at(target, Instant::now()) {
                Some(p) if id == tab.entity_id() => {
                    window.request_animation_frame();
                    return p;
                }
                Some(_) => {}
                None => self.zoom_tween = None,
            }
        }
        target
    }

    pub(crate) fn side_slide(&mut self, window: &mut Window, cx: &App) -> Option<f32> {
        let open = self.side.is_some();
        if open != self.side_was {
            let from = self.side_tween.and_then(|t| t.at(if self.side_was { self.side_w } else { 0. }, Instant::now()));
            let (a, b) = if open { (from.unwrap_or(0.), self.side_w) } else { (from.unwrap_or(self.side_w), 0.) };
            self.side_tween = (!cx.reduce_motion()).then(|| crate::motion::WidthTween::panel(a, b, metrics::SIDE_W));
            self.side_was = open;
        }
        let target = if open { self.side_w } else { 0. };
        match self.side_tween.and_then(|t| t.at(target, Instant::now())) {
            Some(w) => {
                window.request_animation_frame();
                Some(w)
            }
            None => {
                self.side_tween = None;
                open.then_some(self.side_w)
            }
        }
    }

    pub(crate) fn demo_explorer_sliding(&self) -> bool {
        self.explorer_tween.is_some_and(|t| t.at(self.explorer_target(), Instant::now()).is_some())
    }

    pub(crate) fn demo_side_sliding(&self) -> bool {
        self.side_tween.is_some_and(|t| t.at(0., Instant::now()).is_some())
    }

    fn explorer_target(&self) -> f32 {
        if self.explorer_open { self.explorer_w } else { 0. }
    }

    pub(crate) fn explorer_now(&self) -> f32 {
        let target = self.explorer_target();
        self.explorer_tween.and_then(|t| t.at(target, std::time::Instant::now())).unwrap_or(target)
    }

    pub(crate) fn focus_region(&mut self, r: Region, window: &mut Window, cx: &mut Context<Self>) {
        match r {
            Region::Explorer => {
                self.set_explorer_open(true, cx);
                let f = match self.view {
                    NavView::Explorer => self.tree.focus_handle(cx),
                    _ => self.history.focus_handle(cx),
                };
                window.focus(&f, cx);
            }
            Region::Editor => self.focus_editor(window, cx),
            Region::Grid => {
                if !self.results_shown(cx) {
                    return;
                }
                if let Some(t) = self.active_tab() {
                    let f = if t.read(cx).shows_plan(cx) { t.read(cx).plan.focus_handle(cx) } else { t.read(cx).grid.focus_handle(cx) };
                    window.focus(&f, cx);
                }
            }
        }
        self.focus_visible = true;
        cx.notify();
    }

    fn cycle_focus(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mut order = vec![];
        if self.explorer_open {
            order.push(Region::Explorer);
        }
        order.push(Region::Editor);
        if self.results_shown(cx) {
            order.push(Region::Grid);
        }
        let cur = self.focused_region(window, cx).and_then(|r| order.iter().position(|x| *x == r));
        let n = order.len();
        let next = match cur {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None => order.iter().position(|r| *r == Region::Editor).unwrap_or(0),
        };
        self.focus_region(order[next], window, cx);
    }

    fn jump_to_error(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab().cloned() else { return };
        let editor = tab.read(cx).editor.clone();
        if editor.update(cx, |e, cx| e.jump_to_error(cx)) {
            if tab.read(cx).zoomed {
                self.toggle_result_zoom(window, cx);
            }
            window.focus(&editor.focus_handle(cx), cx);
            self.focus_visible = true;
        }
    }

    fn find_in_editor(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.first_run(cx) {
            return;
        }
        let Some(tab) = self.active_tab().cloned() else { return };
        let editor = tab.read(cx).editor.clone();
        editor.update(cx, |e, cx| e.open_find(replace, window, cx));
    }

    fn find_step_in_editor(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.first_run(cx) {
            return;
        }
        let Some(tab) = self.active_tab().cloned() else { return };
        let editor = tab.read(cx).editor.clone();
        editor.update(cx, |e, cx| e.find_step(forward, window, cx));
    }

    pub(crate) fn use_explorer_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = self.current_conn.as_ref().and_then(|id| self.store.read(cx).connection(id));
        if let (Some(t), Some(conn)) = (self.active_tab().cloned(), conn) {
            self.retarget(&t, conn, None, None, window, cx);
        }
    }

    pub(crate) fn run_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = self.active_tab() {
            let e = t.read(cx).editor.clone();
            window.focus(&e.focus_handle(cx), cx);
            e.update(cx, |e, cx| e.run_script(cx));
        }
    }

    fn sync_menu_caps(&mut self, cx: &mut Context<Self>) {
        let conn = self.active_tab().and_then(|t| t.read(cx).connection().cloned());
        let engine = match conn {
            Some(c) => c.engine(),
            None => self.current_conn.as_ref().and_then(|id| self.store.read(cx).profile(id)).map(|p| p.engine).unwrap_or_default(),
        };
        let caps = dataman_core::Caps::of(engine);
        let shown = cx.try_global::<crate::MenuCaps>().map_or(dataman_core::Caps::POSTGRES, |m| m.0);
        if shown != caps {
            cx.set_global(crate::MenuCaps(caps));
            cx.defer(crate::refresh_menus);
        }
    }

    pub(crate) fn favorite_current_query(&mut self, cx: &mut Context<Self>) {
        let Some(t) = self.active_tab().cloned() else { return };
        let Some(stmt) = t.update(cx, |t, cx| t.editor.update(cx, |e, cx| e.current_statement(cx))) else { return };
        let sql = stmt.sql.trim().to_string();
        if sql.is_empty() {
            return;
        }
        let (connection_id, database) = match t.read(cx).connection() {
            Some(c) => (Some(c.profile.id.clone()), Some(t.read(cx).database().to_string())),
            None => (None, None),
        };
        let mut fav = Favorite::new(crate::history::fav_name(&sql), sql);
        fav.connection_id = connection_id;
        fav.database = database;
        let store = self.store.read(cx).core.favorites.clone();
        let new = fav.id.clone();
        if let Ok(saved) = store.add(fav) {
            if saved.id == new {
                let ix = store.list().iter().position(|f| f.id == saved.id).unwrap_or(0);
                self.did(format!("Add favorite '{}'", saved.name), crate::undo::Op::Favorite { fav: saved, ix, added: true });
            }
            self.notice("Added to favorites", cx);
            self.history.update(cx, |h, cx| h.reload_favorites(cx));
        }
    }

    pub(crate) fn import_favorites(&mut self, cx: &mut Context<Self>) {
        let rx = crate::sql_picker::pick_sql_files(true, cx);
        cx.spawn(async move |this, cx| {
            let Ok(paths) = rx.await else { return };
            let _ = this.update(cx, |w, cx| w.import_favorite_paths(&paths, cx));
        })
        .detach();
    }

    pub(crate) fn import_favorite_paths(&mut self, paths: &[std::path::PathBuf], cx: &mut Context<Self>) {
        let store = self.store.read(cx).core.favorites.clone();
        let mut added = Vec::new();
        for path in paths {
            if crate::sql_picker::is_sql(path)
                && let Ok(sql) = dataman_core::sql::read_script(path)
            {
                let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("favorite").to_string();
                let known = store.find_by_sql(&sql).is_some();
                if !known && let Ok(fav) = store.add(Favorite::new(name, sql)) {
                    let ix = store.list().iter().position(|f| f.id == fav.id).unwrap_or(0);
                    added.push(crate::undo::Op::Favorite { fav, ix, added: true });
                }
            }
        }
        if !added.is_empty() {
            let n = added.len();
            self.notice(format!("Added {n} to favorites"), cx);
            self.did(format!("Import {n} favorite{}", if n == 1 { "" } else { "s" }), crate::undo::Op::Many(added));
        }
        self.history.update(cx, |h, cx| h.reload_favorites(cx));
    }

    pub(crate) fn open_favorite(&mut self, fav: Favorite, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let offline = fav.connection_id.clone().filter(|id| self.store.read(cx).connection(id).is_none() && self.store.read(cx).profile(id).is_some());
        let tab = match &offline {
            Some(id) => {
                let tab = self.new_tab(window, cx);
                let (id, database) = (id.clone(), fav.database.clone());
                tab.update(cx, |t, cx| {
                    t.set_connection(None, cx);
                    t.want = Some((id.clone(), database));
                });
                self.store.update(cx, |s, cx| s.connect(&id, None, cx));
                tab
            }
            None => {
                let Some(tab) = self.tab_for_history(fav.connection_id.clone(), fav.database.clone(), false, window, cx) else { return };
                tab
            }
        };
        tab.update(cx, |t, cx| {
            let engine = fav.connection_id.as_deref().and_then(|id| self.store.read(cx).profile(id)).map(|p| p.engine);
            let text = statement_text(&fav.sql, engine);
            t.editor.update(cx, |e, cx| e.set_text(&text, cx));
            t.favorite = Some((fav.id.clone(), fav.name.clone().into()));
            if run {
                t.run(vec![fav.sql.trim_end().trim_end_matches(';').to_string()], cx);
            }
            cx.notify();
        });
    }

    pub(crate) fn save_favorite(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab().cloned() {
            self.save_favorite_of(&tab, cx);
        }
    }

    pub(crate) fn can_save_favorite(tab: &Entity<QueryTab>, cx: &App) -> bool {
        tab.read(cx).monitor.is_none() && !tab.read(cx).editor.read(cx).text().trim().is_empty()
    }

    pub(crate) fn save_favorite_of(&mut self, tab: &Entity<QueryTab>, cx: &mut Context<Self>) {
        if !Self::can_save_favorite(tab, cx) {
            return;
        }
        let sql = tab.read(cx).editor.read(cx).text().trim().to_string();
        let store = self.store.read(cx).core.favorites.clone();
        let updated = match tab.read(cx).favorite.clone() {
            Some((id, _)) => match store.update_sql(&id, &sql) {
                Ok(f) => f,
                Err(e) => {
                    self.notice(format!("cannot save the favorite: {e:#}"), cx);
                    return;
                }
            },
            None => None,
        };
        let (fav, label) = match updated {
            Some(f) => (f, "saved to favorites"),
            None => {
                let names: Vec<String> = store.list().into_iter().map(|f| f.name).collect();
                let taken: Vec<&str> = names.iter().map(String::as_str).collect();
                let name = dataman_core::favorites::unique_name(&tab.read(cx).favorite_name(cx), &taken);
                let mut fav = Favorite::new(name, sql);
                if let Some(c) = tab.read(cx).connection() {
                    fav.connection_id = Some(c.profile.id.clone());
                    fav.database = Some(tab.read(cx).database().to_string());
                }
                match store.add(fav) {
                    Ok(f) => {
                        let ix = store.list().iter().position(|x| x.id == f.id).unwrap_or(0);
                        self.did(format!("Add favorite '{}'", f.name), crate::undo::Op::Favorite { fav: f.clone(), ix, added: true });
                        (f, "added to favorites")
                    }
                    Err(e) => {
                        self.notice(format!("cannot save the favorite: {e:#}"), cx);
                        return;
                    }
                }
            }
        };
        tab.update(cx, |t, cx| {
            t.favorite = Some((fav.id.clone(), fav.name.clone().into()));
            cx.notify();
        });
        self.history.update(cx, |h, cx| h.reload_favorites(cx));
        self.flash_saved(fav.id, label, cx);
    }

    fn flash_saved(&mut self, id: String, label: &'static str, cx: &mut Context<Self>) {
        if !self.explorer_open {
            let mut said = label.to_string();
            said[..1].make_ascii_uppercase();
            self.notice(said, cx);
        }
        let at = Instant::now();
        self.saved_flash = Some(SavedFlash { id, label, at });
        let redraws = crate::motion::SAVED.redraws(cx.reduce_motion());
        self.saved_flash_task = Some(cx.spawn(async move |w, cx| {
            for d in redraws {
                cx.background_executor().timer(d.saturating_sub(at.elapsed())).await;
                if w.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
            let _ = w.update(cx, |w, cx| {
                if w.saved_flash.as_ref().is_some_and(|f| f.at == at) {
                    w.saved_flash = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    pub(crate) fn show_favorite(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.set_nav(NavView::Favorites, cx);
        self.set_explorer_open(true, cx);
        self.saved_flash = None;
        if !self.history.update(cx, |h, cx| h.focus_favorite(id, window, cx)) {
            self.notice("That favorite was removed", cx);
        }
        cx.notify();
    }

    pub(crate) fn sync_favorite_tabs(&mut self, cx: &mut Context<Self>) {
        let favorites = self.store.read(cx).core.favorites.list();
        for tab in self.tabs.clone() {
            let Some((id, name)) = tab.read(cx).favorite.clone() else { continue };
            let now: Option<gpui::SharedString> = favorites.iter().find(|f| f.id == id).map(|f| f.name.clone().into());
            if now.as_ref() != Some(&name) {
                tab.update(cx, |t, cx| {
                    t.favorite = now.map(|n| (id, n));
                    cx.notify();
                });
            }
        }
    }

    pub(crate) fn save_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab().cloned() else { return };
        if tab.read(cx).monitor.is_some() {
            return;
        }
        let name = crate::sql_picker::file_name_for(&tab.read(cx).title(cx));
        let rx = crate::sql_picker::pick_save_path(&name, cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Some(path)) = rx.await else { return };
            let _ = this.update(cx, |w, cx| w.write_sql_file(&tab, &path, cx));
        })
        .detach();
    }

    pub(crate) fn write_sql_file(&mut self, tab: &Entity<QueryTab>, path: &std::path::Path, cx: &mut Context<Self>) {
        let text = tab.read(cx).editor.read(cx).text().to_string();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("query.sql").to_string();
        match std::fs::write(path, &text) {
            Ok(()) => {
                tab.update(cx, |t, cx| t.set_title(name.clone(), text, cx));
                self.notice(format!("Saved {name}"), cx);
            }
            Err(e) => self.notice(format!("cannot write {}: {e}", path.display()), cx),
        }
    }

    pub(crate) fn explain_active(&mut self, analyze: bool, cx: &mut Context<Self>) {
        if let Some(t) = self.active_tab().cloned() {
            if t.read(cx).connection().is_some_and(|c| !c.caps().explain) {
                return;
            }
            t.update(cx, |t, cx| t.explain(analyze, cx));
        }
    }

    pub(crate) fn copy_plan_text(&mut self, cx: &mut Context<Self>) {
        let Some(t) = self.active_tab().cloned() else { return };
        if !t.read(cx).shows_plan(cx) {
            return;
        }
        let task = t.update(cx, |t, cx| t.plan_text(cx));
        cx.spawn(async move |this, cx| {
            if let Some(text) = task.await {
                let _ = this.update(cx, |w, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                    w.notice("Plan copied as text", cx);
                });
            }
        })
        .detach();
    }

    pub(crate) fn toggle_plan_flame(&mut self, cx: &mut Context<Self>) {
        let Some(t) = self.active_tab().cloned() else { return };
        if !t.read(cx).shows_plan(cx) {
            return;
        }
        let plan = t.read(cx).plan.clone();
        plan.update(cx, |p, cx| {
            let m = if p.mode == crate::plan::Mode::Tree { crate::plan::Mode::Flame } else { crate::plan::Mode::Tree };
            p.set_mode(m, cx);
        });
        cx.notify();
    }

    pub(crate) fn select_result(&mut self, key: dataman_core::batch::Key, cx: &mut Context<Self>) {
        if let Some(t) = self.active_tab().cloned() {
            if t.read(cx).shown_key() != Some(key) {
                self.nav_record(None, cx);
            }
            t.update(cx, |t, cx| t.select_result(key, cx));
        }
    }

    pub(crate) fn toggle_conn_menu(&mut self, cx: &mut Context<Self>) {
        if self.conn_menu_shut.take().is_some_and(|at| at.elapsed() < Duration::from_millis(300)) {
            return;
        }
        self.conn_menu = !self.conn_menu;
        if self.conn_menu {
            let conns: Vec<Arc<Connection>> = self.store.read(cx).profiles.iter().filter_map(|p| self.store.read(cx).connection(&p.id)).collect();
            for c in conns.into_iter().filter(|c| c.schema.cached_databases().is_none()) {
                let task = c.databases();
                cx.spawn(async move |this, cx| {
                    let _ = task.await;
                    let _ = this.update(cx, |_, cx| cx.notify());
                })
                .detach();
            }
        }
        cx.notify();
    }

    pub(crate) fn pick_target(&mut self, id: &str, db: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.conn_menu = false;
        cx.notify();
        let (Some(tab), Some(conn)) = (self.active_tab().cloned(), self.store.read(cx).connection(id)) else { return };
        {
            let t = tab.read(cx);
            if t.connection().is_some_and(|c| Arc::ptr_eq(c, &conn)) && t.database() == db {
                return;
            }
        }
        self.current_conn = Some(id.to_string());
        self.retarget(&tab, conn, Some(db.to_string()), None, window, cx);
    }

    pub(crate) fn close_result(&mut self, key: dataman_core::batch::Key, cx: &mut Context<Self>) {
        if let Some(t) = self.active_tab().cloned() {
            self.result_folds_away(&t, key, cx);
            t.update(cx, |t, cx| t.close_result(key, cx));
        }
    }

    fn cycle_result(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Some(t) = self.active_tab().cloned() {
            if t.read(cx).result_tabs().len() > 1 {
                self.nav_record(None, cx);
            }
            t.update(cx, |t, cx| t.cycle_result(forward, cx));
        }
    }

    pub(crate) fn cancel_active(&mut self, cx: &mut Context<Self>) {
        self.cancel_toast_hidden = None;
        if let Some(t) = self.active_tab().cloned() {
            t.update(cx, |t, cx| t.cancel(cx));
            let grid = t.read(cx).grid.clone();
            grid.update(cx, |g, cx| {
                g.cancelling = true;
                cx.notify();
            });
        }
    }

    fn on_drag_move(&mut self, ev: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.drag else { return };
        if ev.pressed_button != Some(MouseButton::Left) {
            self.end_seam_drag();
            return;
        }
        if self.seam_from.is_none() {
            self.seam_from = Some(self.sizes_now());
        }
        match d {
            Drag::Dock(dock, col) => self.resize_column(dock, col, ev.position.x.pt()),
            Drag::Groups => self.resize_groups(ev.position.x.pt()),
            Drag::Split => {
                let top = metrics::TITLE_H + metrics::TOOLBAR_H;
                let column = self.work_column_h(window);
                let max = (column - metrics::DIVIDER - metrics::RESULT_BAR_H - metrics::GRID_MIN).max(metrics::EDITOR_MIN);
                let h = (ev.position.y.pt() - top - metrics::DIVIDER / 2.).clamp(metrics::EDITOR_MIN, max);
                self.editor_frac = Some(h / column);
            }
        }
        cx.notify();
    }

    fn end_seam_drag(&mut self) {
        let Some(d) = self.drag.take() else { return };
        let Some(from) = self.seam_from.take() else { return };
        let to = self.sizes_now();
        if from != to {
            let label = match d {
                Drag::Dock(..) => "Resize panel",
                Drag::Split => "Resize editor",
                Drag::Groups => "Resize editor groups",
            };
            self.did(label, crate::undo::Op::Sizes { from, to });
        }
    }

    pub(crate) fn drop_tab(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        self.tab_motion.landed();
        match self.tab_drop.take() {
            Some(s) => self.move_into(id, self.tab_drop_pane, Some(s), window, cx),
            None => {
                if let Some(ix) = self.tabs.iter().position(|t| t.entity_id() == id) {
                    self.activate(ix, window, cx);
                }
            }
        }
        cx.notify();
    }

    pub(crate) fn close_tabs(&mut self, close: Vec<Entity<QueryTab>>, window: &mut Window, cx: &mut Context<Self>) {
        let (kept, close): (Vec<_>, Vec<_>) = close.into_iter().partition(|t| t.read(cx).edit.read(cx).pending_count() > 0);
        if !kept.is_empty() {
            let n = kept.len();
            self.notice(format!("{n} tab{} with pending edits left open", if n == 1 { "" } else { "s" }), cx);
        }
        if close.is_empty() {
            return;
        }
        let ids: Vec<EntityId> = close.iter().map(|t| t.entity_id()).collect();
        if self.ask_about_tx(Ends::Tabs(&ids), AfterTx::CloseTabs(close.clone()), window, cx) {
            return;
        }
        self.close_tabs_now(close, window, cx);
    }

    fn close_tabs_now(&mut self, close: Vec<Entity<QueryTab>>, window: &mut Window, cx: &mut Context<Self>) {
        let first = close.first().map(|t| t.read(cx).title(cx).to_string()).unwrap_or_default();
        let closed = self.close_tabs_quietly(close, window, cx);
        let label = match closed.as_slice() {
            [] => return,
            [head, .., last] | [head @ last] if last.emptied && closed.iter().all(|c| c.pane == head.pane) => match closed.len() {
                1 => "Close editor group".to_string(),
                n => format!("Close editor group ({n} tabs)"),
            },
            [_] => format!("Close tab '{first}'"),
            _ => format!("Close {} tabs", closed.len()),
        };
        self.did(label, crate::undo::Op::Tabs { closed, reopened: vec![] });
    }

    pub(crate) fn close_tabs_quietly(&mut self, close: Vec<Entity<QueryTab>>, window: &mut Window, cx: &mut Context<Self>) -> Vec<crate::undo::ClosedTab> {
        let active = self.active_tab().cloned();
        let mut closed = Vec::new();
        for tab in &close {
            if let Some(ix) = self.tabs.iter().position(|t| t == tab) {
                closed.extend(self.close_tab_quietly(ix, window, cx));
            }
        }
        if let Some(ix) = active.and_then(|a| self.tabs.iter().position(|t| *t == a)) {
            self.activate(ix, window, cx);
        }
        closed
    }

    pub(crate) fn work_column_h(&self, window: &Window) -> f32 {
        let h = window.viewport_size().height.pt();
        h - metrics::TITLE_H - metrics::STATUS_H - metrics::TOOLBAR_H
    }

    pub(crate) fn editor_h(&self, window: &Window, below: f32) -> f32 {
        let column = self.work_column_h(window);
        let compact = window.viewport_size().height.pt() <= 800.;
        let default = if compact { metrics::EDITOR_COMPACT } else { metrics::EDITOR_H };
        let h = self.editor_frac.map(|f| f * column).unwrap_or(default);
        let max = (column - below - metrics::DIVIDER - metrics::RESULT_BAR_H - metrics::GRID_MIN).max(metrics::EDITOR_MIN);
        h.clamp(metrics::EDITOR_MIN, max)
    }

    pub fn connection_state(&self, id: &str, cx: &App) -> Option<ConnState> {
        self.store.read(cx).states.get(id).cloned()
    }

    pub(crate) fn results_shown(&self, cx: &App) -> bool {
        self.active_tab().is_some_and(|t| t.read(cx).has_run)
    }

    pub(crate) fn first_run(&self, cx: &App) -> bool {
        self.store.read(cx).profiles.is_empty()
    }
}

pub(crate) fn file_url_path(url: &str) -> Option<std::path::PathBuf> {
    let rest = match url.strip_prefix("file://") {
        Some(r) => r.strip_prefix("localhost").unwrap_or(r),
        None if url.starts_with('/') => url,
        None => return None,
    };
    let b = rest.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt as _;
    Some(std::ffi::OsString::from_vec(out).into())
}

/// A statement as an editor shows it: SQL ends with `;`; the other engines' languages have no
/// terminator (a `;` would be part of a Redis key, or break a search's JSON).
pub(crate) fn statement_text(sql: &str, engine: Option<dataman_core::Engine>) -> String {
    let body = sql.trim_end().trim_end_matches(';');
    if engine.is_some_and(|e| !e.is_sql()) {
        sql.trim_end().to_string()
    } else {
        format!("{body};")
    }
}

pub fn fmt_dur(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1e3;
    if ms < 1. {
        format!("{:.0} µs", ms * 1e3)
    } else if ms < 100. {
        format!("{ms:.1} ms")
    } else if ms < 1000. {
        format!("{ms:.0} ms")
    } else {
        format!("{:.2} s", ms / 1e3)
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _trace = TraceRender::new("workspace render");
        RENDERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // A connection change of the active tab (explorer, ⌥↩, palette, disconnect) lands
        // here before anything can act on a panel of the old connection.
        self.sync_bound_views(window, cx);
        self.sync_monitors(window, cx);
        self.sync_menu_caps(cx);
        let fp = if crate::restore::store(cx).is_some() && !self.session_off { self.session_fingerprint(cx) } else { 0 };
        if fp != self.session_fp {
            self.session_fp = fp;
            self.schedule_session_save(cx);
        }
        self.sync_group_focus(window, cx);
        let t = cx.theme();
        let first_run = self.first_run(cx);
        let pass = self.dock_pass(first_run, window, cx);
        self.split_now = if first_run { None } else { self.split_frame(window) };
        let surface = if first_run { self.welcome.clone().into_any_element() } else { self.render_work(window, cx).into_any_element() };
        // The explorer's content keeps the full width and is clipped while it slides, so the tree
        // never reflows mid-flight. Frames are requested only while it moves.
        if let Some(tw) = self.explorer_tween {
            if tw.at(self.explorer_target(), std::time::Instant::now()).is_some() {
                window.request_animation_frame();
            } else {
                self.explorer_tween = None;
            }
        }
        let production = !first_run && self.active_tab().and_then(|t| t.read(cx).connection().map(|c| c.profile.env.is_production())).unwrap_or(false);
        let docks = self.render_docks(&pass, production, window, cx);
        let left_w = pass.frame.dock(dataman_core::layout::Dock::Left).rect.w;
        let left_shown = left_w > 0.5;
        let seam = if pass.band_left() { gpui::transparent_black() } else { t.line };
        let moving = self.explorer_tween.is_some() || !self.dock_moves.is_empty();
        let work = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            // The work column's fill, painted here and only here: every region inside is transparent,
            // so no overlap darkens.
            .bg(t.panel)
            .when(left_shown, |d| d.border_l_1().border_color(seam))
            .overflow_hidden()
            .child(div().h(px(metrics::TITLE_H)).flex_none())
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .when(!production, |d| d.border_t_1().border_color(t.line))
                    .when(production, |d| d.border_t_2().border_color(t.err))
                    .child(div().flex_1().min_w_0().h_full().flex().child(surface)),
            );
        let seams = if moving { Vec::new() } else { self.render_seams(&pass.frame, cx) };
        div()
            .id("workspace")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(t.glass)
            .text_color(t.tx)
            .font_family(mono())
            .text_size(px(12.))
            .key_context("Workspace")
            .track_focus(&self.focus)
            .on_action(cx.listener(|w, _: &GoBack, window, cx| w.go(false, window, cx)))
            .on_action(cx.listener(|w, _: &GoForward, window, cx| w.go(true, window, cx)))
            .capture_any_mouse_down(cx.listener(|w, ev: &gpui::MouseDownEvent, window, cx| {
                if let MouseButton::Navigate(d) = ev.button {
                    w.go(d == gpui::NavigationDirection::Forward, window, cx);
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|w, _: &NewTab, window, cx| {
                w.new_tab(window, cx);
            }))
            .on_action(cx.listener(|w, _: &CloseTab, window, cx| {
                let a = w.active;
                w.close_tab(a, window, cx)
            }))
            .on_action(cx.listener(|w, _: &NextTab, window, cx| w.cycle_tab(true, window, cx)))
            .on_action(cx.listener(|w, _: &PrevTab, window, cx| w.cycle_tab(false, window, cx)))
            .on_action(cx.listener(|w, _: &Tab1, window, cx| w.activate_nth(Some(0), window, cx)))
            .on_action(cx.listener(|w, _: &Tab2, window, cx| w.activate_nth(Some(1), window, cx)))
            .on_action(cx.listener(|w, _: &Tab3, window, cx| w.activate_nth(Some(2), window, cx)))
            .on_action(cx.listener(|w, _: &Tab4, window, cx| w.activate_nth(Some(3), window, cx)))
            .on_action(cx.listener(|w, _: &Tab5, window, cx| w.activate_nth(Some(4), window, cx)))
            .on_action(cx.listener(|w, _: &Tab6, window, cx| w.activate_nth(Some(5), window, cx)))
            .on_action(cx.listener(|w, _: &Tab7, window, cx| w.activate_nth(Some(6), window, cx)))
            .on_action(cx.listener(|w, _: &Tab8, window, cx| w.activate_nth(Some(7), window, cx)))
            .on_action(cx.listener(|w, _: &Tab9, window, cx| w.activate_nth(None, window, cx)))
            .on_action(cx.listener(|w, _: &NewConnection, window, cx| w.open_dialog(None, window, cx)))
            .on_action(cx.listener(|w, _: &EditConnection, window, cx| {
                let id = w.current_conn.clone();
                w.open_dialog(id, window, cx)
            }))
            .on_action(cx.listener(|w, _: &FavoriteQuery, _, cx| w.favorite_current_query(cx)))
            .on_action(cx.listener(|w, _: &SaveFavorite, _, cx| w.save_favorite(cx)))
            .on_action(cx.listener(|w, _: &SaveAs, window, cx| w.save_as(window, cx)))
            .on_action(cx.listener(|w, _: &ImportFavorites, _, cx| w.import_favorites(cx)))
            .on_action(cx.listener(|w, _: &Cancel, _, cx| w.cancel_active(cx)))
            .on_action(cx.listener(|w, _: &NextResult, _, cx| w.cycle_result(true, cx)))
            .on_action(cx.listener(|w, _: &PrevResult, _, cx| w.cycle_result(false, cx)))
            .on_action(cx.listener(|w, _: &NextPage, _, cx| {
                if let Some(t) = w.active_tab().cloned() {
                    t.update(cx, |t, cx| t.table_step(true, cx));
                }
            }))
            .on_action(cx.listener(|w, _: &PrevPage, _, cx| {
                if let Some(t) = w.active_tab().cloned() {
                    t.update(cx, |t, cx| t.table_step(false, cx));
                }
            }))
            .on_action(cx.listener(|w, _: &Explain, _, cx| w.explain_active(false, cx)))
            .on_action(cx.listener(|w, _: &ExplainAnalyze, _, cx| w.explain_active(true, cx)))
            .on_action(cx.listener(|w, _: &CopyPlanText, _, cx| w.copy_plan_text(cx)))
            .on_action(cx.listener(|w, _: &TogglePlanFlame, _, cx| w.toggle_plan_flame(cx)))
            .on_action(cx.listener(|w, _: &ToggleMcpLog, window, cx| w.toggle_mcp_log(window, cx)))
            .on_action(cx.listener(|w, _: &ToggleResultZoom, window, cx| w.toggle_result_zoom(window, cx)))
            .on_action(cx.listener(|w, _: &RunZoomed, window, cx| {
                if w.active_tab().is_some_and(|t| t.read(cx).zoomed) {
                    w.run_active(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|_, _: &CheckForUpdates, _, cx| crate::updates::check(true, cx)))
            .on_action(cx.listener(|w, _: &ShowUpdate, window, cx| w.open_update_sheet(window, cx)))
            .on_action(cx.listener(|w, _: &InstallUpdate, window, cx| w.update_primary(window, cx)))
            .on_action(cx.listener(|w, _: &OpenActivity, window, cx| {
                let id = w.active_tab().and_then(|t| t.read(cx).connection().map(|c| c.profile.id.clone())).or(w.current_conn.clone());
                match id {
                    Some(id) => w.open_activity(&id, window, cx),
                    None => w.notice("Connect first", cx),
                }
            }))
            .on_action(cx.listener(|w, _: &Refresh, _, cx| w.tree.update(cx, |t, cx| t.refresh_selected(cx))))
            .on_action(cx.listener(|w, _: &ToggleExplorer, window, cx| {
                let inside = w.focused_region(window, cx) == Some(Region::Explorer);
                w.set_explorer_open(!w.explorer_open, cx);
                if !w.explorer_open && inside {
                    w.focus_editor(window, cx);
                }
            }))
            .on_action(cx.listener(|w, _: &ToggleSidePanel, _, cx| {
                if w.side.is_some() {
                    w.last_side = w.side.take();
                } else if w.last_side.is_some() {
                    w.side = w.last_side.clone();
                } else {
                    w.notice("Nothing to show in the side panel", cx);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|w, _: &FocusExplorer, window, cx| w.focus_region(Region::Explorer, window, cx)))
            .on_action(cx.listener(|w, _: &FocusGrid, window, cx| w.focus_region(Region::Grid, window, cx)))
            .on_action(cx.listener(|w, _: &FocusEditor, window, cx| {
                if w.shortcuts_open {
                    w.shortcuts_open = false;
                    cx.notify();
                    return;
                }
                if w.copy_menu {
                    w.copy_menu = false;
                }
                if w.conn_menu {
                    w.conn_menu = false;
                    cx.notify();
                    return;
                }
                if w.focused_region(window, cx) == Some(Region::Grid)
                    && let Some(e) = w.active_edit(cx)
                    && e.update(cx, |e, cx| e.dismiss_card(cx))
                {
                    return;
                }
                if w.focused_region(window, cx) == Some(Region::Grid) && w.active_tab().is_some_and(|t| t.read(cx).zoomed) {
                    return w.toggle_result_zoom(window, cx);
                }
                w.focus_region(Region::Editor, window, cx)
            }))
            .on_action(cx.listener(|w, _: &CycleFocus, window, cx| w.cycle_focus(true, window, cx)))
            .on_action(cx.listener(|w, _: &CycleFocusBack, window, cx| w.cycle_focus(false, window, cx)))
            .on_action(cx.listener(|w, _: &JumpToError, window, cx| w.jump_to_error(window, cx)))
            .on_action(cx.listener(|w, _: &crate::find::Find, window, cx| w.find_in_editor(false, window, cx)))
            .on_action(cx.listener(|w, _: &crate::find::FindReplace, window, cx| w.find_in_editor(true, window, cx)))
            .on_action(cx.listener(|w, _: &crate::find::FindNext, window, cx| w.find_step_in_editor(true, window, cx)))
            .on_action(cx.listener(|w, _: &crate::find::FindPrevious, window, cx| w.find_step_in_editor(false, window, cx)))
            .on_action(cx.listener(|w, _: &UseExplorerConnection, window, cx| w.use_explorer_connection(window, cx)))
            .on_action(cx.listener(|w, _: &ShowShortcuts, _, cx| {
                w.shortcuts_open = !w.shortcuts_open;
                cx.notify();
            }))
            .on_action(cx.listener(|w, _: &CommandPalette, window, cx| w.open_palette(crate::palette::Scope::All, window, cx)))
            .on_action(cx.listener(|w, _: &GoTo, window, cx| w.open_palette(crate::palette::Scope::Goto, window, cx)))
            .on_action(cx.listener(|w, _: &OpenSettings, window, cx| w.toggle_settings(window, cx)))
            .on_action(cx.listener(|w, _: &ExportResult, window, cx| w.open_export(window, cx)))
            .on_action(cx.listener(|w, _: &ImportData, window, cx| w.import_data(None, None, window, cx)))
            .on_drop(cx.listener(|w, paths: &gpui::ExternalPaths, window, cx| w.drop_files(paths, window, cx)))
            .on_action(cx.listener(|w, _: &ReviewChanges, window, cx| {
                // On production the edits are writes like any other: always reviewed first.
                let production = w.active_tab().and_then(|t| t.read(cx).connection().map(|c| c.profile.env.is_production())).unwrap_or(false);
                let confirm = w.store.read(cx).core.settings.get().confirm_commit || production;
                // Never commit edits that are not on screen (a live grid hides the result they
                // were made on).
                if let Some(tab) = w.active_tab()
                    && crate::lar::hides_result(tab.read(cx))
                {
                    if tab.read(cx).edit.read(cx).pending_count() > 0 {
                        w.notice("The pending edits belong to a result that is not shown: nothing was committed", cx);
                    }
                    return;
                }
                if let Some(e) = w.active_edit(cx) {
                    e.update(cx, |e, cx| e.review(confirm, window, cx));
                }
            }))
            .on_action(cx.listener(|w, _: &DiscardChanges, window, cx| {
                if let Some(e) = w.active_edit(cx) {
                    e.update(cx, |e, cx| e.discard(window, cx));
                }
            }))
            .on_action(cx.listener(|w, a: &crate::edit_ui::OpenSqlInTab, window, cx| {
                let sql = a.0.clone();
                let conn = w.active_tab().and_then(|t| t.read(cx).connection().cloned());
                let tab = w.new_tab(window, cx);
                tab.update(cx, |t, cx| {
                    t.set_connection(conn, cx);
                    t.editor.update(cx, |e, cx| e.set_text(&sql, cx));
                });
            }))
            .on_action(cx.listener(|w, _: &OpenSqlFile, window, cx| w.open_sql_file(window, cx)))
            .on_action(cx.listener(|w, _: &crate::tree::ShowRowActions, window, cx| {
                let was_open = w.explorer_open;
                w.focus_region(Region::Explorer, window, cx);
                let tree = w.tree.clone();
                if was_open {
                    tree.update(cx, |t, cx| t.show_row_actions(None, window, cx));
                } else {
                    // The explorer lays out first, so the menu finds its row.
                    window.on_next_frame(move |window, cx| tree.update(cx, |t, cx| t.show_row_actions(None, window, cx)));
                }
            }))
            .on_action(cx.listener(|w, _: &crate::row_actions::DuplicateConnection, window, cx| {
                w.row_actions_on_current(crate::tree_menu::Act::DuplicateConnection, window, cx)
            }))
            .on_action(cx.listener(|w, _: &crate::row_actions::DeleteConnection, window, cx| {
                w.row_actions_on_current(crate::tree_menu::Act::DeleteConnection, window, cx)
            }))
            .on_action(cx.listener(|w, _: &crate::row_actions::CopyConnectionString, _, cx| w.copy_connection_string(cx)))
            .on_action(cx.listener(|_, _: &ToggleTheme, _, cx| theme::set_pref(theme::next_pref(theme::pref(cx)), cx)))
            .on_action(cx.listener(|_, _: &ThemeDark, _, cx| theme::set_pref(dataman_core::ThemePref::Dark, cx)))
            .on_action(cx.listener(|_, _: &ThemeLight, _, cx| theme::set_pref(dataman_core::ThemePref::Light, cx)))
            .on_action(cx.listener(|_, _: &ThemeSystem, _, cx| theme::set_pref(dataman_core::ThemePref::System, cx)))
            .on_action(cx.listener(|_, _: &TranslucencyOn, w, cx| theme::set_translucency(Translucency::On, w, cx)))
            .on_action(cx.listener(|_, _: &TranslucencyOff, w, cx| theme::set_translucency(Translucency::Off, w, cx)))
            .on_action(cx.listener(|_, _: &FontGeistMono, _, cx| theme::set_font(dataman_core::UiFont::GeistMono, cx)))
            .on_action(cx.listener(|_, _: &FontLilex, _, cx| theme::set_font(dataman_core::UiFont::Lilex, cx)))
            .on_action(cx.listener(|_, _: &FontZedMono, _, cx| theme::set_font(dataman_core::UiFont::ZedMono, cx)))
            .on_action(cx.listener(|_, _: &FontJetBrainsMono, _, cx| theme::set_font(dataman_core::UiFont::JetBrainsMono, cx)))
            .on_action(cx.listener(|_, _: &ZoomIn, w, cx| theme::set_zoom(theme::zoom_step(theme::zoom(), 1), w, cx)))
            .on_action(cx.listener(|_, _: &ZoomOut, w, cx| theme::set_zoom(theme::zoom_step(theme::zoom(), -1), w, cx)))
            .on_action(cx.listener(|_, _: &ZoomReset, w, cx| theme::set_zoom(1.0, w, cx)))
            .on_action(cx.listener(|w, _: &LarPanel1, window, cx| w.toggle_server_panel(crate::panels::LAR_BUTTONS[0].0, window, cx)))
            .on_action(cx.listener(|w, _: &Quit, window, cx| {
                if w.request_quit(window, cx) {
                    w.quit_now(cx);
                }
            }))
            .on_action(cx.listener(|w, _: &CommitTx, _, cx| w.end_active_tx(true, cx)))
            .on_action(cx.listener(|w, _: &RollbackTx, _, cx| w.end_active_tx(false, cx)))
            .on_action(cx.listener(|w, _: &SheetConfirm, window, cx| w.sheet_answer(crate::tab::ConfirmChoice::Run, window, cx)))
            .on_action(cx.listener(|w, _: &SheetRollback, window, cx| w.sheet_answer(crate::tab::ConfirmChoice::Rollback, window, cx)))
            .on_action(cx.listener(|w, _: &SheetCancel, window, cx| w.sheet_answer(crate::tab::ConfirmChoice::Cancel, window, cx)))
            .on_action(cx.listener(|w, _: &NewConnectionFromClipboard, window, cx| w.new_connection_from_clipboard(window, cx)))
            .on_action(cx.listener(|w, _: &ImportConnections, window, cx| w.open_import(window, cx)))
            .on_action(cx.listener(|w, _: &CancelPendingRun, window, cx| w.resolve_pending_run(false, window, cx)))
            .on_action(cx.listener(|w, _: &DiscardAndRun, window, cx| w.resolve_pending_run(true, window, cx)))
            .map(|d| crate::lar::workspace_actions(d, cx))
            .map(|d| crate::dock::workspace_actions(d, cx))
            .map(|d| crate::sidebar::workspace_actions(d, cx))
            .map(|d| crate::split::workspace_actions(d, cx))
            .map(|d| crate::undo::workspace_actions(d, cx))
            .on_mouse_move(cx.listener(Self::on_drag_move))
            .on_mouse_up(MouseButton::Left, cx.listener(|w, _, _, _| w.end_seam_drag()))
            .capture_any_mouse_down(cx.listener(|w, ev: &gpui::MouseDownEvent, _, _| {
                w.focus_visible = false;
                w.alt_press = ev.modifiers.alt;
            }))
            .capture_key_down(cx.listener(|w, _, _, _| w.alt_press = false))
            .child(div().flex_1().min_h_0().relative().flex().flex_row().children(docks.left).child(work).children(docks.right).children(seams))
            .children(self.drag.map(|d| {
                let rows = matches!(d, Drag::Split);
                let style = if rows { gpui::CursorStyle::ResizeRow } else { gpui::CursorStyle::ResizeColumn };
                gpui::canvas(|_, _, _| {}, move |_, _, window, _| window.set_window_cursor_style(style)).absolute().size_0()
            }))
            .child(self.render_status(window, cx))
            .child(self.render_titlebar(window, cx))
            .when(t.is_dark(), |d| d.child(div().absolute().top_0().left_0().right_0().h(px(1.)).bg(gpui::hsla(0., 0., 1., 0.08))))
            .children(self.render_drop_preview(window, cx))
            .children(self.render_saved_hint(window, cx))
            .children(self.render_panel_menu(cx))
            .children(self.render_toasts(window, cx))
            .children(self.render_crash_toast(cx))
            .children(self.dialog.clone())
            .children(self.import.clone())
            .children(self.shortcuts_open.then(|| self.render_shortcuts(cx)))
            .children(self.settings_view.clone())
            .children(self.export.clone().filter(|e| e.read(cx).visible))
            .children(self.data_import.clone().filter(|e| e.read(cx).visible))
            .children(self.value_view.clone())
            .children(self.palette.clone())
            .children(self.render_filter_popover(cx))
            .children(self.render_confirm_discard(cx))
            .children(self.render_confirm_delete(cx))
            .children(self.render_tx_sheet(cx))
            .children(self.render_param_sheet(cx))
            .children(self.render_mcp_sheet(cx))
            .children(self.render_update_sheet(cx))
    }
}

pub(crate) struct TraceRender(&'static str, Instant);

impl TraceRender {
    pub(crate) fn new(what: &'static str) -> Option<TraceRender> {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        ON.get_or_init(|| std::env::var_os("DATAMAN_TRACE_FRAMES").is_some()).then(|| TraceRender(what, Instant::now()))
    }
}

impl Drop for TraceRender {
    fn drop(&mut self) {
        let ms = self.1.elapsed().as_secs_f64() * 1e3;
        if ms > 1. {
            eprintln!("trace: {} {ms:.2} ms", self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyContext, Keymap, Keystroke};

    #[test]
    fn file_urls() {
        assert_eq!(file_url_path("file:///Users/b/a%20b.sql"), Some("/Users/b/a b.sql".into()));
        assert_eq!(file_url_path("file://localhost/tmp/x.sql"), Some("/tmp/x.sql".into()));
        assert_eq!(file_url_path("file:///tmp/caf%C3%A9.sql"), Some("/tmp/café.sql".into()));
        assert_eq!(file_url_path("file:///tmp/100%"), Some("/tmp/100%".into()));
        assert_eq!(file_url_path("/tmp/plain.sql"), Some("/tmp/plain.sql".into()));
        assert_eq!(file_url_path("https://example.com/x.sql"), None);
    }

    fn stack(s: &[&str]) -> Vec<KeyContext> {
        s.iter().map(|c| KeyContext::parse(c).unwrap()).collect()
    }

    #[test]
    fn fields_and_modals_keep_their_keys() {
        let keymap = Keymap::new(vec![
            KeyBinding::new("backspace", crate::grid::DeleteRows, Some(crate::grid::GRID_KEYS)),
            KeyBinding::new("/", crate::tree::FocusFilter, Some("Explorer && !TextInput")),
            KeyBinding::new("cmd-w", CloseTab, Some(WORKSPACE_KEYS)),
            KeyBinding::new("cmd-i", crate::grid::InsertRow, Some("ResultGrid")),
        ]);
        let hits = |k: &str, s: &[&str]| !keymap.bindings_for_input(&[Keystroke::parse(k).unwrap()], &stack(s)).0.is_empty();
        assert!(hits("backspace", &["Workspace", "ResultGrid"]), "on the grid ⌫ marks rows for deletion");
        assert!(!hits("backspace", &["Workspace", "ResultGrid", "GridCellEditor", "TextInput"]), "in the cell editor ⌫ deletes a character");
        assert!(hits("/", &["Workspace", "Explorer"]));
        assert!(!hits("/", &["Workspace", "Explorer", "TextInput"]), "in the explorer filter `/` types");
        assert!(hits("cmd-w", &["Workspace", "Editor"]));
        assert!(!hits("cmd-w", &["Workspace", "Palette Modal", "TextInput"]), "⌘W does nothing behind the palette");
        assert!(!hits("cmd-w", &["Workspace", "ConfirmDiscard Modal"]));
        assert!(hits("cmd-i", &["Workspace", "ResultGrid", "GridCellEditor", "TextInput"]), "⌘I still adds a row from the cell editor");
    }

    #[test]
    fn result_tab_keys() {
        let w = Some(WORKSPACE_KEYS);
        let keymap = Keymap::new(vec![
            KeyBinding::new("cmd-shift-]", NextTab, w),
            KeyBinding::new("cmd-shift-[", PrevTab, w),
            KeyBinding::new("cmd-alt-]", NextResult, w),
            KeyBinding::new("cmd-alt-[", PrevResult, w),
        ]);
        let first = |k: &str, s: &[&str]| {
            keymap.bindings_for_input(&[Keystroke::parse(k).unwrap()], &stack(s)).0.first().map(|b| b.action().name().to_string())
        };
        for ctx in [&["Workspace", "CodeEditor"][..], &["Workspace", "ResultGrid"], &["Workspace", "ResultGrid", "GridCellEditor", "TextInput"]] {
            assert_eq!(first("cmd-alt-]", ctx).as_deref(), Some("workspace::NextResult"), "{ctx:?}");
            assert_eq!(first("cmd-alt-[", ctx).as_deref(), Some("workspace::PrevResult"), "{ctx:?}");
            assert_eq!(first("cmd-shift-]", ctx).as_deref(), Some("workspace::NextTab"), "{ctx:?}");
        }
        assert_eq!(first("cmd-alt-]", &["Workspace", "ConfirmDiscard Modal"]), None);
    }

    #[test]
    fn explain_keys() {
        let w = Some(WORKSPACE_KEYS);
        let keymap = Keymap::new(vec![
            KeyBinding::new("cmd-l", Explain, w),
            KeyBinding::new("cmd-alt-l", ExplainAnalyze, w),
            KeyBinding::new("cmd-l", crate::welcome::Localhost, Some("Welcome")),
            KeyBinding::new("up", crate::grid::MoveUp, Some(crate::grid::GRID_KEYS)),
            KeyBinding::new("up", crate::plan::SelectPrev, Some(crate::plan::CONTEXT)),
            KeyBinding::new("escape", FocusEditor, w),
        ]);
        let first = |k: &str, s: &[&str]| {
            keymap.bindings_for_input(&[Keystroke::parse(k).unwrap()], &stack(s)).0.first().map(|b| b.action().name().to_string())
        };
        for ctx in [&["Workspace", "CodeEditor"][..], &["Workspace", "ResultGrid"], &["Workspace", "PlanView"]] {
            assert_eq!(first("cmd-l", ctx).as_deref(), Some("workspace::Explain"), "{ctx:?}");
            assert_eq!(first("cmd-alt-l", ctx).as_deref(), Some("workspace::ExplainAnalyze"), "{ctx:?}");
        }
        assert_eq!(first("cmd-l", &["Workspace", "Welcome"]).as_deref(), Some("welcome::Localhost"));
        assert_eq!(first("cmd-l", &["Workspace", "TxSheet Modal"]), None);
        assert_eq!(first("up", &["Workspace", "PlanView"]).as_deref(), Some("plan::SelectPrev"));
        assert_eq!(first("escape", &["Workspace", "PlanView"]).as_deref(), Some("workspace::FocusEditor"));
    }

    #[test]
    fn table_keys() {
        let w = Some(WORKSPACE_KEYS);
        let keymap = Keymap::new(vec![
            KeyBinding::new("cmd-alt-right", NextPage, w),
            KeyBinding::new("cmd-alt-left", PrevPage, w),
            KeyBinding::new("alt-right", crate::editor::MoveWordRight, Some("CodeEditor")),
            KeyBinding::new("shift-space", crate::grid::ViewValue, Some(crate::grid::GRID_KEYS)),
            KeyBinding::new("down", crate::grid::MoveDown, Some(crate::grid::GRID_KEYS)),
            KeyBinding::new("escape", FocusEditor, w),
            KeyBinding::new("escape", crate::value_edit::CancelValue, Some(crate::value_edit::CONTEXT)),
            KeyBinding::new("escape", crate::table_view::CloseFilter, Some(crate::table_view::POPOVER_CONTEXT)),
        ]);
        let first = |k: &str, s: &[&str]| {
            keymap.bindings_for_input(&[Keystroke::parse(k).unwrap()], &stack(s)).0.first().map(|b| b.action().name().to_string())
        };
        for ctx in [&["Workspace", "CodeEditor"][..], &["Workspace", "ResultGrid"]] {
            assert_eq!(first("cmd-alt-right", ctx).as_deref(), Some("workspace::NextPage"), "{ctx:?}");
            assert_eq!(first("cmd-alt-left", ctx).as_deref(), Some("workspace::PrevPage"), "{ctx:?}");
        }
        assert_eq!(first("alt-right", &["Workspace", "CodeEditor"]).as_deref(), Some("code_editor::MoveWordRight"));
        assert_eq!(first("shift-space", &["Workspace", "ResultGrid"]).as_deref(), Some("grid::ViewValue"));
        assert_eq!(first("cmd-alt-right", &["Workspace", "Palette Modal"]), None);
        assert_eq!(first("down", &["Workspace", "ResultGrid"]).as_deref(), Some("grid::MoveDown"));
        assert_eq!(first("escape", &["Workspace", "ResultGrid", crate::value_edit::CONTEXT]).as_deref(), Some("value_editor::CancelValue"));
        assert_eq!(first("escape", &["Workspace", crate::table_view::POPOVER_CONTEXT, "TextInput"]).as_deref(), Some("table_filter::CloseFilter"));
    }

    #[test]
    fn transaction_keys() {
        let w = Some(WORKSPACE_KEYS);
        let keymap = Keymap::new(vec![
            KeyBinding::new("cmd-enter", crate::editor::RunScript, Some("CodeEditor")),
            KeyBinding::new("cmd-backspace", DiscardChanges, Some(crate::grid::GRID_KEYS)),
            KeyBinding::new("escape", FocusEditor, w),
            KeyBinding::new("ctrl-cmd-enter", CommitTx, w),
            KeyBinding::new("ctrl-cmd-backspace", RollbackTx, w),
            KeyBinding::new("cmd-enter", SheetConfirm, Some(SHEET_CONTEXT)),
            KeyBinding::new("cmd-backspace", SheetRollback, Some(SHEET_CONTEXT)),
            KeyBinding::new("escape", SheetCancel, Some(SHEET_CONTEXT)),
        ]);
        let first = |k: &str, s: &[&str]| {
            keymap.bindings_for_input(&[Keystroke::parse(k).unwrap()], &stack(s)).0.first().map(|b| b.action().name().to_string())
        };
        for ctx in [&["Workspace", "CodeEditor"][..], &["Workspace", "ResultGrid"], &["Workspace", "ResultGrid", "GridCellEditor", "TextInput"]] {
            assert_eq!(first("ctrl-cmd-enter", ctx).as_deref(), Some("workspace::CommitTx"), "{ctx:?}");
            assert_eq!(first("ctrl-cmd-backspace", ctx).as_deref(), Some("workspace::RollbackTx"), "{ctx:?}");
        }
        assert_eq!(first("cmd-enter", &["Workspace", "CodeEditor"]).as_deref(), Some("code_editor::RunScript"));
        assert_eq!(first("cmd-backspace", &["Workspace", "ResultGrid"]).as_deref(), Some("workspace::DiscardChanges"));
        let sheet = ["Workspace", "TxSheet Modal"];
        assert_eq!(first("cmd-enter", &sheet).as_deref(), Some("workspace::SheetConfirm"));
        assert_eq!(first("cmd-backspace", &sheet).as_deref(), Some("workspace::SheetRollback"));
        assert_eq!(first("escape", &sheet).as_deref(), Some("workspace::SheetCancel"));
        assert_eq!(first("ctrl-cmd-enter", &sheet), None, "no commit behind a sheet");
    }

    #[test]
    fn close_prompt_asks_about_open_transactions() {
        let tabs = [(1, Some("prod"), true), (2, Some("prod"), false), (3, Some("dev"), true), (4, None, false)];
        assert_eq!(tabs_holding(&tabs, Ends::Tab(1)), [1]);
        assert!(tabs_holding(&tabs, Ends::Tab(2)).is_empty(), "no transaction: closes at once");
        assert!(tabs_holding(&tabs, Ends::Tab(4)).is_empty());
        assert_eq!(tabs_holding(&tabs, Ends::Tabs(&[2, 3, 4])), [3]);
        assert!(tabs_holding(&tabs, Ends::Tabs(&[2, 4])).is_empty());
        assert_eq!(tabs_holding(&tabs, Ends::Conn("prod")), [1]);
        assert_eq!(tabs_holding(&tabs, Ends::Conn("dev")), [3]);
        assert!(tabs_holding(&tabs, Ends::Conn("staging")).is_empty());
        assert_eq!(tabs_holding(&tabs, Ends::All), [1, 3], "a quit asks once, listing them all");
        assert!(tabs_holding(&[(1, Some("a"), false)], Ends::All).is_empty());
    }

    #[test]
    fn switch_save_and_delete_ask_about_open_transactions() {
        let tabs = [(1, Some("prod"), true), (2, Some("prod"), false), (3, Some("dev"), true), (4, Some("prod"), true)];
        let asked = |c: Change<'_, i32>| ends_of(c).map(|e| tabs_holding(&tabs, e)).unwrap_or_default();
        assert_eq!(asked(Change::Retarget { tab: 1, same_conn: true, from_db: "app", to_db: "audit" }), [1]);
        assert_eq!(asked(Change::Retarget { tab: 1, same_conn: false, from_db: "app", to_db: "app" }), [1]);
        assert!(asked(Change::Retarget { tab: 1, same_conn: true, from_db: "app", to_db: "app" }).is_empty());
        assert!(asked(Change::Retarget { tab: 2, same_conn: true, from_db: "app", to_db: "audit" }).is_empty(), "none open");
        assert_eq!(asked(Change::Edit { id: "prod", is_new: false }), [1, 4]);
        assert!(asked(Change::Edit { id: "prod", is_new: true }).is_empty());
        assert_eq!(asked(Change::Edit { id: "dev", is_new: false }), [3]);
        assert_eq!(asked(Change::Delete("prod")), [1, 4]);
        assert!(asked(Change::Delete("staging")).is_empty());
        assert_eq!(asked(Change::Disconnect("dev")), [3]);
        assert_eq!(asked(Change::CloseTab(4)), [4]);
        assert_eq!(asked(Change::Quit), [1, 3, 4]);
        assert_eq!(AfterTx::DeleteConnection("prod".into()).verb(), "delete");
        assert_eq!(AfterTx::Disconnect("prod".into()).verb(), "disconnect");
    }

    #[test]
    fn transaction_age_is_coarse() {
        use crate::chrome::fmt_tx_age;
        let s = Duration::from_secs;
        assert_eq!(fmt_tx_age(s(0)), "<1m");
        assert_eq!(fmt_tx_age(s(59)), "<1m");
        assert_eq!(fmt_tx_age(s(60)), "1m");
        assert_eq!(fmt_tx_age(s(59 * 60 + 59)), "59m");
        assert_eq!(fmt_tx_age(s(3600 + 5 * 60)), "1h 05m");
    }

    #[test]
    fn save_keys() {
        let w = Some(WORKSPACE_KEYS);
        let keymap = Keymap::new(vec![
            KeyBinding::new("cmd-s", SaveFavorite, Some("CodeEditor")),
            KeyBinding::new("cmd-s", ReviewChanges, Some("ResultGrid")),
            KeyBinding::new("cmd-shift-s", SaveAs, w),
        ]);
        let first = |k: &str, s: &[&str]| {
            keymap.bindings_for_input(&[Keystroke::parse(k).unwrap()], &stack(s)).0.first().map(|b| b.action().name().to_string())
        };
        assert_eq!(first("cmd-s", &["Workspace", "CodeEditor"]).as_deref(), Some("workspace::SaveFavorite"));
        assert_eq!(first("cmd-s", &["Workspace", "ResultGrid"]).as_deref(), Some("workspace::ReviewChanges"));
        assert_eq!(first("cmd-s", &["Workspace", "ResultGrid", "GridCellEditor", "TextInput"]).as_deref(), Some("workspace::ReviewChanges"));
        for ctx in [&["Workspace", "CodeEditor"][..], &["Workspace", "ResultGrid"]] {
            assert_eq!(first("cmd-shift-s", ctx).as_deref(), Some("workspace::SaveAs"), "{ctx:?}");
        }
        assert_eq!(first("cmd-shift-s", &["Workspace", "Palette Modal", "TextInput"]), None);
        let save = crate::keymap::find("save_favorite").unwrap();
        let review = crate::keymap::find("review_changes").unwrap();
        assert_eq!((save.keys, review.keys), (&["cmd-s"][..], &["cmd-s"][..]));
        assert!(!save.scope.overlaps(review.scope), "the editor and the grid never meet");
    }

    #[test]
    fn the_open_menu_takes_the_keys() {
        use crate::tree;
        let keymap = Keymap::new(vec![
            KeyBinding::new("up", tree::SelectPrev, Some("Explorer")),
            KeyBinding::new("escape", tree::Escape, Some("Explorer")),
            KeyBinding::new("enter", tree::Open, Some("Explorer")),
            KeyBinding::new("shift-f10", tree::ShowRowActions, Some("Explorer")),
            KeyBinding::new("escape", FocusEditor, Some(WORKSPACE_KEYS)),
            KeyBinding::new("up", tree::MenuUp, Some("ExplorerMenu")),
            KeyBinding::new("escape", tree::MenuClose, Some("ExplorerMenu")),
            KeyBinding::new("enter", tree::MenuConfirm, Some("ExplorerMenu")),
        ]);
        let first = |k: &str, s: &[&str]| {
            let (b, _) = keymap.bindings_for_input(&[Keystroke::parse(k).unwrap()], &stack(s));
            b.first().map(|b| b.action().name().to_string())
        };
        let menu = ["Workspace", "Explorer", "ExplorerMenu"];
        assert_eq!(first("up", &menu).as_deref(), Some("explorer::MenuUp"));
        assert_eq!(first("escape", &menu).as_deref(), Some("explorer::MenuClose"));
        assert_eq!(first("enter", &menu).as_deref(), Some("explorer::MenuConfirm"));
        assert_eq!(first("shift-f10", &["Workspace", "Explorer"]).as_deref(), Some("explorer::ShowRowActions"));
        assert_eq!(first("escape", &["Workspace", "Explorer"]).as_deref(), Some("explorer::Escape"));
    }

    #[test]
    fn find_keys_resolve_by_context() {
        use crate::find;
        let keymap = Keymap::new(vec![
            KeyBinding::new("cmd-shift-g", FocusGrid, Some(WORKSPACE_KEYS)),
            KeyBinding::new("escape", FocusEditor, Some(WORKSPACE_KEYS)),
            KeyBinding::new("cmd-f", find::Find, Some("CodeEditor")),
            KeyBinding::new("cmd-shift-g", find::FindPrevious, Some("CodeEditor && find")),
            KeyBinding::new("cmd-f", find::Find, Some(crate::grid::GRID_KEYS)),
            KeyBinding::new("cmd-shift-g", find::FindPrevious, Some("ResultGrid && find && !GridCellEditor")),
            KeyBinding::new("escape", find::CloseFind, Some("ResultGrid && find && !GridCellEditor")),
            KeyBinding::new("escape", find::CloseFind, Some(find::CONTEXT)),
            KeyBinding::new("shift-enter", find::FindPrevious, Some(find::CONTEXT)),
            KeyBinding::new("enter", crate::input::Submit, Some("TextInput")),
        ]);
        let first = |k: &str, s: &[&str]| {
            let (b, _) = keymap.bindings_for_input(&[Keystroke::parse(k).unwrap()], &stack(s));
            b.first().map(|b| b.action().name().to_string())
        };
        assert_eq!(first("cmd-shift-g", &["Workspace", "CodeEditor"]).as_deref(), Some("workspace::FocusGrid"));
        assert_eq!(first("cmd-shift-g", &["Workspace", "CodeEditor find"]).as_deref(), Some("find::FindPrevious"));
        assert_eq!(first("cmd-f", &["Workspace", "ResultGrid"]).as_deref(), Some("find::Find"));
        assert_eq!(first("escape", &["Workspace", "ResultGrid"]).as_deref(), Some("workspace::FocusEditor"));
        assert_eq!(first("escape", &["Workspace", "ResultGrid find"]).as_deref(), Some("find::CloseFind"));
        assert_eq!(first("cmd-shift-g", &["Workspace", "ResultGrid find"]).as_deref(), Some("find::FindPrevious"));
        let field = ["Workspace", find::CONTEXT, "TextInput"];
        assert_eq!(first("escape", &field).as_deref(), Some("find::CloseFind"));
        assert_eq!(first("enter", &field).as_deref(), Some("text_input::Submit"));
        assert_eq!(first("shift-enter", &field).as_deref(), Some("find::FindPrevious"));
    }

    #[test]
    fn the_cell_menu_takes_the_keys() {
        use crate::grid::{self, GRID_KEYS, MENU_CONTEXT};
        let keymap = Keymap::new(vec![
            KeyBinding::new("up", grid::MoveUp, Some(GRID_KEYS)),
            KeyBinding::new("left", grid::MoveLeft, Some(GRID_KEYS)),
            KeyBinding::new("enter", grid::EditCell, Some(GRID_KEYS)),
            KeyBinding::new("cmd-c", grid::CopyTsv, Some(GRID_KEYS)),
            KeyBinding::new("alt-down", grid::OpenCellMenu, Some(GRID_KEYS)),
            KeyBinding::new("escape", FocusEditor, Some(WORKSPACE_KEYS)),
            KeyBinding::new("up", grid::MenuUp, Some(MENU_CONTEXT)),
            KeyBinding::new("enter", grid::MenuConfirm, Some(MENU_CONTEXT)),
            KeyBinding::new("escape", grid::MenuClose, Some(MENU_CONTEXT)),
            KeyBinding::new("alt-down", grid::MenuClose, Some(MENU_CONTEXT)),
        ]);
        let first = |k: &str, s: &[&str]| {
            let (b, _) = keymap.bindings_for_input(&[Keystroke::parse(k).unwrap()], &stack(s));
            b.first().map(|b| b.action().name().to_string())
        };
        let menu = ["Workspace", "ResultGrid", MENU_CONTEXT];
        assert_eq!(first("up", &menu).as_deref(), Some("grid::MenuUp"));
        assert_eq!(first("enter", &menu).as_deref(), Some("grid::MenuConfirm"));
        assert_eq!(first("escape", &menu).as_deref(), Some("grid::MenuClose"));
        assert_eq!(first("alt-down", &menu).as_deref(), Some("grid::MenuClose"));
        assert_eq!(first("left", &menu).as_deref(), Some("grid::MoveLeft"));
        assert_eq!(first("cmd-c", &menu).as_deref(), Some("grid::CopyTsv"));
        assert_eq!(first("alt-down", &["Workspace", "ResultGrid"]).as_deref(), Some("grid::OpenCellMenu"));
        assert_eq!(first("enter", &["Workspace", "ResultGrid"]).as_deref(), Some("grid::EditCell"));
    }
}
