//! LAR's panels (board page "LAR panels"): they light up only when `version()` says LAR and
//! vanish on plain PostgreSQL (the registry filters them by the server's extensions).
//!
//! - `live`: SUBSCRIBE / LISTEN … AS run from the editor on their own connection; the grid
//!   becomes live (fading highlights, scroll anchored) and the panel lists the streams.
//!
//! The other LAR panels of the boards (time ruler and past mode, A ⇄ B, row history, graph,
//! search, engine health) were removed for now; `PanelRegistry` is where they would plug back
//! in. The shell calls the small hooks below (`start_live`, `grid_override`, `result_bar`,
//! `tab_indicator`, `status_text`, `workspace_actions`); the state of a tab lives in
//! [`TabLar`] inside the tab.

pub mod demo;
pub mod grid;
pub mod live;
pub mod widgets;

use std::sync::Arc;

use gpui::{
    Action, AnyElement, AnyView, App, AppContext, Context, Entity, Hsla, IntoElement, SharedString, Stateful, WeakEntity, Window,
    actions, prelude::*,
};
use diskman_core::Connection;

use crate::app::Workspace;
use crate::panels::ServerPanel;
use crate::result_bar::ResultState;
use crate::tab::QueryTab;
use crate::theme::Theme;

actions!(
    lar,
    [
        /// ⌥P: pause or resume the tab's live query.
        PauseLive,
        /// Stop the tab's live query.
        StopLive,
        /// Start the tab's live query again after it ended.
        Resubscribe,
        /// Keep the ended stream's rows as a plain snapshot.
        KeepSnapshot,
    ]
);

pub fn bind_keys(cx: &mut App) {
    // ⌥P (pause / resume) is a remappable command (`keymap`).
    grid::bind_keys(cx);
}

/// LAR state of one query tab.
#[derive(Default)]
pub struct TabLar {
    /// The live query shown instead of the grid.
    pub live: Option<LiveView>,
    /// The last query that reads a table: "Insert SUBSCRIBE" in the live panel starts from it.
    pub last_query: Option<String>,
}

pub struct LiveView {
    pub run: Entity<live::LiveRun>,
    pub grid: Entity<grid::LarGrid>,
}

impl TabLar {
    pub fn live(&self) -> Option<&Entity<live::LiveRun>> {
        self.live.as_ref().map(|v| &v.run)
    }
}

pub fn register(cx: &mut App) {
    cx.global_mut::<crate::panels::PanelRegistry>().register(LivePanel);
}

/// The live queries panel (⌥1).
struct LivePanel;

impl ServerPanel for LivePanel {
    fn extension(&self) -> &'static str {
        diskman_core::server::LAR
    }

    fn id(&self) -> &'static str {
        "live"
    }

    fn title(&self) -> SharedString {
        "Live".into()
    }

    fn width(&self) -> Option<f32> {
        Some(360.)
    }

    fn build(&self, conn: Arc<Connection>, window: &mut Window, cx: &mut App) -> AnyView {
        let ws = workspace(window).map(|w| w.downgrade()).unwrap_or_else(WeakEntity::new_invalid);
        cx.new(|cx| live::LivePanel::new(conn, ws, cx)).into()
    }
}

// ---------------------------------------------------------------------------------------------
// Small shared helpers.

pub fn workspace(window: &Window) -> Option<Entity<Workspace>> {
    window.root::<Workspace>().flatten()
}

pub fn active_tab(ws: &WeakEntity<Workspace>, cx: &App) -> Option<Entity<QueryTab>> {
    ws.upgrade().and_then(|w| w.read(cx).active_tab().cloned())
}

/// The tab a panel built for `conn` may act on: the active tab, only when it is on that same
/// connection (a Live panel opened on LAR X must never act on a tab of another server).
pub fn target_tab(ws: &WeakEntity<Workspace>, conn: &Arc<Connection>, cx: &App) -> Option<Entity<QueryTab>> {
    active_tab(ws, cx).filter(|t| same_conn(t.read(cx).connection(), conn))
}

pub fn same_conn(a: Option<&Arc<Connection>>, b: &Arc<Connection>) -> bool {
    a.is_some_and(|a| Arc::ptr_eq(a, b))
}

/// `#48 113`: commit numbers grouped by thin spaces, as the boards print them.
pub fn fmt_commit(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::from("#");
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// The table a simple query reads first (`FROM [schema.]table`).
pub fn first_table(sql: &str) -> Option<String> {
    // `FROM lar_history(…)`, `FROM (subquery)`: not a table.
    diskman_core::sql::first_table(sql)
}

/// `SUBSCRIBE …`, `COPY (SUBSCRIBE …) TO STDOUT` and `LISTEN ch AS …`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveStmt {
    Subscribe { query: String, copy_form: bool },
    Listen { channel: String, query: String },
}

