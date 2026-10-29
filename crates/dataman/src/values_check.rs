//! `json-check`: the eye on SQL Server nvarchar and PostgreSQL text cells holding JSON (not on
//! plain or broken text), the viewer's editor, invalid JSON refused, Apply as a pending edit
//! committed with the grid's others; jsonb and MySQL's JSON too. `dates-check`: dates and times
//! typed as text in the cell (no calendar), a bad one stopped at commit with the column and the
//! format, good ones written on PostgreSQL and SQL Server.

use gpui::{AsyncWindowContext, Entity, Focusable};
use dataman_core::config::{ConnectionProfile, Engine};

use crate::app::Workspace;
use crate::check::*;
use crate::tab::QueryTab;

async fn open_table(ws: &Entity<Workspace>, p: &ConnectionProfile, schema: &str, table: &str, rows: usize, cx: &mut AsyncWindowContext) -> Option<Entity<QueryTab>> {
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_relation(&p.id, &p.database, schema, table, window, cx)));
    let tab = active(ws, cx)?;
    until(cx, 15, |cx| result(&tab, cx).1 >= rows).await;
    Some(tab)
}

fn col(tab: &Entity<QueryTab>, name: &str, cx: &mut AsyncWindowContext) -> usize {
    cx.update(|_, cx| tab.read(cx).result().and_then(|r| r.columns()).and_then(|c| c.iter().position(|c| c.name == name))).ok().flatten().unwrap_or(usize::MAX)
}

/// Clicks the cell, then its eye when the cell has one: the eye's point.
async fn eye(tab: &Entity<QueryTab>, row: usize, col: usize, cx: &mut AsyncWindowContext) -> Option<gpui::Point<gpui::Pixels>> {
    let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
    // Selected first: a cell past the window's edge scrolls in, its eye with it.
    let _ = cx.update(|_, cx| grid.update(cx, |g, cx| g.select_cell(row, col, cx)));
    draw(cx);
    let p = cx.update(|_, cx| grid.read(cx).demo_cell_point(row, col)).ok()?;
    click(cx, p);
    // The eye shows once the cell's kind and the table's editability are known (a round trip to
    // the server): a cell with none stays without one for the whole wait.
    for _ in 0..12 {
        pause(cx, 80).await;
        draw(cx);
        let at = cx.update(|_, cx| grid.read(cx).demo_eyes()).ok()?.into_iter().find(|(c, _)| *c == (row, col)).map(|(_, p)| p);
        if at.is_some() {
            return at;
        }
    }
    None
}

fn pending(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> usize {
    cx.update(|_, cx| tab.read(cx).edit.read(cx).pending_count()).unwrap_or(0)
}

/// ⌘S, then the review's Commit; waits for nothing left pending.
async fn commit(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
    let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
    key(cx, "cmd-s");
    until(cx, 5, |cx| probed("rv-commit", cx).is_some_and(|b| b.size.width > gpui::px(20.))).await;
    pause(cx, 200).await;
    click_probe("rv-commit", cx);
    until(cx, 10, |cx| pending(tab, cx) == 0 || cx.update(|_, cx| tab.read(cx).edit.read(cx).demo_card()).ok().flatten().is_some()).await;
}

/// The eye opens the viewer, Text shows its editor: the text set in it, ⌘↩.
async fn edit_in_viewer(ws: &Entity<Workspace>, at: gpui::Point<gpui::Pixels>, text: &str, cx: &mut AsyncWindowContext) -> (bool, Option<String>) {
    click(cx, at);
    pause(cx, 150).await;
    let Some(v) = cx.update(|_, cx| ws.read(cx).value_view.clone()).ok().flatten() else { return (false, Some("no viewer".into())) };
    click_probe("vv-text", cx);
    let Some((editor, _)) = cx.update(|_, cx| v.read(cx).demo_editor()).ok().flatten() else { return (true, Some("not editable".into())) };
    let _ = cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.set_text(text, cx));
        window.focus(&editor.focus_handle(cx), cx);
    });
    draw(cx);
    let error = cx.update(|_, cx| v.read(cx).demo_editor().and_then(|(_, e)| e)).ok().flatten();
    key(cx, "cmd-enter");
    pause(cx, 100).await;
    (true, error)
}

