//! `dropdowns-check`: a second click on a dropdown's button closes it (the history's status and
//! connection filters, the result bar's Copy, the export sheet's fields). `history-filter-check`:
//! the history narrowed to a connection or one of its databases, with the status filter (AND),
//! what resets them, and its days shut and opened.

use chrono::Datelike as _;
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

    let filter_text = |cx: &mut AsyncWindowContext| cx.update(|_, cx| h.read(cx).demo_filter_text(cx)).unwrap_or_default();
    r.check("a list with rows offers no Reset filters", probed("history-reset", cx).is_none(), "");
    click_probe("history-chip", cx);
    click_probe("Error", cx);
    click_probe("history-scope", cx);
    click_probe(&format!("hscope:{}", my.id), cx);
    let _ = cx.update(|window, cx| h.update(cx, |h, cx| h.demo_search("nothing like this", window, cx)));
    let nothing = shown(cx);
    let offered = probed("history-reset", cx).is_some();
    click_probe("history-reset", cx);
    let scope = cx.update(|_, cx| h.read(cx).demo_scope().0).ok().flatten();
    let text = filter_text(cx);
    r.check(
        "History with a status, a connection and a search matching nothing: Reset filters clears all three",
        nothing == 0 && offered && scope.is_none() && text.is_empty() && shown(cx) == all && probed("history-reset", cx).is_none(),
        format!("{nothing} {offered} {scope:?} {text:?} {}", shown(cx)),
    );

    let _ = cx.update(|window, cx| h.update(cx, |h, cx| h.demo_search("kept for History", window, cx)));
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.show_nav(NavView::Favorites, window, cx)));
    draw(cx);
    let slate = probed("history-reset", cx).is_none();
    let _ = cx.update(|_, cx| {
        let core = ws.read(cx).store.read(cx).core.clone();
        let mut f = dataman_core::Favorite::new("Only on PostgreSQL", "SELECT 'reset' AS check");
        f.connection_id = Some(pg.id.clone());
        let saved = core.favorites.add(f);
        h.update(cx, |h, cx| h.reload_favorites(cx));
        saved.map(|_| ())
    });
    let favs = |cx: &mut AsyncWindowContext| crate::history_check::rows(&h, cx);
    click_probe("fav-scope", cx);
    click_probe(&format!("fscope:{}", my.id), cx);
    let _ = cx.update(|window, cx| h.update(cx, |h, cx| h.demo_search("nothing like this", window, cx)));
    let (emptied, offered) = (favs(cx).is_empty(), probed("history-reset", cx).is_some());
    click_probe("history-reset", cx);
    let scope = cx.update(|_, cx| h.read(cx).demo_fav_scope().0).ok().flatten();
    let (text, listed) = (filter_text(cx), favs(cx));
    r.check(
        "Favorites with no favorites: no Reset filters; with a connection and a search matching nothing: it clears both",
        slate && emptied && offered && scope.is_none() && text.is_empty() && listed == ["Only on PostgreSQL"],
        format!("{slate} {emptied} {offered} {scope:?} {text:?} {listed:?}"),
    );
    show_history(ws, cx);
    let text = filter_text(cx);
    r.check("…and History keeps its own search", text == "kept for History", &text);
    let _ = cx.update(|window, cx| h.update(cx, |h, cx| h.demo_search("", window, cx)));
    days(ws, &pg.id, &h, r, cx).await;
}

