//! `sqlite-check` and `duckdb-check`: a database file and no server. The file opens as Finder
//! would hand it over, the sheet fills itself in, then the explorer, the table's grid, an edit
//! saved, a query and its plan in the editor, and the disconnect.

use gpui::{AsyncWindowContext, Entity};
use dataman_core::config::{ConnectionProfile, Engine};
use dataman_core::edit::CellValue;
use dataman_core::result::Phase;

use crate::app::Workspace;
use crate::check::{Report, cell, connected, core, draw, pause, result, run_keys, sql1, state, until};
use crate::tab::QueryTab;
use crate::tree_menu::Act;

/// What differs between the two engines along the same path.
pub struct Case {
    engine: Engine,
    seed: &'static str,
    ext: &'static str,
    /// The file's database (SQLite's is `main`; DuckDB's is named after the file).
    db: &'static str,
    explain: &'static str,
}

pub const SQLITE: Case = Case { engine: Engine::Sqlite, seed: include_str!("../../../seeds/sqlite.sql"), ext: "sqlite", db: "main", explain: "EXPLAIN QUERY PLAN SELECT" };
pub const DUCKDB: Case = Case { engine: Engine::DuckDb, seed: include_str!("../../../seeds/duckdb.sql"), ext: "duckdb", db: "shop", explain: "EXPLAIN SELECT" };

fn dialog(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Entity<crate::conn_dialog::ConnDialog>> {
    cx.update(|_, cx| ws.read(cx).dialog.clone()).ok().flatten()
}

fn profiles(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Vec<ConnectionProfile> {
    cx.update(|_, cx| ws.read(cx).store.read(cx).profiles.clone()).unwrap_or_default()
}

fn col(tab: &Entity<QueryTab>, name: &str, cx: &mut AsyncWindowContext) -> Option<usize> {
    cx.update(|_, cx| tab.read(cx).result().and_then(|r| r.columns()).and_then(|c| c.iter().position(|c| c.name == name))).ok().flatten()
}

async fn table_page(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
    until(cx, 20, |cx| {
        cx.update(|_, cx| {
            let t = tab.read(cx);
            t.shows_table() && t.result().is_some_and(|r| r.phase().is_finished()) && !t.is_running()
        })
        .unwrap_or(true)
    })
    .await;
    draw(cx);
}

fn open_url(ws: &Entity<Workspace>, path: &std::path::Path, cx: &mut AsyncWindowContext) {
    let url = format!("file://{}", path.display());
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_urls(vec![url], window, cx)));
}

