//! Demo stages of a MySQL connection (`--demo`), against the `dataman-mysql` test container
//! (`bench::mysql_profile`; load `seeds/mysql.sql` first):
//!
//! - `mysql`: the explorer open on the database (no schema level) and `customer` as data
//!   (screenshot);
//! - `mysql-dialog`: the connection sheet on that connection, MySQL picked (screenshot);
//! - `mysql-check`: the rules with real keys and clicks where they matter (connect, the
//!   explorer, a table's pages, filter and sort, a grid edit committed, the enum labels, an FK
//!   jump, a script with DELIMITER, a syntax error's line, the cancel of `SELECT SLEEP(30)`,
//!   a typed START TRANSACTION and its commit, and what MySQL hides), printed as PASS/FAIL lines; exit code 1 on a failure.

use std::time::{Duration, Instant};

use gpui::{AsyncWindowContext, Entity, Focusable, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput, Pixels, Point};
use dataman_core::edit::CellValue;
use dataman_core::result::Phase;
use dataman_core::sql::Dialect;
use dataman_core::tx::TxStatus;

use crate::app::Workspace;
use crate::tab::QueryTab;
use crate::tree_menu::Act;

pub fn is_stage(s: &str) -> bool {
    s.starts_with("mysql")
}

fn input(cx: &mut AsyncWindowContext, e: PlatformInput) {
    let _ = cx.update(|window, cx| {
        window.dispatch_event(e, cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn key(cx: &mut AsyncWindowContext, k: &str) {
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(Keystroke::parse(k).unwrap(), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn click(cx: &mut AsyncWindowContext, p: Point<Pixels>, modifiers: Modifiers) {
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: p, modifiers, click_count: 1, first_mouse: false }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: p, modifiers, click_count: 1 }));
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn active(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Entity<QueryTab> {
    cx.update(|_, cx| ws.read(cx).active_tab().unwrap().clone()).unwrap()
}

/// Waits until the tab's run is over.
async fn finished(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
    pause(cx, 50).await;
    for _ in 0..400 {
        let done = cx.update(|_, cx| {
            let t = tab.read(cx);
            !t.is_running() && t.run_phase().is_none_or(|p| p.is_finished())
        });
        if done.unwrap_or(true) {
            break;
        }
        pause(cx, 25).await;
    }
    draw(cx);
}

fn sql(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> String {
    cx.update(|_, cx| tab.read(cx).editor.read(cx).text().to_string()).unwrap_or_default()
}

fn col(tab: &Entity<QueryTab>, name: &str, cx: &mut AsyncWindowContext) -> usize {
    cx.update(|_, cx| tab.read(cx).result().and_then(|r| r.columns()).and_then(|c| c.iter().position(|c| c.name == name)))
        .ok()
        .flatten()
        .unwrap_or(0)
}

fn cell(tab: &Entity<QueryTab>, row: usize, col: usize, cx: &mut AsyncWindowContext) -> Option<String> {
    cx.update(|_, cx| tab.read(cx).result().and_then(|r| r.snapshot().text(row, col).map(|v| v.into_owned()))).ok().flatten()
}

/// Runs `sql` in the active tab (typed into its editor) and waits for it.
async fn run_in(tab: &Entity<QueryTab>, text: &str, script: bool, cx: &mut AsyncWindowContext) {
    let before = cx.update(|_, cx| tab.read(cx).result().cloned()).ok().flatten();
    let _ = cx.update(|window, cx| {
        let editor = tab.read(cx).editor.clone();
        editor.update(cx, |e, cx| {
            e.set_text(text, cx);
            if script {
                e.run_script(cx);
            } else {
                e.run_statement(window, cx);
            }
        });
    });
    // A new result first (the old one is finished too).
    for _ in 0..200 {
        let new = cx.update(|_, cx| tab.read(cx).result().is_some_and(|r| before.as_ref().is_none_or(|b| !std::sync::Arc::ptr_eq(b, r)))).unwrap_or(true);
        if new {
            break;
        }
        pause(cx, 25).await;
    }
    finished(tab, cx).await;
}

/// Opens `db.table` as data (the explorer's ↩) and waits for its first page.
async fn open(ws: &Entity<Workspace>, conn: &str, db: &str, table: &str, cx: &mut AsyncWindowContext) -> Entity<QueryTab> {
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_relation(conn, db, db, table, window, cx)));
    let tab = active(ws, cx);
    page(&tab, cx).await;
    tab
}

/// Waits until the tab shows a finished page of its table.
async fn page(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
    pause(cx, 50).await;
    for _ in 0..400 {
        let done = cx.update(|_, cx| {
            let t = tab.read(cx);
            t.shows_table() && t.result().is_some_and(|r| r.phase().is_finished()) && !t.is_running()
        });
        if done.unwrap_or(true) {
            break;
        }
        pause(cx, 25).await;
    }
    draw(cx);
}

/// A statement on a session of its own (setup and verification), its first cell.
async fn direct(ws: &Entity<Workspace>, sql: &str, cx: &mut AsyncWindowContext) -> Option<String> {
    let conn = cx.update(|_, cx| ws.read(cx).active_tab().and_then(|t| t.read(cx).connection().cloned())).ok().flatten()?;
    let rs = conn.session(None).execute(sql.to_string());
    rs.fetch_all();
    for _ in 0..400 {
        if rs.phase().is_finished() {
            break;
        }
        pause(cx, 25).await;
    }
    if rs.phase() != Phase::Done {
        eprintln!("mysql check: {sql}: {:?}", rs.status().error);
    }
    let snap = rs.snapshot();
    (!snap.columns.is_empty() && snap.rows() > 0).then(|| snap.text(0, 0).map(|v| v.into_owned())).flatten()
}

/// Opens the explorer down to the database's tables.
async fn explore(ws: &Entity<Workspace>, conn: &str, db: &str, cx: &mut AsyncWindowContext) {
    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    for node in [format!("c:{conn}/d:{db}/s:{db}/tables"), format!("c:{conn}/d:{db}/s:{db}/r:customer")] {
        for _ in 0..60 {
            if cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&node, cx))).unwrap_or(false) {
                break;
            }
            pause(cx, 50).await;
        }
        pause(cx, 300).await;
    }
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, conn: &str, db: &str, cx: &mut AsyncWindowContext) -> bool {
    match stage {
        "mysql-dialog" => {
            let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dialog(Some(conn.to_string()), window, cx)));
            true
        }
        "mysql-check" => check(ws, conn, db, cx).await,
        _ => {
            explore(ws, conn, db, cx).await;
            let tab = open(ws, conn, db, "customer", cx).await;
            let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
            let balance = col(&tab, "balance", cx);
            let _ = cx.update(|window, cx| {
                grid.update(cx, |g, cx| g.select_cells(1..=1, balance, cx));
                window.focus(&grid.focus_handle(cx), cx);
            });
            true
        }
    }
}