/// Runs of other days written to the history file, then each day shut and opened through the
/// list itself: clicks and keys, under the filters and the search, and across a reload.
async fn days(ws: &Entity<Workspace>, conn: &str, h: &Entity<HistoryPanel>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let now = chrono::Local::now();
    let run = |days: i64, sql: &str, outcome: dataman_core::Outcome| dataman_core::HistoryEntry {
        ts: (now - chrono::Duration::days(days)).with_timezone(&chrono::Utc),
        database: "e2e".into(),
        sql: sql.into(),
        duration_ms: 1.,
        rows: None,
        outcome,
        error: None,
        source: None,
    };
    let older = (now - chrono::Duration::days(9)).date_naive();
    let older = if older.year() == now.year() { older.format("%a %d %b") } else { older.format("%d %b %Y") }.to_string().to_uppercase();
    let store = core(ws, cx);
    let wrote = store.history.append(conn, &run(9, "SELECT 'older_day'", dataman_core::Outcome::Ok))
        .and_then(|_| store.history.append(conn, &run(1, "SELECT 'yesterday_ok'", dataman_core::Outcome::Ok)))
        .and_then(|_| store.history.append(conn, &run(1, "SELECT 'yesterday_failed'", dataman_core::Outcome::Error)));
    r.check("runs of other days written", wrote.is_ok(), format!("{wrote:?}"));
    let _ = cx.update(|_, cx| h.update(cx, |h, cx| h.reload_soon(cx)));
    let rows = |cx: &mut AsyncWindowContext| crate::history_check::rows(h, cx);
    let (open_y, shut_y, open_o) = ("▾ YESTERDAY".to_string(), "▸ YESTERDAY".to_string(), format!("▾ {older}"));
    until(cx, 5, |cx| rows(cx).contains(&open_o)).await;
    let at = |row: &str, cx: &mut AsyncWindowContext| rows(cx).iter().position(|x| x == row);
    let entries = |cx: &mut AsyncWindowContext| rows(cx).iter().filter(|x| *x == "entry").count();
    let all = rows(cx);
    let n = entries(cx);
    let ordered = all.first().map(String::as_str) == Some("▾ TODAY") && at(&open_y, cx).is_some_and(|y| all.get(y + 1).is_some_and(|x| x == "entry") && all.get(y + 2).is_some_and(|x| x == "entry") && all.get(y + 3) == Some(&open_o));
    r.check("the history is grouped under its days, newest first, all open", ordered, format!("{all:?}"));

    let shown = |cx: &mut AsyncWindowContext| cx.update(|_, cx| h.read(cx).demo_scope().2).unwrap_or(0);
    let total = shown(cx);
    let selected = |cx: &mut AsyncWindowContext| cx.update(|_, cx| h.read(cx).demo_selected_row()).ok().flatten();
    let header = at(&open_y, cx).unwrap_or(0);
    let Some(p) = crate::history_check::row_point(h, header, cx).await else {
        r.check("the day's header has bounds", false, "");
        return;
    };
    let click_at = |p, cx: &mut AsyncWindowContext| crate::check::click(cx, p);
    click_at(p, cx);
    let after = rows(cx);
    r.check(
        "a click shuts a day: its runs go, the header stays, selected; the search's count still has them",
        after.get(header) == Some(&shut_y) && after.get(header + 1) == Some(&open_o) && entries(cx) == n - 2 && shown(cx) == total && selected(cx).as_ref() == Some(&shut_y),
        format!("{after:?} shown {} of {total}, selected {:?}", shown(cx), selected(cx)),
    );
    let focus = |cx: &mut AsyncWindowContext| cx.update(|window, cx| h.read(cx).demo_has_focus(window)).unwrap_or(false);
    r.check("…and the panel has the keys", focus(cx), "");
    key(cx, "right");
    let opened = rows(cx).contains(&open_y) && entries(cx) == n;
    key(cx, "left");
    let shut = rows(cx).contains(&shut_y) && entries(cx) == n - 2;
    key(cx, "enter");
    let entered = rows(cx).contains(&open_y) && entries(cx) == n;
    key(cx, "down");
    let on_run = selected(cx).as_deref() == Some("entry");
    key(cx, "left");
    let back = selected(cx).as_ref() == Some(&open_y) && rows(cx).contains(&open_y);
    key(cx, "up");
    let above = selected(cx).as_deref() == Some("entry");
    r.check("→ opens the day, ← shuts it, ↩ toggles it; ↓ reaches a run, ← from there goes up to its day, ↑ moves on", opened && shut && entered && on_run && back && above, format!("{opened} {shut} {entered} {on_run} {back} {above}"));

    let Some(p) = crate::history_check::row_point(h, header, cx).await else {
        r.check("the day's header has bounds", false, "");
        return;
    };
    click_at(p, cx);
    click_probe("history-chip", cx);
    click_probe("Error", cx);
    pause(cx, 50).await;
    let errors = rows(cx);
    let (failed, had_today) = (shown(cx), errors.first().map(String::as_str) == Some("▾ TODAY"));
    r.check(
        "the status filter works inside the days: a day with no failure goes, a shut one with a failure stays shut",
        had_today && errors.contains(&shut_y) && !errors.contains(&open_o) && failed >= 2,
        format!("{errors:?} {failed}"),
    );
    click_probe("history-chip", cx);
    click_probe("All", cx);
    pause(cx, 50).await;

    let search = |q: &str, cx: &mut AsyncWindowContext| {
        let _ = cx.update(|window, cx| h.update(cx, |h, cx| h.demo_search(q, window, cx)));
        crate::history_check::rows(h, cx)
    };
    let found = search("yesterday_", cx);
    let matched = shown(cx);
    r.check("a search opens the day holding its matches, shut or not", found == [open_y.as_str(), "entry", "entry"] && matched == 2, format!("{found:?} {matched}"));
    let Some(p) = crate::history_check::row_point(h, 0, cx).await else {
        r.check("the day's header has bounds", false, "");
        return;
    };
    click_at(p, cx);
    let shut_there = rows(cx);
    let cleared = search("", cx);
    let again = search("yesterday_", cx);
    let back = search("", cx);
    r.check(
        "a day can be shut under a search; cleared, the days are as they were, and a new search opens it again",
        shut_there == [shut_y.as_str()] && cleared.contains(&shut_y) && again == [open_y.as_str(), "entry", "entry"] && back.contains(&shut_y),
        format!("{shut_there:?} {cleared:?} {again:?} {back:?}"),
    );

    let _ = cx.update(|_, cx| h.update(cx, |h, cx| h.reload_soon(cx)));
    pause(cx, 100).await;
    until(cx, 5, |cx| cx.update(|_, cx| h.read(cx).demo_entry_count() > 0).unwrap_or(false)).await;
    let reloaded = rows(cx);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.show_nav(NavView::Explorer, window, cx)));
    draw(cx);
    show_history(ws, cx);
    let returned = rows(cx);
    r.check("a day stays shut across a reload of the files and a trip to another view", reloaded.contains(&shut_y) && returned.contains(&shut_y), format!("{reloaded:?} {returned:?}"));
}