pub async fn run_json(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (ms, mpw) = profile(Engine::SqlServer);
    let up = connect(ws, &ms, &mpw, cx).await;
    if !r.check("SQL Server connects", up.is_ok(), format!("{up:?}")) {
        return;
    }
    let core = core(ws, cx);
    let _ = sql(&core, &ms, &mpw, "UPDATE dbo.documents SET body = N'{\"order\": 1001, \"customer\": {\"name\": \"Ana Lima\", \"city\": \"São Paulo\", \"tags\": [\"vip\", \"early\"]}, \"items\": [{\"sku\": \"AUD-0101\", \"qty\": 2, \"price\": 199.0}, {\"sku\": \"AUD-0105\", \"qty\": 1, \"price\": 59.0}], \"paid\": true, \"notes\": null}' WHERE id = 1", cx).await;
    let Some(tab) = open_table(ws, &ms, "dbo", "documents", 7, cx).await else { return };
    let body = col(&tab, "body", cx);
    let order = eye(&tab, 0, body, cx).await;
    let tags = eye(&tab, 2, body, cx).await;
    let plain = eye(&tab, 5, body, cx).await;
    let broken = eye(&tab, 6, body, cx).await;
    r.check(
        "SQL Server nvarchar: the eye on JSON objects and arrays (unicode too), none on plain or broken text",
        order.is_some() && tags.is_some() && plain.is_none() && broken.is_none(),
        format!("{order:?} {tags:?} {plain:?} {broken:?}"),
    );
    let Some(at) = eye(&tab, 0, body, cx).await else { return };
    let (opened, error) = edit_in_viewer(ws, at, "{\"order\": 1001, \"customer\": ", cx).await;
    let still = cx.update(|_, cx| ws.read(cx).value_view.is_some()).unwrap_or(false);
    r.check("the eye opens the viewer; broken JSON in its editor is refused and said", opened && error.is_some() && still && pending(&tab, cx) == 0, format!("{opened} {error:?} open {still}"));
    key(cx, "escape");
    let at = eye(&tab, 0, body, cx).await.unwrap_or(at);
    let edited = "{\n  \"order\": 1001,\n  \"customer\": {\"name\": \"Ana Souza\", \"city\": \"São Paulo\", \"tags\": [\"vip\", \"early\"]},\n  \"items\": [],\n  \"paid\": true\n}";
    let (_, error) = edit_in_viewer(ws, at, edited, cx).await;
    let closed = cx.update(|_, cx| ws.read(cx).value_view.is_none()).unwrap_or(false);
    // The first edit of a result asks the server for its key: the edit lands when it answers.
    for _ in 0..100 {
        if pending(&tab, cx) > 0 {
            break;
        }
        pause(cx, 50).await;
    }
    let n = pending(&tab, cx);
    r.check("Apply (⌘↩) closes the viewer: a pending edit of the cell", error.is_none() && closed && n == 1, format!("{error:?} closed {closed} pending {n}"));
    commit(&tab, cx).await;
    let stored = sql1(&core, &ms, &mpw, "SELECT body FROM dbo.documents WHERE id = 1", cx).await.unwrap_or_default();
    r.check(
        "committed with the grid's edits: the JSON is stored, on one line as it was",
        stored.contains("\"Ana Souza\"") && stored.contains("\"São Paulo\"") && !stored.contains('\n') && stored.contains("\"items\": []"),
        &stored,
    );

    let (pg, ppw) = profile(Engine::Postgres);
    let up = connect(ws, &pg, &ppw, cx).await;
    if r.check("PostgreSQL connects", up.is_ok(), format!("{up:?}"))
        && let Some(tab) = open_table(ws, &pg, "dataman_scratch", "documents", 4, cx).await
    {
        let (b, bt) = (col(&tab, "body", cx), col(&tab, "body_text", cx));
        let text_eye = eye(&tab, 0, bt, cx).await;
        let plain = eye(&tab, 3, bt, cx).await;
        r.check("PostgreSQL text holding JSON: the eye; plain text: none", text_eye.is_some() && plain.is_none(), format!("{text_eye:?} {plain:?}"));
        if let Some(at) = eye(&tab, 1, b, cx).await {
            edit_in_viewer(ws, at, "{\"theme\": \"light\", \"font\": {\"family\": \"Lilex\", \"size\": 14}}", cx).await;
            commit(&tab, cx).await;
        }
        let theme = sql1(&core, &pg, &ppw, "SELECT body->>'theme' FROM dataman_scratch.documents WHERE id = 2", cx).await;
        r.check("jsonb: edited in the viewer and committed", theme.as_deref() == Some("light"), format!("{theme:?}"));
    }

    let (my, mypw) = profile(Engine::MySql);
    let up = connect(ws, &my, &mypw, cx).await;
    if r.check("MySQL connects", up.is_ok(), format!("{up:?}"))
        && let Some(tab) = open_table(ws, &my, &my.database, "customer", 3, cx).await
    {
        let profile_col = col(&tab, "profile", cx);
        let at = eye(&tab, 0, profile_col, cx).await;
        if let Some(at) = at {
            click(cx, at);
            pause(cx, 150).await;
        }
        let editable = cx.update(|_, cx| ws.read(cx).value_view.as_ref().is_some_and(|v| v.read(cx).demo_editor().is_some())).unwrap_or(false);
        r.check("MySQL's JSON: the eye opens an editable viewer", at.is_some() && editable, format!("{at:?} {editable}"));
        key(cx, "escape");
    }
}

