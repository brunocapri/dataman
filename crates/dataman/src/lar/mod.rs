#[cfg(feature = "checks")]
pub mod demo;
pub mod grid;
pub mod live;
pub mod widgets;

use std::sync::Arc;

use gpui::{
    Action, AnyElement, AnyView, App, AppContext, Context, Entity, Hsla, IntoElement, SharedString, Stateful, WeakEntity, Window,
    actions, prelude::*,
};
use dataman_core::Connection;

use crate::app::Workspace;
use crate::panels::ServerPanel;
use crate::result_bar::ResultState;
use crate::tab::QueryTab;
use crate::theme::Theme;

actions!(
    lar,
    [
        PauseLive,
        StopLive,
        Resubscribe,
        KeepSnapshot,
    ]
);

pub fn bind_keys(cx: &mut App) {
    grid::bind_keys(cx);
}

#[derive(Default)]
pub struct TabLar {
    pub live: Option<LiveView>,
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

struct LivePanel;

impl ServerPanel for LivePanel {
    fn extension(&self) -> &'static str {
        dataman_core::server::LAR
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

pub fn workspace(window: &Window) -> Option<Entity<Workspace>> {
    window.root::<Workspace>().flatten()
}

pub fn active_tab(ws: &WeakEntity<Workspace>, cx: &App) -> Option<Entity<QueryTab>> {
    ws.upgrade().and_then(|w| w.read(cx).active_tab().cloned())
}

/// Only the active tab on that same connection: a panel on one server must never act on another's tab.
pub fn target_tab(ws: &WeakEntity<Workspace>, conn: &Arc<Connection>, cx: &App) -> Option<Entity<QueryTab>> {
    active_tab(ws, cx).filter(|t| same_conn(t.read(cx).connection(), conn))
}

pub fn same_conn(a: Option<&Arc<Connection>>, b: &Arc<Connection>) -> bool {
    a.is_some_and(|a| Arc::ptr_eq(a, b))
}

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

pub fn first_table(sql: &str) -> Option<String> {
    dataman_core::sql::first_table(sql)
}

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

fn strip_comments(sql: &str) -> String {
    sql.lines().filter(|l| !l.trim_start().starts_with("--")).collect::<Vec<_>>().join("\n")
}

pub fn start_live(tab: &mut QueryTab, sql: &str, cx: &mut Context<QueryTab>) -> bool {
    let Some(conn) = tab.connection().cloned().filter(|c| c.lar().is_some()) else { return false };
    if let Some(stmt) = parse_live(sql) {
        let run = cx.new(|cx| live::LiveRun::start(conn, stmt, sql.to_string(), cx));
        let grid = run.read(cx).grid.clone();
        cx.observe(&run, |_, _, cx| cx.notify()).detach();
        tab.lar.live = Some(LiveView { run, grid });
        return true;
    }
    tab.lar.live = None;
    if first_table(sql).is_some() {
        tab.lar.last_query = Some(sql.trim().trim_end_matches(';').to_string());
    }
    false
}

pub fn is_live(tab: &QueryTab, sql: &str) -> bool {
    tab.connection().is_some_and(|c| c.lar().is_some()) && parse_live(sql).is_some()
}

pub fn hides_result(tab: &QueryTab) -> bool {
    tab.lar.live.is_some()
}

pub fn grid_override(tab: &Entity<QueryTab>, cx: &mut App) -> Option<AnyElement> {
    let grid = tab.read(cx).lar.live.as_ref()?.grid.clone();
    Some(grid.into_any_element())
}

pub fn tab_indicator(tab: &QueryTab, t: &Theme, cx: &App) -> Option<(SharedString, Hsla)> {
    use dataman_core::lar::live::SubState;
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

pub fn status_text(tab: &QueryTab, cx: &App) -> Option<(SharedString, Hsla)> {
    use crate::theme::ActiveTheme;
    use dataman_core::lar::live::SubState;
    let t = cx.theme();
    let l = tab.lar.live()?.read(cx);
    Some(match l.state() {
        SubState::Live | SubState::Starting => (format!("● live · {} changes/s", l.current_rate() as u64).into(), t.lar),
        SubState::Paused => ("‖ live paused".into(), t.info),
        SubState::Ended { .. } => ("■ stream ended".into(), t.tx3),
    })
}

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