async fn check(ws: &Entity<Workspace>, conn: &str, db: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    // 1. Connected: the engine, the version, the dialect.
    let tab = active(ws, cx);
    let info = cx.update(|_, cx| tab.read(cx).connection().map(|c| (c.engine(), c.info.version.clone(), c.dialect()))).ok().flatten();
    report(
        "connected to MySQL",
        info.as_ref().is_some_and(|(e, v, d)| *e == dataman_core::config::Engine::MySql && v.starts_with("MySQL ") && *d == Dialect::MySql),
        format!("{info:?}"),
    );

    // 2. The explorer: database → tables, no schema level.
    explore(ws, conn, db, cx).await;
    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    let rows = |cx: &mut AsyncWindowContext, ids: &[String]| cx.update(|_, cx| ids.iter().map(|i| tree.read(cx).has_row(i)).collect::<Vec<_>>()).unwrap();
    let ids = [
        format!("c:{conn}/d:{db}"),
        format!("c:{conn}/d:{db}/s:{db}"),
        format!("c:{conn}/d:{db}/s:{db}/tables"),
        format!("c:{conn}/d:{db}/s:{db}/views"),
        format!("c:{conn}/d:{db}/s:{db}/functions"),
        format!("c:{conn}/d:{db}/s:{db}/triggers"),
        format!("c:{conn}/d:{db}/s:{db}/r:orders"),
        format!("c:{conn}/d:{db}/s:{db}/r:customer/cols/id"),
    ];
    let seen = rows(cx, &ids);
    report("the database row is there", seen[0], format!("{seen:?}"));
    report("no schema level under it", !seen[1], format!("{seen:?}"));
    report("tables, views, routines and triggers right under the database", seen[2..6].iter().all(|x| *x), format!("{seen:?}"));
    report("tables and their columns load", seen[6] && seen[7], format!("{seen:?}"));
    // What MySQL cannot do is not in the menus.
    let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.demo_menu(&format!("c:{conn}"), false, false, window, cx)));
    let conn_acts = cx.update(|_, cx| tree.read(cx).demo_menu_acts()).unwrap_or_default();
    let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.close_menu(false, window, cx)));
    let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.demo_menu(&format!("c:{conn}/d:{db}/s:{db}/r:orders"), false, false, window, cx)));
    let rel_acts = cx.update(|_, cx| tree.read(cx).demo_menu_acts()).unwrap_or_default();
    let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.close_menu(false, window, cx)));
    report(
        "menus hide the activity monitor, imports and privileges",
        !conn_acts.contains(&Act::Activity) && !rel_acts.contains(&Act::ImportData) && !rel_acts.contains(&Act::Privileges) && rel_acts.contains(&Act::Export),
        format!("{conn_acts:?} {rel_acts:?}"),
    );
    draw(cx);
    let menu_caps = cx.update(|_, cx| cx.try_global::<crate::MenuCaps>().map(|m| m.0)).ok().flatten();
    report("the menu bar leaves out Explain and the activity monitor", menu_caps.is_some_and(|c| !c.explain && !c.activity && !c.import), format!("{menu_caps:?}"));

    // 3. A table as data: the page's SELECT, the estimate, MySQL's quoting.
    let t = open(ws, conn, db, "customer", cx).await;
    let s = sql(&t, cx);
    report("the data view's SELECT", s == format!("SELECT * FROM {db}.customer\n ORDER BY id\n LIMIT 1000;"), s.clone());
    let n = cx.update(|_, cx| t.read(cx).result().map_or(0, |r| r.rows())).unwrap_or(0);
    report("the page has the table's 5 rows", n == 5, n.to_string());
    let (dialect_ok, editor_dialect) = cx.update(|_, cx| (t.read(cx).dialect() == Dialect::MySql, t.read(cx).editor.read(cx).demo_cursor().1)).unwrap();
    report("the editor reads MySQL", dialect_ok && editor_dialect == Dialect::MySql, format!("{editor_dialect:?}"));
    let mut est = None;
    for _ in 0..60 {
        est = cx.update(|_, cx| t.read(cx).table.as_ref().and_then(|d| d.estimate)).ok().flatten();
        if est.is_some() {
            break;
        }
        pause(cx, 50).await;
    }
    report("the estimate comes from TABLE_ROWS", est.is_some(), format!("{est:?}"));

    // Filter and sort on the server.
    let tier = col(&t, "tier", cx);
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.filter_value(tier, Some("silver".into()), cx)));
    page(&t, cx).await;
    let s = sql(&t, cx);
    let rows = cx.update(|_, cx| t.read(cx).result().map_or(0, |r| r.rows())).unwrap_or(0);
    report("filter by value on an ENUM", s.contains("\n WHERE tier = 'silver'\n") && rows == 2, format!("{s} {rows}"));
    let balance = col(&t, "balance", cx);
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.table_sort(balance, Some(dataman_core::result::SortDir::Desc), cx)));
    page(&t, cx).await;
    let s = sql(&t, cx);
    report("server-side sort", s.contains("ORDER BY balance DESC") && cell(&t, 0, 1, cx).as_deref() == Some("Ben Lewis"), s.clone());

    // Paging: a table of 2,500 rows, ⌥⌘→.
    direct(ws, "DROP TABLE IF EXISTS chk_many", cx).await;
    direct(ws, "CREATE TABLE chk_many (id INT PRIMARY KEY, v VARCHAR(10))", cx).await;
    direct(
        ws,
        "INSERT INTO chk_many WITH RECURSIVE g(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM g WHERE n < 49) \
         SELECT a.n * 50 + b.n + 1, CONCAT('v', a.n * 50 + b.n + 1) FROM g a CROSS JOIN g b",
        cx,
    )
    .await;
    let p = open(ws, conn, db, "chk_many", cx).await;
    let grid = cx.update(|_, cx| p.read(cx).grid.clone()).unwrap();
    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
    key(cx, "cmd-alt-right");
    page(&p, cx).await;
    let s = sql(&p, cx);
    report("⌥⌘→ pages with LIMIT … OFFSET", s.ends_with("LIMIT 1000 OFFSET 1000;") && cell(&p, 0, 0, cx).as_deref() == Some("1001"), s.clone());
    let mut total = None;
    let _ = cx.update(|_, cx| p.update(cx, |t, cx| t.start_count(cx)));
    for _ in 0..200 {
        total = cx.update(|_, cx| p.read(cx).table.as_ref().filter(|d| d.counting.is_none()).and_then(|d| d.total)).ok().flatten();
        if total.is_some() {
            break;
        }
        pause(cx, 50).await;
    }
    report("count(*) on its own connection", total == Some(2500), format!("{total:?}"));

    // 4. A grid edit committed in a transaction (MySQL re-reads by key: no RETURNING).
    let t = open(ws, conn, db, "customer", cx).await;
    let name = col(&t, "name", cx);
    let tier = col(&t, "tier", cx);
    let edit = cx.update(|_, cx| t.read(cx).edit.clone()).unwrap();
    let _ = cx.update(|window, cx| edit.update(cx, |e, cx| e.set_cell(1, name, CellValue::text("Ben \\ O'Hara"), window, cx)));
    for _ in 0..100 {
        if cx.update(|_, cx| edit.read(cx).pending_count()).unwrap_or(0) == 1 {
            break;
        }
        pause(cx, 50).await;
    }
    let labels = cx.update(|_, cx| edit.read(cx).enum_labels(tier).map(|l| l.as_ref().clone())).ok().flatten();
    report("the enum editor gets the ENUM's labels", labels.as_deref() == Some(&["bronze".to_string(), "silver".into(), "gold".into()][..]), format!("{labels:?}"));
    let plan = cx.update(|_, cx| edit.read(cx).plan()).ok().and_then(|p| p.ok()).map(|p| dataman_core::edit::preview(&p)).unwrap_or_default();
    report("the edit's UPDATE is MySQL's", plan.contains(&format!("UPDATE {db}.customer SET name = 'Ben \\\\ O''Hara' WHERE id = 2 AND name <=> 'Ben Lewis'")), plan.clone());
    let _ = cx.update(|_, cx| edit.update(cx, |e, cx| e.apply(cx)));
    for _ in 0..100 {
        if cx.update(|_, cx| edit.read(cx).pending_count()).unwrap_or(1) == 0 {
            break;
        }
        pause(cx, 50).await;
    }
    pause(cx, 200).await;
    page(&t, cx).await;
    let saved = direct(ws, "SELECT name FROM customer WHERE id = 2", cx).await;
    report("the edit is committed", saved.as_deref() == Some("Ben \\ O'Hara"), format!("{saved:?}"));
    report("the tab shows the row again", cell(&t, 1, name, cx).as_deref() == Some("Ben \\ O'Hara"), format!("{:?}", cell(&t, 1, name, cx)));

    // 5. A foreign key: ⌘-click on orders.customer_id opens the customer.
    let q = active(ws, cx);
    run_in(&q, "SELECT * FROM orders ORDER BY id", false, cx).await;
    let grid = cx.update(|_, cx| q.read(cx).grid.clone()).unwrap();
    let fk = col(&q, "customer_id", cx);
    let mut is_fk = false;
    for _ in 0..60 {
        is_fk = cx.update(|_, cx| grid.read(cx).is_fk_column(fk)).unwrap_or(false);
        if is_fk {
            break;
        }
        pause(cx, 50).await;
    }
    report("customer_id is a reference", is_fk, String::new());
    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
    draw(cx);
    let at = cx.update(|_, cx| grid.read(cx).demo_cell_point(2, fk)).unwrap();
    click(cx, at, Modifiers { platform: true, ..Default::default() });
    let landed = active(ws, cx);
    finished(&landed, cx).await;
    let s = sql(&landed, cx);
    report("⌘-click jumps to the referenced row", s == format!("SELECT * FROM {db}.customer WHERE id = 2;") && cell(&landed, 0, 0, cx).as_deref() == Some("2"), s.clone());

    // 6. A script with DELIMITER: the procedure's body keeps its `;`.
    direct(ws, "DROP PROCEDURE IF EXISTS chk_two", cx).await;
    let script = "# a procedure, then its call\nSET @n := 1;\nDELIMITER //\nCREATE PROCEDURE chk_two()\nBEGIN\n  SELECT 'one' AS a; -- first\n  SELECT 'two' AS b;\nEND //\nDELIMITER ;\nCALL chk_two();";
    run_in(&landed, script, true, cx).await;
    let items = cx.update(|_, cx| landed.read(cx).batch_items().iter().map(|b| (b.sql.clone(), b.rs.as_ref().map(|r| (r.phase(), r.rows())))).collect::<Vec<_>>()).unwrap();
    let fine = items.len() == 3 && items.iter().all(|(_, r)| r.is_some_and(|(p, _)| p == Phase::Done)) && items[2].1.is_some_and(|(_, n)| n == 1) && items[1].0.ends_with("END");
    report("a DELIMITER script runs as 3 statements", fine, format!("{items:?}"));

    // 7. A syntax error lands on its line.
    let bad = "SELECT 1,\n       2\n  FROM WHERE x = 1";
    run_in(&landed, bad, false, cx).await;
    let editor = cx.update(|_, cx| landed.read(cx).editor.clone()).unwrap();
    let jumped = cx.update(|_, cx| editor.update(cx, |e, cx| e.jump_to_error(cx))).unwrap_or(false);
    let at = cx.update(|_, cx| editor.read(cx).demo_cursor().0).unwrap_or(0);
    let err = cx.update(|_, cx| landed.read(cx).result().and_then(|r| r.status().error)).ok().flatten();
    report(
        "the error's near '…' at line N lands on its token",
        jumped && bad[at..].starts_with("WHERE") && err.as_ref().and_then(|e| e.code.as_deref()) == Some("1064"),
        format!("{at} {err:?}"),
    );

    // 8. ⌘. cancels a running SLEEP (KILL QUERY on a connection of its own).
    let _ = cx.update(|window, cx| {
        let editor = landed.read(cx).editor.clone();
        editor.update(cx, |e, cx| {
            e.set_text("SELECT SLEEP(30)", cx);
            e.run_statement(window, cx);
        });
        window.focus(&editor.focus_handle(cx), cx);
    });
    pause(cx, 400).await;
    let t0 = Instant::now();
    key(cx, "cmd-.");
    finished(&landed, cx).await;
    let phase = cx.update(|_, cx| landed.read(cx).run_phase()).ok().flatten();
    report("⌘. stops SELECT SLEEP(30) at once", phase == Some(Phase::Cancelled) && t0.elapsed() < Duration::from_secs(3), format!("{phase:?} {:?}", t0.elapsed()));

    // 9. A typed START TRANSACTION: the transaction badge from the server's status flags.
    run_in(&landed, "START TRANSACTION", false, cx).await;
    run_in(&landed, "UPDATE customer SET balance = balance + 1 WHERE id = 3", false, cx).await;
    let open_tx = cx.update(|_, cx| landed.read(cx).tx().status).ok();
    let other = direct(ws, "SELECT balance FROM customer WHERE id = 3", cx).await;
    report("START TRANSACTION keeps the transaction open", open_tx == Some(TxStatus::Open) && other.as_deref() == Some("0.00"), format!("{open_tx:?} {other:?}"));
    let end = cx.update(|_, cx| landed.update(cx, |t, cx| t.end_tx(true, cx))).ok();
    if let Some(end) = end {
        let _ = end.await;
    }
    pause(cx, 100).await;
    let after = cx.update(|_, cx| landed.read(cx).tx().status).ok();
    let saved = direct(ws, "SELECT balance FROM customer WHERE id = 3", cx).await;
    report("Commit ends it and the change is saved", after == Some(TxStatus::Idle) && saved.as_deref() == Some("1.00"), format!("{after:?} {saved:?}"));
    direct(ws, "DROP TABLE IF EXISTS chk_many", cx).await;
    direct(ws, "DROP PROCEDURE IF EXISTS chk_two", cx).await;
    ok
}