pub async fn run(ws: &Entity<Workspace>, case: &Case, r: &mut Report, cx: &mut AsyncWindowContext) {
    let label = case.engine.label();
    let dir = std::env::temp_dir().join(format!("dataman-{}-check-{}", case.engine.slug(), std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("shop.{}", case.ext));
    let _ = dataman_core::import::create_database(case.engine, &file);
    let core = core(ws, cx);
    let direct = ConnectionProfile::new_file(case.engine, file.to_string_lossy());
    sql1(&core, &direct, "", case.seed, cx).await;
    let seeded = sql1(&core, &direct, "", "SELECT count(*) FROM customer", cx).await;
    r.check("the file is seeded through a connection of the check's own", seeded.as_deref() == Some("3"), format!("{seeded:?}"));

    // As Finder (or `dataman shop.sqlite`) hands it over: the sheet, filled in.
    let before = profiles(ws, cx).len();
    open_url(ws, &file, cx);
    pause(cx, 300).await;
    let Some(d) = dialog(ws, cx) else {
        r.check(&format!("opening a .{} file shows the connection sheet", case.ext), false, "no sheet");
        return;
    };
    let (p, _) = cx.update(|_, cx| d.read(cx).current(cx)).unwrap();
    // The app keeps the file's canonical path (`/private/var/…` for `/var/…` on macOS).
    let canonical = std::fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
    let shown = dataman_core::config::tilde(&canonical.to_string_lossy());
    r.check(
        &format!("the sheet is a {label} one, with the file and its name"),
        p.engine == case.engine && p.path.as_deref() == Some(shown.as_str()) && p.name == "shop" && p.host.is_empty(),
        format!("{:?} {:?} {:?} {:?}", p.engine, p.path, p.name, p.host),
    );
    draw(cx);
    let probes: Vec<bool> = ["file-browse", "file-new"].iter().map(|id| crate::check::probed(id, cx).is_some()).collect();
    let server: Vec<bool> = ["tls-prefer", "entry-URL", "save-pw"].iter().map(|id| crate::check::probed(id, cx).is_some()).collect();
    r.check("Browse… and New… are there; TLS, URL and the keychain are not", probes == [true, true] && server == [false, false, false], format!("{probes:?} {server:?}"));
    let access = crate::check::probed("file-access", cx).is_some();
    r.check("the file access option is DuckDB's", access == (case.engine == Engine::DuckDb), access);
    let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_test(cx)));
    let tested = until(cx, 10, |cx| cx.update(|_, cx| d.read(cx).demo_status(cx).0 != "testing").unwrap_or(true)).await;
    let status = cx.update(|_, cx| d.read(cx).demo_status(cx).0).unwrap_or_default();
    r.check("Test opens the file", tested && status == "ok", status);
    let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_save(cx)));
    pause(cx, 300).await;
    let saved = profiles(ws, cx).into_iter().find(|p| p.engine == case.engine && p.path.as_deref() == Some(shown.as_str()));
    let Some(id) = saved.map(|p| p.id) else {
        r.check("Save & connect keeps the connection", false, format!("{} profiles", profiles(ws, cx).len()));
        return;
    };
    let up = connected(ws, &id, 20, cx).await;
    r.check("Save & connect connects", up.is_ok(), format!("{up:?}"));

    // The same file again: its connection, no second one.
    open_url(ws, &file, cx);
    pause(cx, 300).await;
    let again = profiles(ws, cx).len();
    r.check("opening the file again reuses its connection", again == before + 1 && dialog(ws, cx).is_none(), format!("{before} → {again}"));
    let text = dir.join("notes.db");
    let _ = std::fs::write(&text, "this is a text file named like a database, nothing more");
    open_url(ws, &text, cx);
    pause(cx, 200).await;
    r.check("a .db that is no database is not offered as one", dialog(ws, cx).is_none() && profiles(ws, cx).len() == again, "");

    // The explorer: tables, views and columns (SQLite flat, DuckDB with its schemas).
    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    let db = case.db;
    let s = format!("c:{id}/d:{db}/s:main");
    for node in [format!("c:{id}"), format!("c:{id}/d:{db}"), s.clone(), format!("{s}/tables"), format!("{s}/r:customer")] {
        until(cx, 5, |cx| cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&node, cx))).unwrap_or(false)).await;
        pause(cx, 200).await;
    }
    let mut ids = vec![format!("{s}/r:customer"), format!("{s}/r:orders"), format!("{s}/views"), format!("{s}/r:customer/cols/name")];
    if case.engine == Engine::DuckDb {
        ids.push(format!("c:{id}/d:{db}/s:sales"));
    }
    let seen: Vec<bool> = cx.update(|_, cx| ids.iter().map(|i| tree.read(cx).has_row(i)).collect()).unwrap_or_default();
    r.check("the explorer lists the tables, the views and the columns", seen.iter().all(|x| *x), format!("{seen:?}"));
    let menu = |node: String, cx: &mut AsyncWindowContext| -> Vec<Act> {
        let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.demo_menu(&node, false, false, window, cx)));
        let acts = cx.update(|_, cx| tree.read(cx).demo_menu_acts()).unwrap_or_default();
        let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.close_menu(false, window, cx)));
        acts
    };
    let conn_acts = menu(format!("c:{id}"), cx);
    let db_acts = menu(format!("c:{id}/d:{db}"), cx);
    r.check(
        "the connection's menu reveals the file; nothing drops the database or monitors activity",
        conn_acts.contains(&Act::RevealFile) && !conn_acts.contains(&Act::Activity) && !db_acts.contains(&Act::DropDatabase),
        format!("{conn_acts:?} {db_acts:?}"),
    );

    // The table's grid, an edit, saved.
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_relation(&id, db, "main", "customer", window, cx)));
    let Some(t) = crate::check::active(ws, cx) else {
        r.check("the table opens in a tab", false, "no tab");
        return;
    };
    table_page(&t, cx).await;
    let (phase, rows, err) = result(&t, cx);
    let sql = cx.update(|_, cx| t.read(cx).editor.read(cx).text().to_string()).unwrap_or_default();
    r.check("the table opens with its 3 rows", phase == Some(Phase::Done) && rows == 3 && sql.contains("FROM main.customer"), format!("{phase:?} {rows} {err} {sql}"));
    let Some(name) = col(&t, "name", cx) else {
        r.check("the grid has the name column", false, "");
        return;
    };
    // SQLite's label is generated (read-only); DuckDB's id is its key, editable still.
    let other = col(&t, if case.engine == Engine::Sqlite { "label" } else { "id" }, cx).unwrap_or(0);
    let edit = cx.update(|_, cx| t.read(cx).edit.clone()).unwrap();
    // The target is resolved with the first edit.
    let _ = cx.update(|window, cx| edit.update(cx, |e, cx| e.set_cell(1, name, CellValue::text("Bruno C. ü"), window, cx)));
    until(cx, 5, |cx| cx.update(|_, cx| edit.read(cx).pending_count()).unwrap_or(0) == 1).await;
    let editable = cx.update(|_, cx| (edit.read(cx).editable(name), edit.read(cx).editable(other))).ok();
    let want = if case.engine == Engine::Sqlite { Some((Some(true), Some(false))) } else { Some((Some(true), Some(true))) };
    r.check("the grid's edits follow the table (a generated column is read-only)", editable == want, format!("{editable:?}"));
    let plan = cx.update(|_, cx| edit.read(cx).plan()).ok().and_then(|p| p.ok()).map(|p| dataman_core::edit::preview(&p)).unwrap_or_default();
    r.check("the edit's UPDATE is keyed by id", plan.contains("UPDATE main.customer SET name = 'Bruno C. ü' WHERE id = 2"), plan.clone());
    let _ = cx.update(|_, cx| edit.update(cx, |e, cx| e.apply(cx)));
    until(cx, 10, |cx| cx.update(|_, cx| edit.read(cx).pending_count()).unwrap_or(1) == 0).await;
    pause(cx, 200).await;
    let now = sql1(&core, &direct, "", "SELECT name FROM customer WHERE id = 2", cx).await;
    r.check("the edit is in the file", now.as_deref() == Some("Bruno C. ü"), format!("{now:?}"));

    // A query in the editor, then its plan.
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.new_tab(window, cx)));
    pause(cx, 100).await;
    let q = run_keys(ws, "SELECT status, count(*) AS n FROM orders GROUP BY status ORDER BY status", "cmd-enter", cx).await;
    let Some(q) = q else {
        r.check("a query tab runs", false, "no tab");
        return;
    };
    let got = (cell(&q, 0, 0, cx), cell(&q, 0, 1, cx), result(&q, cx).1);
    r.check("the editor's query runs on the file", got == (Some("new".into()), Some("1".into()), 2), format!("{got:?}"));
    let dialect = cx.update(|_, cx| q.read(cx).dialect()).ok();
    r.check(&format!("the editor reads {label}"), dialect == Some(case.engine.dialect()), format!("{dialect:?}"));
    let _ = cx.update(|_, cx| q.update(cx, |t, cx| t.explain(false, cx)));
    until(cx, 10, |cx| crate::check::idle(&q, cx)).await;
    pause(cx, 300).await;
    let explained = cx.update(|_, cx| q.read(cx).result().map(|r| r.sql.clone())).ok().flatten().unwrap_or_default();
    let plan_shown = cx.update(|_, cx| q.read(cx).shows_plan(cx)).unwrap_or(false);
    r.check(&format!("Explain shows {label}'s plan"), explained.starts_with(case.explain) && plan_shown, format!("{explained} {plan_shown}"));

    // Disconnected, the file is free again.
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.request_disconnect(id.clone(), false, window, cx)));
    let off = until(cx, 5, |cx| state(ws, &id, cx).is_none()).await;
    r.check("Disconnect closes the file", off, format!("{:?}", state(ws, &id, cx).is_some()));
    let _ = std::fs::remove_dir_all(&dir);
}