/// Clicks the cell, ↩ opens its editor (text, not a calendar), types over it, ↩.
async fn type_cell(tab: &Entity<QueryTab>, row: usize, col: usize, text: &str, cx: &mut AsyncWindowContext) -> bool {
    let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
    draw(cx);
    let p = cx.update(|_, cx| grid.read(cx).demo_cell_point(row, col)).unwrap();
    click(cx, p);
    key(cx, "enter");
    let edit = cx.update(|_, cx| tab.read(cx).edit.clone()).unwrap();
    // The first edit of a result asks the server about the table: the editor opens when it answers.
    for _ in 0..100 {
        pause(cx, 50).await;
        if cx.update(|_, cx| edit.read(cx).demo_cell_editor().is_some() || edit.read(cx).demo_value_editor().is_some()).unwrap_or(false) {
            break;
        }
    }
    let as_text = cx.update(|_, cx| edit.read(cx).demo_cell_editor().is_some() && edit.read(cx).demo_value_editor().is_none()).unwrap_or(false);
    key(cx, "cmd-a");
    typ(cx, text);
    key(cx, "enter");
    pause(cx, 50).await;
    as_text
}

pub async fn run_dates(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (pg, ppw) = profile(Engine::Postgres);
    let up = connect(ws, &pg, &ppw, cx).await;
    if !r.check("PostgreSQL connects", up.is_ok(), format!("{up:?}")) {
        return;
    }
    let core = core(ws, cx);
    let _ = sql(&core, &pg, &ppw, "UPDATE dataman_scratch.documents SET due = '2026-10-15', updated_at = '2026-10-01 09:00:00+00' WHERE id = 1", cx).await;
    let Some(tab) = open_table(ws, &pg, "dataman_scratch", "documents", 4, cx).await else { return };
    let (due, upd) = (col(&tab, "due", cx), col(&tab, "updated_at", cx));
    let as_text = type_cell(&tab, 0, due, "2026-02-30", cx).await;
    r.check("a date cell is edited as text: no calendar", as_text, "");
    commit(&tab, cx).await;
    let note = cx.update(|_, cx| tab.read(cx).edit.read(cx).demo_card()).ok().flatten().unwrap_or_default();
    let kept = sql1(&core, &pg, &ppw, "SELECT due::text FROM dataman_scratch.documents WHERE id = 1", cx).await;
    r.check("an impossible date is stopped at commit: the column, the value, the format", note.contains("due") && note.contains("2026-02-30") && note.contains("YYYY-MM-DD") && kept.as_deref() == Some("2026-10-15"), format!("{note:?} {kept:?}"));

    let edit = cx.update(|_, cx| tab.read(cx).edit.clone()).unwrap();
    let _ = cx.update(|window, cx| edit.update(cx, |e, cx| e.discard(window, cx)));
    pause(cx, 100).await;
    type_cell(&tab, 0, due, "2026-11-20", cx).await;
    type_cell(&tab, 0, upd, "2026-10-20 08:30:00-03:00", cx).await;
    commit(&tab, cx).await;
    let got = sql(&core, &pg, &ppw, "SELECT due::text, to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI') FROM dataman_scratch.documents WHERE id = 1", cx).await;
    r.check(
        "dates typed freely are written (a timestamptz with its offset)",
        got.as_ref().ok().and_then(|g| g.first().cloned()) == Some(vec![Some("2026-11-20".into()), Some("2026-10-20 11:30".into())]),
        format!("{got:?}"),
    );

    let (ms, mpw) = profile(Engine::SqlServer);
    let up = connect(ws, &ms, &mpw, cx).await;
    if r.check("SQL Server connects", up.is_ok(), format!("{up:?}"))
        && let Some(tab) = open_table(ws, &ms, "dbo", "documents", 7, cx).await
    {
        let upd = col(&tab, "updated_at", cx);
        type_cell(&tab, 1, upd, "2026-12-24T18:45:00", cx).await;
        commit(&tab, cx).await;
        let got = sql1(&core, &ms, &mpw, "SELECT CONVERT(varchar(19), updated_at, 120) FROM dbo.documents WHERE id = 2", cx).await;
        r.check("SQL Server datetime2: typed with a T, written", got.as_deref() == Some("2026-12-24 18:45:00"), format!("{got:?}"));
    }
}
