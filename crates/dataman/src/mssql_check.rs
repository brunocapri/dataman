use std::time::{Duration, Instant};

use gpui::{AsyncWindowContext, Entity, Focusable, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput, Pixels, Point};
use dataman_core::batch::Key;
use dataman_core::edit::CellValue;
use dataman_core::result::Phase;
use dataman_core::sql::Dialect;
use dataman_core::tx::TxStatus;

use crate::app::Workspace;
use crate::messages::{Of, Row as MessageRow};
use crate::tab::QueryTab;
use crate::tree_menu::Act;

pub fn is_stage(s: &str) -> bool {
    s.starts_with("mssql")
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

async fn finished(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
    pause(cx, 50).await;
    for _ in 0..400 {
        let done = cx.update(|_, cx| {
            let t = tab.read(cx);
            !t.is_running() && t.run_phase().is_none_or(|p| p.is_finished()) && !t.run_continues()
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
    cx.update(|_, cx| tab.read(cx).result().filter(|r| row < r.rows()).and_then(|r| r.snapshot().text(row, col).map(|v| v.into_owned()))).ok().flatten()
}

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
    for _ in 0..200 {
        let new = cx.update(|_, cx| tab.read(cx).result().is_some_and(|r| before.as_ref().is_none_or(|b| !std::sync::Arc::ptr_eq(b, r)))).unwrap_or(true);
        if new {
            break;
        }
        pause(cx, 25).await;
    }
    finished(tab, cx).await;
}

async fn open(ws: &Entity<Workspace>, conn: &str, db: &str, schema: &str, table: &str, cx: &mut AsyncWindowContext) -> Entity<QueryTab> {
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_relation(conn, db, schema, table, window, cx)));
    let tab = active(ws, cx);
    page(&tab, cx).await;
    tab
}

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
        eprintln!("mssql check: {sql}: {:?}", rs.status().error);
    }
    let snap = rs.snapshot();
    (!snap.columns.is_empty() && snap.rows() > 0).then(|| snap.text(0, 0).map(|v| v.into_owned())).flatten()
}

async fn explore(ws: &Entity<Workspace>, conn: &str, db: &str, cx: &mut AsyncWindowContext) {
    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    let s = format!("c:{conn}/d:{db}/s:dbo");
    for node in [s.clone(), format!("{s}/tables"), format!("{s}/r:customer"), format!("c:{conn}/d:{db}/s:sales")] {
        for _ in 0..60 {
            if cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&node, cx))).unwrap_or(false) {
                break;
            }
            pause(cx, 50).await;
        }
        pause(cx, 300).await;
    }
}

const SCRIPT: &str = "-- two batches, three result sets\nCREATE TABLE #chk (a int, b nvarchar(10));\nINSERT #chk VALUES (1, N'one'), (2, N'two');\nSELECT a, b FROM #chk ORDER BY a;\nSELECT COUNT(*) AS n FROM #chk\nGO\nSELECT N'café' AS c, SYSDATETIMEOFFSET() AS now_at\nGO";

const PRINTS: &str = "DECLARE @i int = 1;\nWHILE @i <= 5\nBEGIN\n    PRINT CONCAT(N'Batch ', @i, N' of 5: ', @i * 200, N' rows');\n    SET @i += 1;\nEND\nPRINT N'Done.';";