pub fn parse_live(sql: &str) -> Option<LiveStmt> {
    let s = strip_comments(sql);
    let s = s.trim().trim_end_matches(';').trim();
    let upper = s.to_ascii_uppercase();
    if upper.starts_with("SUBSCRIBE ") || upper.starts_with("SUBSCRIBE\n") {
        return Some(LiveStmt::Subscribe { query: s[9..].trim().to_string(), copy_form: false });
    }
    if upper.starts_with("COPY") {
        let rest = s[4..].trim_start();
        if let Some(inner) = rest.strip_prefix('(') {
            let up = inner.trim_start().to_ascii_uppercase();
            if up.starts_with("SUBSCRIBE") {
                let close = inner.rfind(')')?;
                let q = inner[..close].trim_start();
                return Some(LiveStmt::Subscribe { query: q[9..].trim().to_string(), copy_form: true });
            }
        }
        return None;
    }
    if upper.starts_with("LISTEN ") || upper.starts_with("LISTEN\n") {
        let rest = s[6..].trim_start();
        let ch_end = rest.find(char::is_whitespace)?;
        let channel = rest[..ch_end].trim_matches('"').to_string();
        let after = rest[ch_end..].trim_start();
        if after.get(..2).is_some_and(|a| a.eq_ignore_ascii_case("AS")) && after[2..].starts_with(char::is_whitespace) {
            return Some(LiveStmt::Listen { channel, query: after[2..].trim().to_string() });
        }
    }
    None
}

/// The statement without `--` line comments in front (a leading comment is common).
fn strip_comments(sql: &str) -> String {
    sql.lines().filter(|l| !l.trim_start().starts_with("--")).collect::<Vec<_>>().join("\n")
}

// ---------------------------------------------------------------------------------------------
// Hooks the shell calls.

/// Before a statement runs in a tab: a live statement starts on its own connection and the
/// tab shows it (`true`: nothing else to run). Plain PostgreSQL is untouched.
pub fn start_live(tab: &mut QueryTab, sql: &str, cx: &mut Context<QueryTab>) -> bool {
    let Some(conn) = tab.connection().cloned().filter(|c| c.lar().is_some()) else { return false };
    if let Some(stmt) = parse_live(sql) {
        let run = cx.new(|cx| live::LiveRun::start(conn, stmt, sql.to_string(), cx));
        let grid = run.read(cx).grid.clone();
        cx.observe(&run, |_, _, cx| cx.notify()).detach();
        tab.lar.live = Some(LiveView { run, grid });
        return true;
    }
    // Anything else gives the grid back (a live query in this tab stops when dropped).
    tab.lar.live = None;
    if first_table(sql).is_some() {
        tab.lar.last_query = Some(sql.trim().trim_end_matches(';').to_string());
    }
    false
}

/// `sql` would start a live query in this tab (a LAR connection and a live statement).
pub fn is_live(tab: &QueryTab, sql: &str) -> bool {
    tab.connection().is_some_and(|c| c.lar().is_some()) && parse_live(sql).is_some()
}

/// The tab's result grid is not on screen (live rows stand in its place): its grid edits must
/// not be committed from there.
pub fn hides_result(tab: &QueryTab) -> bool {
    tab.lar.live.is_some()
}

/// The live rows standing in for the result grid.
pub fn grid_override(tab: &Entity<QueryTab>, cx: &mut App) -> Option<AnyElement> {
    let grid = tab.read(cx).lar.live.as_ref()?.grid.clone();
    Some(grid.into_any_element())
}

/// `● live` / `‖ live` / `■` in the title bar tab.
pub fn tab_indicator(tab: &QueryTab, t: &Theme, cx: &App) -> Option<(SharedString, Hsla)> {
    use diskman_core::lar::live::SubState;
    if tab.is_running() || tab.is_cancelling() {
        return None;
    }
    if let Some(l) = tab.lar.live() {
        return Some(match l.read(cx).state() {
            SubState::Live | SubState::Starting => ("● live".into(), t.lar),
            SubState::Paused => ("‖ live".into(), t.info),
            SubState::Ended { error: Some(_) } => ("■ ended".into(), t.warn),
            SubState::Ended { error: None } => ("■".into(), t.tx3),
        });
    }
    None
}

