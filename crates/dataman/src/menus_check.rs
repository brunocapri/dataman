//! `dropdowns-check`: a second click on a dropdown's button closes it (the history's status
//! filter, the result bar's Copy, the export sheet's fields).

use gpui::{AsyncWindowContext, Entity};
use dataman_core::config::Engine;
use dataman_core::layout::NavView;

use crate::app::Workspace;
use crate::check::*;
use crate::history::HistoryPanel;

fn history(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Entity<HistoryPanel> {
    cx.update(|_, cx| ws.read(cx).history.clone()).unwrap()
}

fn show_history(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.show_nav(NavView::History, window, cx)));
    draw(cx);
}

/// Clicks the button: open; clicks it again: closed.
async fn toggles(id: &str, open: impl Fn(&mut AsyncWindowContext) -> bool, cx: &mut AsyncWindowContext) -> (bool, bool) {
    click_probe(id, cx);
    pause(cx, 100).await;
    let first = open(cx);
    click_probe(id, cx);
    pause(cx, 100).await;
    (first, open(cx))
}

pub async fn run_dropdowns(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (pg, pw) = profile(Engine::Postgres);
    let up = connect(ws, &pg, &pw, cx).await;
    if !r.check("PostgreSQL connects", up.is_ok(), format!("{up:?}")) {
        return;
    }
    run_keys(ws, "SELECT id, name FROM customer ORDER BY id LIMIT 20", "cmd-enter", cx).await;
    show_history(ws, cx);
    let h = history(ws, cx);
    let (opened, after) = toggles("history-chip", |cx| cx.update(|_, cx| h.read(cx).demo_dropdown_open()).unwrap_or(false), cx).await;
    r.check("History's status filter: a click opens it, a second click on its button closes it", opened && !after, format!("{opened} {after}"));

    let (opened, after) = toggles("rb-copy", |cx| cx.update(|_, cx| ws.read(cx).copy_menu).unwrap_or(false), cx).await;
    r.check("The result bar's Copy menu: the same", opened && !after, format!("{opened} {after}"));

    key(cx, "escape");
    let tab = active(ws, cx).unwrap();
    let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
    let _ = cx.update(|window, cx| window.focus(&gpui::Focusable::focus_handle(grid.read(cx), cx), cx));
    key(cx, "cmd-e");
    pause(cx, 150).await;
    let Some(export) = cx.update(|_, cx| ws.read(cx).export.clone()).ok().flatten() else {
        r.check("⌘E opens the export sheet", false, "no sheet");
        return;
    };
    click_probe("ex-advanced", cx);
    let (opened, after) = toggles("HEADER", |cx| cx.update(|_, cx| export.read(cx).demo_menu_open()).unwrap_or(false), cx).await;
    r.check("The export sheet's field menus: the same", opened && !after, format!("{opened} {after}"));
    key(cx, "escape");
}
