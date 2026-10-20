//! `edit-check`: editing a PostgreSQL result in the grid with keys and clicks (a cell, a deleted
//! row, NULL, discard), reviewing and committing, a conflict and Overwrite, committing without
//! review, and the result zoom on ⌘⇧↩. Every commit is read back on a connection of its own.

use gpui::{AsyncWindowContext, Entity, Focusable};
use dataman_core::config::Engine;

use crate::app::Workspace;
use crate::check::*;
use crate::tab::QueryTab;

const SQL: &str = "SELECT * FROM dataman_scratch.products_edit ORDER BY id";
const NAME: usize = 2;
const PRICE: usize = 4;
const STOCK: usize = 5;

fn row_of(tab: &Entity<QueryTab>, id: &str, cx: &mut AsyncWindowContext) -> Option<usize> {
    let rows = result(tab, cx).1;
    (0..rows).find(|&r| cell(tab, r, 0, cx).as_deref() == Some(id))
}

fn pending(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> usize {
    cx.update(|_, cx| tab.read(cx).edit.read(cx).pending_count()).unwrap_or(0)
}

/// Clicks the cell, ↩ opens its editor, the text replaces the value, ↩ keeps it.
async fn edit_cell(tab: &Entity<QueryTab>, id: &str, col: usize, text: Option<&str>, cx: &mut AsyncWindowContext) -> bool {
    let Some(row) = row_of(tab, id, cx) else { return false };
    let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
    draw(cx);
    let p = cx.update(|_, cx| grid.read(cx).demo_cell_point(row, col)).unwrap();
    click(cx, p);
    key(cx, "enter");
    pause(cx, 50).await;
    match text {
        Some(t) => {
            key(cx, "cmd-a");
            typ(cx, t);
        }
        None => key(cx, "cmd-shift-n"),
    }
    key(cx, "enter");
    pause(cx, 50).await;
    true
}

/// The review opens in the side panel as it slides in: its Commit button, clicked once there.
async fn commit(cx: &mut AsyncWindowContext) {
    until(cx, 5, |cx| probed("rv-commit", cx).is_some_and(|b| b.size.width > gpui::px(20.))).await;
    pause(cx, 200).await;
    click_probe("rv-commit", cx);
}

async fn committed(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> bool {
    until(cx, 15, |cx| pending(tab, cx) == 0 && idle(tab, cx)).await
}

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (pg, pw) = profile(Engine::Postgres);
    let up = connect(ws, &pg, &pw, cx).await;
    if !r.check("PostgreSQL connects", up.is_ok(), format!("{up:?}")) {
        return;
    }
    let core = core(ws, cx);
    // A copy of its own: the commits below leave the seeded products to the other stages.
    for stmt in [
        "DROP TABLE IF EXISTS dataman_scratch.products_edit",
        "CREATE TABLE dataman_scratch.products_edit (LIKE dataman_scratch.products INCLUDING ALL)",
        "INSERT INTO dataman_scratch.products_edit SELECT * FROM dataman_scratch.products",
    ] {
        if let Err(e) = sql(&core, &pg, &pw, stmt, cx).await {
            r.check("the scratch copy of the products is made", false, e);
            return;
        }
    }
    let read = |col: &str, id: &str| format!("SELECT {col} FROM dataman_scratch.products_edit WHERE id = {id}");
    let Some(tab) = run_keys(ws, SQL, "cmd-enter", cx).await else { return };
    let (phase, rows, err) = result(&tab, cx);
    r.check("the products result runs", rows >= 10, format!("{phase:?} {rows} {err}"));

    edit_cell(&tab, "101", NAME, Some("Aurora edited"), cx).await;
    let one = pending(&tab, cx);
    let row = row_of(&tab, "103", cx).unwrap_or(2);
    let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
    let p = cx.update(|_, cx| grid.read(cx).demo_cell_point(row, 0)).unwrap();
    click(cx, p);
    key(cx, "backspace");
    let two = pending(&tab, cx);
    r.check("↩ on a cell edits it, ⌫ on a row marks it deleted: two pending changes", one == 1 && two == 2, format!("{one} then {two}"));
    key(cx, "cmd-s");
    pause(cx, 100).await;
    let open = cx.update(|_, cx| tab.read(cx).edit.read(cx).review_open).unwrap_or(false);
    r.check("⌘S in the grid opens the review", open, "no review");
    commit(cx).await;
    let done = committed(&tab, cx).await;
    let name = sql1(&core, &pg, &pw, &read("name", "101"), cx).await;
    let gone = sql1(&core, &pg, &pw, "SELECT count(*) FROM dataman_scratch.products_edit WHERE id = 103", cx).await;
    r.check("Commit writes the edit and the delete", done && name.as_deref() == Some("Aurora edited") && gone.as_deref() == Some("0"), format!("{done} {name:?} {gone:?}"));

    edit_cell(&tab, "104", PRICE, Some("1"), cx).await;
    let had = pending(&tab, cx);
    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
    key(cx, "cmd-backspace");
    let left = pending(&tab, cx);
    let price = sql1(&core, &pg, &pw, &read("price", "104"), cx).await;
    r.check("⌘⌫ discards the pending edit; the table keeps its value", had == 1 && left == 0 && price.as_deref() == Some("49.00"), format!("{had} → {left} {price:?}"));

    edit_cell(&tab, "104", STOCK, None, cx).await;
    key(cx, "cmd-s");
    commit(cx).await;
    committed(&tab, cx).await;
    let stock = sql1(&core, &pg, &pw, "SELECT coalesce(stock::text, 'null') FROM dataman_scratch.products_edit WHERE id = 104", cx).await;
    r.check("⌘⇧N sets NULL, committed as NULL", stock.as_deref() == Some("null"), format!("{stock:?}"));

    edit_cell(&tab, "105", NAME, Some("mine"), cx).await;
    let _ = sql(&core, &pg, &pw, "UPDATE dataman_scratch.products_edit SET name = 'theirs' WHERE id = 105", cx).await;
    key(cx, "cmd-s");
    commit(cx).await;
    until(cx, 10, |cx| cx.update(|_, cx| tab.read(cx).edit.read(cx).demo_conflicts()).unwrap_or(0) > 0).await;
    let conflicts = cx.update(|_, cx| tab.read(cx).edit.read(cx).demo_conflicts()).unwrap_or(0);
    let kept = sql1(&core, &pg, &pw, &read("name", "105"), cx).await;
    r.check("a row changed underneath: the commit stops on the conflict, theirs kept", conflicts == 1 && kept.as_deref() == Some("theirs"), format!("{conflicts} {kept:?}"));
    until(cx, 5, |cx| probed("cf-over", cx).is_some()).await;
    click_probe("cf-over", cx);
    committed(&tab, cx).await;
    let now = sql1(&core, &pg, &pw, &read("name", "105"), cx).await;
    r.check("Overwrite then writes mine", now.as_deref() == Some("mine"), format!("{now:?}"));

    let _ = core.settings.update(|s| s.confirm_commit = false);
    edit_cell(&tab, "106", NAME, Some("no review"), cx).await;
    key(cx, "cmd-s");
    let open = cx.update(|_, cx| tab.read(cx).edit.read(cx).review_open).unwrap_or(true);
    committed(&tab, cx).await;
    let name = sql1(&core, &pg, &pw, &read("name", "106"), cx).await;
    r.check("with review off, ⌘S commits at once", !open && name.as_deref() == Some("no review"), format!("review {open} {name:?}"));
    let _ = core.settings.update(|s| s.confirm_commit = true);

    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
    key(cx, "cmd-shift-enter");
    let zoomed = cx.update(|_, cx| tab.read(cx).zoomed).unwrap_or(false);
    key(cx, "escape");
    let back = cx.update(|_, cx| tab.read(cx).zoomed).unwrap_or(true);
    r.check("⌘⇧↩ in the grid zooms the result; esc puts it back", zoomed && !back, format!("{zoomed} {back}"));
}