pub async fn run(ws: &Entity<Workspace>, stage: &str, conn: &str, db: &str, cx: &mut AsyncWindowContext) -> bool {
    match stage {
        "mssql-dialog" => {
            let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dialog(Some(conn.to_string()), window, cx)));
            true
        }
        "mssql-check" => check(ws, conn, db, cx).await,
        "mssql-script" => {
            explore(ws, conn, db, cx).await;
            let tab = active(ws, cx);
            run_in(&tab, SCRIPT, true, cx).await;
            true
        }
        "mssql-print" | "mssql-print-tab" => {
            explore(ws, conn, db, cx).await;
            let tab = active(ws, cx);
            if stage == "mssql-print" {
                run_in(&tab, PRINTS, true, cx).await;
            } else {
                run_in(&tab, "PRINT N'Reading customers…'\nSELECT id, name, balance FROM dbo.customer ORDER BY id\nPRINT CONCAT(@@ROWCOUNT, ' customers')", true, cx).await;
                let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.select_result(Key::Messages, cx)));
            }
            true
        }
        _ => {
            explore(ws, conn, db, cx).await;
            let tab = open(ws, conn, db, "dbo", "customer", cx).await;
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
    // The edit and transaction checks below commit changes to customers 2 and 3: start from the seed's values.
    direct(ws, "UPDATE dbo.customer SET name = CASE id WHEN 2 THEN N'Ben Lewis' ELSE name END, balance = CASE id WHEN 3 THEN 0.00 ELSE balance END WHERE id IN (2, 3)", cx).await;
    let tab = active(ws, cx);
    let info = cx.update(|_, cx| tab.read(cx).connection().map(|c| (c.engine(), c.info.version.clone(), c.dialect()))).ok().flatten();
    report(
        "connected to SQL Server",
        info.as_ref().is_some_and(|(e, v, d)| *e == dataman_core::config::Engine::SqlServer && v.starts_with("SQL Server ") && *d == Dialect::TSql),
        format!("{info:?}"),
    );

    explore(ws, conn, db, cx).await;
    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    let rows = |cx: &mut AsyncWindowContext, ids: &[String]| cx.update(|_, cx| ids.iter().map(|i| tree.read(cx).has_row(i)).collect::<Vec<_>>()).unwrap();
    let s = format!("c:{conn}/d:{db}/s:dbo");
    let ids = [
        format!("c:{conn}/d:{db}"),
        s.clone(),
        format!("c:{conn}/d:{db}/s:sales"),
        format!("c:{conn}/d:{db}/s:sys"),
        format!("{s}/tables"),
        format!("{s}/views"),
        format!("{s}/functions"),
        format!("{s}/sequences"),
        format!("{s}/types"),
        format!("{s}/triggers"),
        format!("{s}/r:orders"),
        format!("{s}/r:customer/cols/id"),
    ];
    let seen = rows(cx, &ids);
    report("the database row is there", seen[0], format!("{seen:?}"));
    report("its schemas: dbo, sales, sys", seen[1..4].iter().all(|x| *x), format!("{seen:?}"));
    report("tables, views, routines, sequences, types and triggers under dbo", seen[4..10].iter().all(|x| *x), format!("{seen:?}"));
    report("tables and their columns load", seen[10] && seen[11], format!("{seen:?}"));
    let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.demo_menu(&format!("c:{conn}"), false, false, window, cx)));
    let conn_acts = cx.update(|_, cx| tree.read(cx).demo_menu_acts()).unwrap_or_default();
    let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.close_menu(false, window, cx)));
    let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.demo_menu(&format!("{s}/r:orders"), false, false, window, cx)));
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

    let t = open(ws, conn, db, "dbo", "customer", cx).await;
    let s = sql(&t, cx);
    report("the data view's SELECT", s == "SELECT * FROM dbo.customer\n ORDER BY id\n OFFSET 0 ROWS FETCH NEXT 1000 ROWS ONLY;", s.clone());
    let n = cx.update(|_, cx| t.read(cx).result().map_or(0, |r| r.rows())).unwrap_or(0);
    report("the page has the table's 5 rows", n == 5, n.to_string());
    let (dialect_ok, editor_dialect) = cx.update(|_, cx| (t.read(cx).dialect() == Dialect::TSql, t.read(cx).editor.read(cx).demo_cursor().1)).unwrap();
    report("the editor reads T-SQL", dialect_ok && editor_dialect == Dialect::TSql, format!("{editor_dialect:?}"));
    let mut est = None;
    for _ in 0..60 {
        est = cx.update(|_, cx| t.read(cx).table.as_ref().and_then(|d| d.estimate)).ok().flatten();
        if est.is_some() {
            break;
        }
        pause(cx, 50).await;
    }
    report("the estimate comes from sys.partitions", est == Some(5), format!("{est:?}"));

    let tier = col(&t, "tier", cx);
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.filter_value(tier, Some("2".into()), cx)));
    page(&t, cx).await;
    let s = sql(&t, cx);
    let rows = cx.update(|_, cx| t.read(cx).result().map_or(0, |r| r.rows())).unwrap_or(0);
    report("filter by value", s.contains("\n WHERE tier = 2\n") && rows == 2, format!("{s} {rows}"));
    let balance = col(&t, "balance", cx);
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.table_sort(balance, Some(dataman_core::result::SortDir::Desc), cx)));
    page(&t, cx).await;
    let s = sql(&t, cx);
    report("server-side sort", s.contains("ORDER BY balance DESC") && cell(&t, 0, 1, cx).as_deref() == Some("Ben Lewis"), s.clone());

    direct(ws, "DROP TABLE IF EXISTS dbo.chk_many", cx).await;
    direct(ws, "CREATE TABLE dbo.chk_many (id int PRIMARY KEY, v nvarchar(10))", cx).await;
    direct(
        ws,
        "INSERT INTO dbo.chk_many SELECT n, CONCAT(N'v', n) FROM (SELECT TOP (2500) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n \
         FROM sys.all_columns a CROSS JOIN sys.all_columns b) g",
        cx,
    )
    .await;
    let p = open(ws, conn, db, "dbo", "chk_many", cx).await;
    let grid = cx.update(|_, cx| p.read(cx).grid.clone()).unwrap();
    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
    key(cx, "cmd-alt-right");
    page(&p, cx).await;
    let s = sql(&p, cx);
    report("⌥⌘→ pages with OFFSET … FETCH", s.ends_with("OFFSET 1000 ROWS FETCH NEXT 1000 ROWS ONLY;") && cell(&p, 0, 0, cx).as_deref() == Some("1001"), s.clone());
    let mut total = None;
    let _ = cx.update(|_, cx| p.update(cx, |t, cx| t.start_count(cx)));
    for _ in 0..200 {
        total = cx.update(|_, cx| p.read(cx).table.as_ref().filter(|d| d.counting.is_none()).and_then(|d| d.total)).ok().flatten();
        if total.is_some() {
            break;
        }
        pause(cx, 50).await;
    }
    report("COUNT_BIG(*) on its own connection", total == Some(2500), format!("{total:?}"));

    let t = open(ws, conn, db, "dbo", "customer", cx).await;
    let name = col(&t, "name", cx);
    let row_version = col(&t, "row_version", cx);
    let edit = cx.update(|_, cx| t.read(cx).edit.clone()).unwrap();
    let _ = cx.update(|window, cx| edit.update(cx, |e, cx| e.set_cell(1, name, CellValue::text("Ben O'Hara ü"), window, cx)));
    for _ in 0..100 {
        if cx.update(|_, cx| edit.read(cx).pending_count()).unwrap_or(0) == 1 {
            break;
        }
        pause(cx, 50).await;
    }
    let id = col(&t, "id", cx);
    let editable = cx.update(|_, cx| (edit.read(cx).editable(id), edit.read(cx).editable(row_version), edit.read(cx).editable(name))).ok();
    report("identity and rowversion columns are read-only", editable == Some((Some(false), Some(false), Some(true))), format!("{editable:?}"));
    let plan = cx.update(|_, cx| edit.read(cx).plan()).ok().and_then(|p| p.ok()).map(|p| dataman_core::edit::preview(&p)).unwrap_or_default();
    report(
        "the edit's UPDATE is T-SQL's",
        plan.contains("UPDATE dbo.customer SET name = N'Ben O''Hara ü' WHERE id = 2 AND (name = N'Ben Lewis' OR (name IS NULL AND N'Ben Lewis' IS NULL))"),
        plan.clone(),
    );
    let _ = cx.update(|_, cx| edit.update(cx, |e, cx| e.apply(cx)));
    for _ in 0..100 {
        if cx.update(|_, cx| edit.read(cx).pending_count()).unwrap_or(1) == 0 {
            break;
        }
        pause(cx, 50).await;
    }
    pause(cx, 200).await;
    page(&t, cx).await;
    let saved = direct(ws, "SELECT name FROM dbo.customer WHERE id = 2", cx).await;
    report("the edit is committed", saved.as_deref() == Some("Ben O'Hara ü"), format!("{saved:?}"));
    report("the tab shows the row again", cell(&t, 1, name, cx).as_deref() == Some("Ben O'Hara ü"), format!("{:?}", cell(&t, 1, name, cx)));

    let q = active(ws, cx);
    run_in(&q, "DECLARE @n int = 2;\nSET @n = @n + 1;\nSELECT TOP (@n) id FROM dbo.orders ORDER BY id;", false, cx).await;
    let (prompt, rows) = cx.update(|_, cx| (q.read(cx).params.is_some(), q.read(cx).result().map_or(0, |r| r.rows()))).unwrap_or((true, 0));
    report("Run Statement runs a statement with the variables declared above it", !prompt && rows == 3, format!("prompt {prompt}, rows {rows}"));
    run_in(&q, "SELECT * FROM dbo.orders ORDER BY id", false, cx).await;
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
    report("⌘-click jumps to the referenced row", s == "SELECT * FROM dbo.customer WHERE id = 2;" && cell(&landed, 0, 0, cx).as_deref() == Some("2"), s.clone());

    run_in(&landed, SCRIPT, true, cx).await;
    let items = cx
        .update(|_, cx| {
            landed.read(cx).batch_items().iter().map(|b| (b.label.clone().unwrap_or_else(|| b.sql.clone()), b.rs.as_ref().map(|r| (r.phase(), r.rows())))).collect::<Vec<_>>()
        })
        .unwrap();
    let fine = items.len() == 3
        && items.iter().all(|(_, r)| r.is_some_and(|(p, _)| p == Phase::Done))
        && items.iter().map(|(_, r)| r.map_or(0, |(_, n)| n)).collect::<Vec<_>>() == [2, 1, 1]
        && items[0].0 == "SELECT a, b FROM #chk ORDER BY a"
        && items[1].0 == "SELECT COUNT(*) AS n FROM #chk";
    report("two GO batches give three results", fine, format!("{items:?}"));
    let tabs = cx.update(|_, cx| landed.read(cx).result_tabs().into_iter().map(|t| t.label).collect::<Vec<_>>()).unwrap_or_default();
    report("each result set has its tab", tabs.len() == 3, format!("{tabs:?}"));

    let bad = "SELECT 1 AS one\nGO\nSELECT 2,\n       3\n  FROM WHERE x = 1\nGO";
    run_in(&landed, bad, true, cx).await;
    let editor = cx.update(|_, cx| landed.read(cx).editor.clone()).unwrap();
    let jumped = cx.update(|_, cx| editor.update(cx, |e, cx| e.jump_to_error(cx))).unwrap_or(false);
    let at = cx.update(|_, cx| editor.read(cx).demo_cursor().0).unwrap_or(0);
    let err = cx.update(|_, cx| landed.read(cx).result().and_then(|r| r.status().error)).ok().flatten();
    report(
        "the second batch's error lands on its token",
        jumped && bad[at..].starts_with("WHERE x") && err.as_ref().and_then(|e| e.code.as_deref()) == Some("156"),
        format!("{at} {err:?}"),
    );

    let _ = cx.update(|window, cx| {
        let editor = landed.read(cx).editor.clone();
        editor.update(cx, |e, cx| {
            e.set_text("WAITFOR DELAY '00:00:30'", cx);
            e.run_statement(window, cx);
        });
        window.focus(&editor.focus_handle(cx), cx);
    });
    pause(cx, 400).await;
    let t0 = Instant::now();
    key(cx, "cmd-.");
    finished(&landed, cx).await;
    let phase = cx.update(|_, cx| landed.read(cx).run_phase()).ok().flatten();
    report("⌘. stops WAITFOR DELAY '00:00:30' at once", phase == Some(Phase::Cancelled) && t0.elapsed() < Duration::from_secs(3), format!("{phase:?} {:?}", t0.elapsed()));
    run_in(&landed, "SELECT 41 + 1", false, cx).await;
    report("the tab runs on after the cancel", cell(&landed, 0, 0, cx).as_deref() == Some("42"), format!("{:?}", cell(&landed, 0, 0, cx)));

    run_in(&landed, "BEGIN TRANSACTION", false, cx).await;
    run_in(&landed, "UPDATE dbo.customer SET balance = balance + 1 WHERE id = 3", false, cx).await;
    let open_tx = cx.update(|_, cx| landed.read(cx).tx().status).ok();
    let other = direct(ws, "SELECT COUNT(*) FROM dbo.customer WITH (READPAST) WHERE id = 3", cx).await;
    report("BEGIN TRANSACTION keeps the transaction open", open_tx == Some(TxStatus::Open) && other.as_deref() == Some("0"), format!("{open_tx:?} {other:?}"));
    let end = cx.update(|_, cx| landed.update(cx, |t, cx| t.end_tx(true, cx))).ok();
    if let Some(end) = end {
        let _ = end.await;
    }
    pause(cx, 100).await;
    let after = cx.update(|_, cx| landed.read(cx).tx().status).ok();
    let saved = direct(ws, "SELECT balance FROM dbo.customer WHERE id = 3", cx).await;
    report("Commit ends it and the change is saved", after == Some(TxStatus::Idle) && saved.as_deref() == Some("1.00"), format!("{after:?} {saved:?}"));
    run_in(&landed, "BEGIN TRANSACTION", false, cx).await;
    run_in(&landed, "UPDATE dbo.customer SET balance = 99 WHERE id = 3", false, cx).await;
    let end = cx.update(|_, cx| landed.update(cx, |t, cx| t.end_tx(false, cx))).ok();
    if let Some(end) = end {
        let _ = end.await;
    }
    pause(cx, 100).await;
    let after = cx.update(|_, cx| landed.read(cx).tx().status).ok();
    let kept = direct(ws, "SELECT balance FROM dbo.customer WHERE id = 3", cx).await;
    report("Rollback undoes it", after == Some(TxStatus::Idle) && kept.as_deref() == Some("1.00"), format!("{after:?} {kept:?}"));

    run_in(&landed, "PRINT N'hello'\nRAISERROR('step %d', 10, 1, 2)", true, cx).await;
    let log = cx
        .update(|_, cx| (ws.read(cx).messages.of(), landed.read(cx).result().map(|r| r.messages().0.to_vec()), landed.read(cx).result_tabs().len()))
        .ok();
    report(
        "PRINT shows in the grid's place",
        matches!(&log, Some((Some(Of::Log(_)), Some(m), 1)) if m == &["hello", "step 2"]),
        format!("{log:?}"),
    );
    run_in(&landed, "PRINT 'before'; SELECT 7 AS n; PRINT 'after'", true, cx).await;
    let tabs = cx.update(|_, cx| landed.read(cx).result_tabs().into_iter().map(|t| t.key).collect::<Vec<_>>()).unwrap_or_default();
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.select_result(Key::Messages, cx)));
    draw(cx);
    let shown = cx.update(|_, cx| (ws.read(cx).messages.of().is_some_and(|o| matches!(o, Of::Tab(_))), (0..4).map(|i| ws.read(cx).messages.row(i)).collect::<Vec<_>>())).ok();
    report(
        "beside rows, PRINT goes to the messages tab",
        tabs == [Key::Stmt(0), Key::Messages]
            && shown == Some((true, vec![Some(MessageRow::Stmt(0)), Some(MessageRow::Line(0, 0)), Some(MessageRow::Line(0, 1)), None])),
        format!("{tabs:?} {shown:?}"),
    );
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.copy_messages(cx)));
    let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text())).ok().flatten();
    report("Copy messages", copied.as_deref() == Some("before\nafter\n"), format!("{copied:?}"));

    let editor = cx.update(|_, cx| landed.read(cx).editor.clone()).unwrap();
    let lints = async |text: &str, cx: &mut AsyncWindowContext| {
        let _ = cx.update(|_, cx| editor.update(cx, |e, cx| e.set_text(text, cx)));
        pause(cx, 700).await;
        cx.update(|_, cx| editor.read(cx).lints().iter().map(|l| l.message.clone()).collect::<Vec<_>>()).unwrap_or_default()
    };
    let upd = "UPDATE o\nSET o.status = 'paid', -- settled\n    o.created_at = GETUTCDATE()\nFROM dbo.orders o\nJOIN dbo.customer c ON c.id = o.customer_id\nWHERE o.status = 'open'\n  AND c.tier = 0\n  AND o.created_at < DATEADD(MINUTE, -@DelayMinutes, GETUTCDATE());";
    let clean = lints(upd, cx).await;
    let typo = lints(&upd.replace("o.status = 'paid'", "o.sttus = 'paid'"), cx).await;
    report("UPDATE alias … FROM table alias is not an unknown table", clean.is_empty() && typo.len() == 1, format!("{clean:?} {typo:?}"));

    let settled = async |text: &str, want: &dyn Fn(&[String]) -> bool, cx: &mut AsyncWindowContext| {
        let _ = cx.update(|_, cx| editor.update(cx, |e, cx| e.set_text(text, cx)));
        let t0 = std::time::Instant::now();
        let mut got = Vec::new();
        while t0.elapsed() < Duration::from_secs(4) {
            pause(cx, 50).await;
            got = cx.update(|_, cx| editor.read(cx).lints().iter().map(|l| l.message.clone()).collect::<Vec<_>>()).unwrap_or_default();
            if want(&got) {
                break;
            }
        }
        (got, t0.elapsed())
    };
    let (bad, took) = settled("SELECT * FRM dbo.orders",&|m| m.iter().any(|x| x.contains("Incorrect syntax")), cx).await;
    report(
        "a syntax error gets the server's message as a squiggle (PARSEONLY)",
        bad.iter().any(|x| x.contains("Incorrect syntax near 'FRM'")),
        format!("{bad:?} after {took:?}"),
    );
    println!("     server syntax check: squiggle {} ms after the last edit (600 ms pause included)", took.as_millis());
    let (fixed, _) = settled("SELECT * FROM dbo.orders",&|m| m.is_empty(), cx).await;
    pause(cx, 900).await;
    let fixed2 = cx.update(|_, cx| editor.read(cx).lints().len()).unwrap_or(1);
    report("fixed, the squiggle goes", fixed.is_empty() && fixed2 == 0, format!("{fixed:?} {fixed2}"));
    direct(ws, "IF OBJECT_ID('dbo.HierarchyLevels') IS NULL CREATE TABLE dbo.HierarchyLevels (Id uniqueidentifier PRIMARY KEY, ParentLevelId uniqueidentifier NULL, IsDeleted bit NOT NULL DEFAULT 0, PathToRoot nvarchar(4000) NULL, IsRoot bit NOT NULL DEFAULT 0, OrganizationId uniqueidentifier NULL)", cx).await;
    let _ = cx.update(|_, cx| {
        if let Some(c) = landed.read(cx).connection().cloned() {
            c.schema.invalidate(None);
        }
    });
    let cte = "WITH Roots AS (\n    SELECT CAST(RootId AS uniqueidentifier) AS RootId FROM (VALUES\n        ('902ebf67-556c-1b71-4858-3a1e21c5bd74'),\n        ('06d0eb4d-6c89-d5f7-1c2e-3a2009634fba')\n    ) v(RootId)\n),\nByParent AS (\n    SELECT h.Id FROM HierarchyLevels h JOIN Roots r ON r.RootId = h.Id WHERE h.IsDeleted = 0\n    UNION ALL\n    SELECT c.Id FROM HierarchyLevels c JOIN ByParent p ON c.ParentLevelId = p.Id WHERE c.IsDeleted = 0\n),\nByPath AS (\n    SELECT h.Id FROM HierarchyLevels h\n    WHERE h.IsDeleted = 0\n      AND EXISTS (SELECT 1 FROM Roots r WHERE h.PathToRoot LIKE '%' + CONVERT(nvarchar(36), r.RootId) + '%')\n)\nSELECT\n    (SELECT COUNT(DISTINCT Id) FROM ByParent) AS ProgramsByParent,\n    (SELECT COUNT(*) FROM ByPath) AS ProgramsByPath,\n    (SELECT COUNT(*) FROM ByPath WHERE Id NOT IN (SELECT Id FROM ByParent)) AS OnlyInPath,\n    (SELECT COUNT(*) FROM ByParent WHERE Id NOT IN (SELECT Id FROM ByPath)) AS OnlyInParent,\n    (SELECT COUNT(*) FROM HierarchyLevels h\n        WHERE h.IsDeleted = 0 AND h.IsRoot = 0\n          AND h.OrganizationId = '74A17135-94CF-62A5-BDE1-3A16C41F541A'\n          AND h.Id NOT IN (SELECT Id FROM ByPath)) AS OrgProgramsOutOfScope\nOPTION (MAXRECURSION 100);";
    let _ = settled(cte, &|_| false, cx).await;
    let quiet = cx.update(|_, cx| editor.read(cx).lints().iter().map(|l| l.message.clone()).collect::<Vec<_>>()).unwrap_or_default();
    report("the recursive CTE with OPTION (MAXRECURSION) has no squiggle at all", quiet.is_empty(), format!("{quiet:?}"));
    direct(ws, "DROP TABLE IF EXISTS dbo.HierarchyLevels", cx).await;
    direct(ws, "DROP TABLE IF EXISTS dbo.chk_many", cx).await;
    ok
}
