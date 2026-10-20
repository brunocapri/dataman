//! `dropdowns-check`: a second click on a dropdown's button closes it (the history's status and
//! connection filters, the result bar's Copy, the export sheet's fields). `history-filter-check`:
//! the history narrowed to a connection or one of its databases, with the status filter (AND).

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
    let (opened, after) = toggles("history-scope", |cx| cx.update(|_, cx| h.read(cx).demo_scope().1).unwrap_or(false), cx).await;
    r.check("History's connection filter: the same", opened && !after, format!("{opened} {after}"));

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

pub async fn run_history_filter(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (pg, pgpw) = profile(Engine::Postgres);
    let (my, mypw) = profile(Engine::MySql);
    for (p, pw) in [(&pg, &pgpw), (&my, &mypw)] {
        let up = connect(ws, p, pw, cx).await;
        r.check(&format!("{} connects", p.engine.label()), up.is_ok(), format!("{up:?}"));
    }
    let open_on = |id: String, cx: &mut AsyncWindowContext| {
        let _ = cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                w.current_conn = Some(id);
                w.new_tab(window, cx);
            })
        });
    };
    open_on(pg.id.clone(), cx);
    run_keys(ws, "SELECT 1 AS pg_ok", "cmd-enter", cx).await;
    run_keys(ws, "SELECT * FROM no_such_table_pg", "cmd-enter", cx).await;
    open_on(my.id.clone(), cx);
    run_keys(ws, "SELECT 2 AS my_ok", "cmd-enter", cx).await;
    run_keys(ws, "SELECT 3 AS my_ok_too", "cmd-enter", cx).await;
    show_history(ws, cx);
    let h = history(ws, cx);
    let shown = |cx: &mut AsyncWindowContext| cx.update(|_, cx| h.read(cx).demo_scope().2).unwrap_or(0);
    until(cx, 5, |cx| shown(cx) >= 4).await;
    let all = shown(cx);
    r.check("the history lists the runs of both connections", all >= 4, all);

    click_probe("history-scope", cx);
    let listed = probed(&format!("hscope:{}", pg.id), cx).is_some() && probed(&format!("hscope:{}", my.id), cx).is_some() && probed(&format!("hscope:{}:{}", my.id, my.database), cx).is_some();
    r.check("the connection filter lists every connection and the databases their history used", listed, "");
    click_probe(&format!("hscope:{}", pg.id), cx);
    pause(cx, 100).await;
    let on_pg = shown(cx);
    let scope = cx.update(|_, cx| h.read(cx).demo_scope().0).ok().flatten();
    r.check("picking PostgreSQL leaves its two runs", on_pg == 2 && scope == Some((pg.id.clone(), None)), format!("{on_pg} {scope:?}"));

    click_probe("history-chip", cx);
    click_probe("Error", cx);
    pause(cx, 100).await;
    let pg_errors = shown(cx);
    r.check("…with the status filter on Error: its failed run only (both filters apply)", pg_errors == 1, pg_errors);

    click_probe("history-scope", cx);
    click_probe(&format!("hscope:{}:{}", my.id, my.database), cx);
    pause(cx, 100).await;
    let my_errors = shown(cx);
    click_probe("history-chip", cx);
    click_probe("All", cx);
    pause(cx, 100).await;
    let my_all = shown(cx);
    r.check("MySQL's database with Error: nothing; with All: its two runs", my_errors == 0 && my_all == 2, format!("{my_errors} {my_all}"));

    click_probe("history-scope", cx);
    click_probe("hscope:all", cx);
    pause(cx, 100).await;
    r.check("All connections brings everything back", shown(cx) == all, shown(cx));
}