/// The status bar's LAR segment: `● live · 142 changes/s`.
pub fn status_text(tab: &QueryTab, cx: &App) -> Option<(SharedString, Hsla)> {
    use crate::theme::ActiveTheme;
    use diskman_core::lar::live::SubState;
    let t = cx.theme();
    let l = tab.lar.live()?.read(cx);
    Some(match l.state() {
        SubState::Live | SubState::Starting => (format!("● live · {} changes/s", l.current_rate() as u64).into(), t.lar),
        SubState::Paused => ("‖ live paused".into(), t.info),
        SubState::Ended { .. } => ("■ stream ended".into(), t.tx3),
    })
}

/// What the result bar shows for a live query.
pub struct LarBar {
    pub state: ResultState,
    pub label: SharedString,
    pub rows: Option<String>,
    pub rows_label: Option<&'static str>,
    pub metrics: Vec<(&'static str, String)>,
    pub note: String,
    pub note_err: bool,
    pub actions: Vec<(&'static str, &'static str, &'static str, Box<dyn Action>)>,
}

pub fn result_bar(tab: &Entity<QueryTab>, cx: &mut App) -> Option<LarBar> {
    tab.read(cx).lar.live().map(|l| l.read(cx).bar())
}

/// The LAR actions, handled on the workspace root.
pub fn workspace_actions(d: Stateful<gpui::Div>, cx: &mut Context<Workspace>) -> Stateful<gpui::Div> {
    d.on_action(cx.listener(|w, _: &PauseLive, _, cx| {
            if let Some(l) = w.active_tab().and_then(|t| t.read(cx).lar.live().cloned()) {
                l.update(cx, |l, cx| l.toggle_pause(cx));
            }
        }))
        .on_action(cx.listener(|w, _: &StopLive, _, cx| {
            if let Some(l) = w.active_tab().and_then(|t| t.read(cx).lar.live().cloned()) {
                l.update(cx, |l, cx| l.stop(cx));
            }
        }))
        .on_action(cx.listener(|w, _: &Resubscribe, _, cx| {
            if let Some(tab) = w.active_tab().cloned()
                && let Some(l) = tab.read(cx).lar.live().cloned()
            {
                let sql = l.read(cx).source_sql.clone();
                tab.update(cx, |t, cx| t.run(vec![sql], cx));
            }
        }))
        .on_action(cx.listener(|w, _: &KeepSnapshot, _, cx| {
            if let Some(l) = w.active_tab().and_then(|t| t.read(cx).lar.live().cloned()) {
                l.update(cx, |l, cx| l.keep_snapshot(cx));
            }
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_labels() {
        assert_eq!(fmt_commit(7), "#7");
        assert_eq!(fmt_commit(48113), "#48 113");
        assert_eq!(fmt_commit(1234567), "#1 234 567");
    }

    #[test]
    fn live_statements() {
        assert_eq!(
            parse_live("-- deltas\nSUBSCRIBE SELECT id FROM t WHERE x = 1;"),
            Some(LiveStmt::Subscribe { query: "SELECT id FROM t WHERE x = 1".into(), copy_form: false })
        );
        assert_eq!(
            parse_live("COPY (SUBSCRIBE SELECT * FROM t) TO STDOUT"),
            Some(LiveStmt::Subscribe { query: "SELECT * FROM t".into(), copy_form: true })
        );
        assert_eq!(
            parse_live("LISTEN big_orders AS\n  SELECT id, total FROM orders WHERE total > 1000"),
            Some(LiveStmt::Listen { channel: "big_orders".into(), query: "SELECT id, total FROM orders WHERE total > 1000".into() })
        );
        assert_eq!(parse_live("LISTEN plain"), None);
        assert_eq!(parse_live("SELECT 1"), None);
    }

    #[test]
    fn tables_of_queries() {
        assert_eq!(first_table("SELECT * FROM demo_orders WHERE id = 1").as_deref(), Some("demo_orders"));
        assert_eq!(first_table("select id from public.\"Orders\" o").as_deref(), Some("Orders"));
        assert_eq!(first_table("SELECT * FROM lar_history('t', 1)"), None);
        assert_eq!(first_table("SELECT 1"), None);
    }

    #[test]
    fn multibyte_after_a_channel_does_not_panic() {
        assert_eq!(parse_live("LISTEN ch €x"), None);
        assert_eq!(parse_live("LISTEN ch ÅS SELECT 1"), None);
        assert_eq!(parse_live("LISTEN ch\u{a0}x"), None);
    }
}
