//! `clickhouse-check`: a server with no transactions and no row keys, in the real app. The picker
//! finds it, the explorer has no schema level, the table opens in its grid and stays read-only,
//! a query's cells and a mid-result failure show, and a read-only database is refused by the server.

use gpui::{AsyncWindowContext, Entity};
use dataman_core::config::{ConnectionProfile, Engine};
use dataman_core::result::Phase;
use dataman_core::sql::{Dialect, split_in};

use crate::app::Workspace;
use crate::check::{Report, active, cell, connect, core, draw, profile, result, run_keys, sql1, until};
use crate::tree_menu::Act;

const SEED: &str = include_str!("../../../seeds/clickhouse.sql");

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (prof, pw) = profile(Engine::ClickHouse);
    let (id, db) = (prof.id.clone(), prof.database.clone());
    let core = core(ws, cx);
    let admin = ConnectionProfile { database: String::new(), ..prof.clone() };
    for s in split_in(SEED, Dialect::ClickHouse) {
        sql1(&core, &admin, &pw, &SEED[s.range], cx).await;
    }
    let seeded = sql1(&core, &admin, &pw, "SELECT count() FROM dataman_ch.events", cx).await;
    r.check("the seed loads through a connection of the check's own", seeded.as_deref() == Some("3"), format!("{seeded:?}"));

    let up = connect(ws, &prof, &pw, cx).await;
    r.check("it connects", up.is_ok(), format!("{up:?}"));
    if up.is_err() {
        return;
    }

    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    let s = format!("c:{id}/d:{db}/s:{db}");
    for node in [format!("c:{id}"), format!("c:{id}/d:{db}"), format!("{s}/tables"), format!("{s}/r:events")] {
        until(cx, 5, |cx| cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&node, cx))).unwrap_or(false)).await;
        crate::check::pause(cx, 200).await;
    }
    let ids = [format!("c:{id}/d:{db}"), s.clone(), format!("{s}/r:events"), format!("{s}/views"), format!("{s}/r:events/cols/token"), format!("{s}/functions"), format!("{s}/triggers")];
    let seen: Vec<bool> = cx.update(|_, cx| ids.iter().map(|i| tree.read(cx).has_row(i)).collect()).unwrap_or_default();
    r.check("the database row is there, with no schema level under it", seen[0] && !seen[1], format!("{seen:?}"));
    r.check("tables, views and columns load", seen[2] && seen[3] && seen[4], format!("{seen:?}"));
    r.check("no functions or triggers to list", !seen[5] && !seen[6], format!("{seen:?}"));
    let menu = |node: String, cx: &mut AsyncWindowContext| -> Vec<Act> {
        let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.demo_menu(&node, false, false, window, cx)));
        let acts = cx.update(|_, cx| tree.read(cx).demo_menu_acts()).unwrap_or_default();
        let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.close_menu(false, window, cx)));
        acts
    };
    let conn_acts = menu(format!("c:{id}"), cx);
    r.check("nothing offers what the engine has no answer for", !conn_acts.contains(&Act::Activity), format!("{conn_acts:?}"));

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_relation(&id, &db, &db, "events", window, cx)));
    let Some(t) = active(ws, cx) else {
        r.check("the table opens in a tab", false, "no tab");
        return;
    };
    until(cx, 20, |cx| cx.update(|_, cx| t.read(cx).shows_table() && t.read(cx).result().is_some_and(|r| r.phase().is_finished())).unwrap_or(true)).await;
    draw(cx);
    let (phase, rows, err) = result(&t, cx);
    r.check("the table opens with its 3 rows", phase == Some(Phase::Done) && rows == 3, format!("{phase:?} {rows} {err}"));
    let edit = cx.update(|_, cx| t.read(cx).edit.clone()).unwrap();
    let _ = cx.update(|window, cx| edit.update(cx, |e, cx| e.set_cell(0, 2, dataman_core::edit::CellValue::text("x"), window, cx)));
    crate::check::pause(cx, 300).await;
    let pending = cx.update(|_, cx| edit.read(cx).pending_count()).unwrap_or(1);
    r.check("no cell can be edited: a sort key names no row", pending == 0, pending);

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.new_tab(window, cx)));
    let q = run_keys(ws, "SELECT id, user_id, note FROM events ORDER BY id", "cmd-enter", cx).await.expect("a tab");
    let (phase, rows, err) = result(&q, cx);
    r.check("a query runs in the editor", phase == Some(Phase::Done) && rows == 3, format!("{phase:?} {rows} {err}"));
    draw(cx);
    r.check("a number past an i64 shows whole", cell(&q, 2, 0, cx).as_deref() == Some("18446744073709551615"), format!("{:?}", cell(&q, 2, 0, cx)));
    r.check("NULL is NULL", cell(&q, 1, 1, cx).is_none(), format!("{:?}", cell(&q, 1, 1, cx)));
    let note = cell(&q, 1, 2, cx);
    r.check("a cell's escapes are undone", note.as_deref() == Some("tab\there, line\nbreak, back\\slash"), format!("{note:?}"));

    let q = run_keys(ws, "SELECT number, throwIf(number = 2000000, 'boom') FROM numbers(3000000) SETTINGS max_threads = 1", "cmd-enter", cx).await.expect("a tab");
    // The grid asks for what it shows: the failure is further on.
    let _ = cx.update(|_, cx| q.read(cx).result().map(|rs| rs.fetch_all()));
    until(cx, 30, |cx| result(&q, cx).0.is_some_and(Phase::is_finished)).await;
    let (phase, _, err) = result(&q, cx);
    r.check("a failure inside the result says what the server said", phase == Some(Phase::Failed) && err.contains("boom"), format!("{phase:?} {err}"));

    let q = run_keys(ws, "SELEC 1", "cmd-enter", cx).await.expect("a tab");
    let (phase, _, err) = result(&q, cx);
    r.check("a syntax error shows", phase == Some(Phase::Failed) && err.contains("Syntax error"), format!("{phase:?} {err}"));

    let conn = cx.update(|_, cx| q.read(cx).connection().cloned()).ok().flatten();
    if let Some(c) = &conn {
        c.set_read_only_for(&db, true);
    }
    let q = run_keys(ws, "INSERT INTO events (id) VALUES (99)", "cmd-enter", cx).await.expect("a tab");
    let (phase, _, _) = result(&q, cx);
    let count = sql1(&core, &admin, &pw, "SELECT count() FROM dataman_ch.events WHERE id = 99", cx).await;
    r.check("a read-only database writes nothing", phase != Some(Phase::Done) && count.as_deref() == Some("0"), format!("{phase:?} {count:?}"));
    if let Some(c) = &conn {
        c.set_read_only_for(&db, false);
    }
}
