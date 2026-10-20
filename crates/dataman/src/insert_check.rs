use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Focusable};
use dataman_core::result::Phase;

use crate::app::Workspace;
use crate::tab::QueryTab;

pub fn is_stage(s: &str) -> bool {
    s == "insert-check"
}

const ROWS: usize = 25_000;

fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn key(cx: &mut AsyncWindowContext, k: &str) {
    draw(cx);
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(crate::keymap::keystroke(k), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn typ(cx: &mut AsyncWindowContext, s: &str) {
    for c in s.chars() {
        key(cx, &if c == ' ' { "space".to_string() } else { c.to_string() });
    }
}

fn snap(name: &str, cx: &mut AsyncWindowContext) {
    let Ok(dir) = std::env::var("DATAMAN_SHOT_DIR") else { return };
    let path = format!("{dir}/insert-{name}.png");
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
    });
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

async fn run_sql(tab: &Entity<QueryTab>, sql: &str, cx: &mut AsyncWindowContext) -> (Option<Phase>, usize, String) {
    let editor = cx.update(|_, cx| tab.read(cx).editor.clone()).unwrap();
    let _ = cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.set_text(sql, cx));
        window.focus(&editor.focus_handle(cx), cx);
    });
    draw(cx);
    crate::editor::dispatch(cx, Box::new(crate::editor::RunScript));
    // DROP waits for the guard sheet: ⌘↩ (not at once: a held ⌘↩ does not confirm).
    pause(cx, 450).await;
    if cx.update(|_, cx| matches!(tab.read(cx).confirm, Some(crate::tab::TabConfirm::Guard { .. }))).unwrap_or(false) {
        key(cx, "cmd-enter");
    }
    settle(tab, cx).await
}

async fn settle(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> (Option<Phase>, usize, String) {
    let read = |cx: &mut AsyncWindowContext| {
        cx.update(|_, cx| {
            let t = tab.read(cx);
            let rs = t.result().cloned();
            let queued = t.batch_items().iter().any(|b| b.state == crate::tab::ItemState::Pending);
            (t.is_running() || queued, rs.as_ref().map(|r| r.phase()), rs.as_ref().map_or(0, |r| r.rows()), rs.and_then(|r| r.status().error.map(|e| e.message)).unwrap_or_default())
        })
        .unwrap_or((false, None, 0, String::new()))
    };
    pause(cx, 80).await;
    for _ in 0..600 {
        let (running, phase, rows, err) = read(cx);
        if !running && phase.is_some_and(|p| p.is_finished() || p == Phase::Paused) {
            return (phase, rows, err);
        }
        pause(cx, 50).await;
    }
    let (_, phase, rows, err) = read(cx);
    (phase, rows, err)
}

async fn applied(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
    pause(cx, 80).await;
    for _ in 0..200 {
        if !cx.update(|_, cx| tab.read(cx).edit.read(cx).applying).unwrap_or(false) {
            return;
        }
        pause(cx, 50).await;
    }
}

async fn plus(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
    let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
    let _ = cx.update(|_, cx| grid.update(cx, |g, cx| g.insert_row(cx)));
    for _ in 0..100 {
        pause(cx, 50).await;
        draw(cx);
        let e = cx.update(|_, cx| {
            let e = tab.read(cx).edit.read(cx);
            e.demo_cell_editor().is_some() || e.demo_value_editor().is_some() || e.demo_read_only().is_some()
        });
        if e.unwrap_or(false) {
            return;
        }
    }
}

struct View {
    rows: usize,
    pending: usize,
    editing: bool,
    sel: Option<(usize, usize)>,
    scroll: f64,
    bottom: f64,
}

fn view(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> View {
    cx.update(|_, cx| {
        let t = tab.read(cx);
        let rows = t.result().map_or(0, |r| r.rows());
        let e = t.edit.read(cx);
        let g = t.grid.read(cx);
        let (row_h, _) = g.demo_row_metrics();
        let (_, _, _, body_h) = g.demo_cells_rect();
        let all = rows + e.model.changes.inserted().len();
        View {
            rows,
            pending: e.pending_count(),
            editing: e.demo_cell_editor().is_some(),
            sel: g.active_cell(),
            scroll: g.demo_scroll().1,
            bottom: (all as f64 * row_h as f64 - body_h as f64).max(0.),
        }
    })
    .unwrap_or(View { rows: 0, pending: 0, editing: false, sel: None, scroll: 0., bottom: 0. })
}

fn first_cell(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> String {
    cx.update(|_, cx| {
        let s = tab.read(cx).result()?.snapshot();
        (s.rows() > 0).then(|| s.text(0, 0).map(|v| v.into_owned())).flatten()
    })
    .ok()
    .flatten()
    .unwrap_or_default()
}

fn new_value(tab: &Entity<QueryTab>, i: usize, col: usize, cx: &mut AsyncWindowContext) -> Option<String> {
    cx.update(|_, cx| tab.read(cx).edit.read(cx).model.changes.inserted().get(i).and_then(|r| r.get(col).cloned().flatten()).and_then(|v| v.as_deref().map(str::to_string)))
        .ok()
        .flatten()
}

fn discard(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        tab.read(cx).edit.clone().update(cx, |e, cx| e.discard(window, cx));
        let grid = tab.read(cx).grid.clone();
        window.focus(&grid.focus_handle(cx), cx);
    });
    draw(cx);
}

async fn commit(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
    key(cx, "cmd-s");
    pause(cx, 100).await;
    let open = cx.update(|_, cx| tab.read(cx).edit.read(cx).review_open).unwrap_or(false);
    if open {
        let _ = cx.update(|_, cx| tab.read(cx).edit.clone().update(cx, |e, cx| e.apply(cx)));
    }
    applied(tab, cx).await;
    draw(cx);
}

pub async fn run(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut check = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass || detail.is_empty() { String::new() } else { format!(": {detail}") });
        ok &= pass;
    };
    pause(cx, 300).await;
    let tab = cx.update(|_, cx| ws.read(cx).active_tab().unwrap().clone()).unwrap();
    let setup = format!(
        "DROP TABLE IF EXISTS public.insert_t;\nCREATE TABLE public.insert_t (id serial PRIMARY KEY, name text NOT NULL, n int, note text DEFAULT 'none', at timestamptz NOT NULL DEFAULT now());\nINSERT INTO public.insert_t (name, n) SELECT 'row ' || g, g FROM generate_series(1, {ROWS}) g;\nSELECT count(*) FROM public.insert_t;"
    );
    let (phase, _, err) = run_sql(&tab, &setup, cx).await;
    check(&format!("a table of {ROWS} rows"), phase == Some(Phase::Done) && first_cell(&tab, cx) == ROWS.to_string(), format!("{phase:?} {} {err}", first_cell(&tab, cx)));

    let (phase, rows, err) = run_sql(&tab, "SELECT * FROM public.insert_t ORDER BY id", cx).await;
    check("the result pauses after its first page", phase == Some(Phase::Paused) && rows > 0 && rows < ROWS, format!("{phase:?} {rows} {err}"));
    let page = rows;

    plus(&tab, cx).await;
    let v = view(&tab, cx);
    check("+ adds a row after the loaded ones", v.pending == 1 && v.rows == page, format!("pending {} rows {}", v.pending, v.rows));
    check("+ opens the new row's first field", v.editing && v.sel == Some((page, 0)), format!("editing {} sel {:?}", v.editing, v.sel));
    check("+ scrolls the grid to the new row", (v.scroll - v.bottom).abs() < 1., format!("scroll {} bottom {}", v.scroll, v.bottom));

    key(cx, "tab");
    typ(cx, "novo um");
    key(cx, "tab");
    typ(cx, "4a2. 4x2");
    snap("typing", cx);
    key(cx, "enter");
    draw(cx);
    let (id, name, n) = (new_value(&tab, 0, 0, cx), new_value(&tab, 0, 1, cx), new_value(&tab, 0, 2, cx));
    check("⇥ moves along the new row; typed values land, the int takes digits only", id.is_none() && name.as_deref() == Some("novo um") && n.as_deref() == Some("4242"), format!("{id:?} {name:?} {n:?}"));

    key(cx, "escape");
    let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
    key(cx, &crate::keymap::effective("fetch_more")[0]);
    let (phase, rows, _) = settle(&tab, cx).await;
    pause(cx, 150).await;
    draw(cx);
    let v = view(&tab, cx);
    check("⌘↓ fetches more; the new row follows the result", rows > page && v.pending == 1 && v.sel.map(|s| s.0) == Some(rows), format!("{phase:?} {rows} sel {:?} pending {}", v.sel, v.pending));
    check("the typed values survive the fetch", new_value(&tab, 0, 1, cx).as_deref() == Some("novo um"), format!("{:?}", new_value(&tab, 0, 1, cx)));

    plus(&tab, cx).await;
    let v = view(&tab, cx);
    check("a second + adds a second row after the first", v.pending == 2 && v.sel == Some((v.rows + 1, 0)) && v.editing, format!("pending {} sel {:?} rows {}", v.pending, v.sel, v.rows));
    key(cx, "tab");
    typ(cx, "novo dois");
    key(cx, "enter");
    commit(&tab, cx).await;
    let v = view(&tab, cx);
    let report = cx.update(|_, cx| tab.read(cx).edit.read(cx).report.as_ref().map(|r| format!("{:?}", r.error))).ok().flatten();
    check("⌘S commits both rows", v.pending == 0, format!("pending {} {report:?}", v.pending));
    let (_, _, err) = run_sql(&tab, "SELECT count(*) FROM public.insert_t WHERE (name = 'novo um' AND n = 4242 AND note = 'none') OR (name = 'novo dois' AND n IS NULL)", cx).await;
    check("the rows are in the table, defaults filled", first_cell(&tab, cx) == "2", format!("{} {err}", first_cell(&tab, cx)));

    run_sql(&tab, "SELECT * FROM public.insert_t ORDER BY id", cx).await;
    plus(&tab, cx).await;
    key(cx, "escape");
    commit(&tab, cx).await;
    let v = view(&tab, cx);
    let report = cx.update(|_, cx| tab.read(cx).edit.read(cx).report.as_ref().map(|r| (r.committed, r.error.clone().unwrap_or_default(), r.outcomes.len()))).ok().flatten();
    check("an empty NOT NULL fails the commit; the row stays", v.pending == 1 && report.as_ref().is_some_and(|r| !r.0), format!("pending {} {report:?}", v.pending));
    discard(&tab, cx);
    check("⌘⌫ discards the new row", view(&tab, cx).pending == 0, String::new());

    plus(&tab, cx).await;
    key(cx, "escape");
    let v = view(&tab, cx);
    check("Esc closes the field, the row stays", !v.editing && v.pending == 1, format!("editing {} pending {}", v.editing, v.pending));
    discard(&tab, cx);

    plus(&tab, cx).await;
    key(cx, "escape");
    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
    key(cx, &crate::keymap::effective("fetch_all")[0]);
    let (phase, rows, _) = settle(&tab, cx).await;
    pause(cx, 150).await;
    draw(cx);
    let v = view(&tab, cx);
    check("Fetch all: every row, the new one last", phase == Some(Phase::Done) && rows == ROWS + 2 && v.sel.map(|s| s.0) == Some(rows), format!("{phase:?} {rows} sel {:?}", v.sel));
    discard(&tab, cx);

    let _ = cx.update(|_, cx| grid.update(cx, |g, cx| g.toggle_sort(1, cx)));
    pause(cx, 300).await;
    plus(&tab, cx).await;
    let v = view(&tab, cx);
    check("a sorted result: + adds at the end", v.pending == 1 && v.sel == Some((v.rows, 0)) && (v.scroll - v.bottom).abs() < 1., format!("sel {:?} rows {} scroll {} bottom {}", v.sel, v.rows, v.scroll, v.bottom));
    discard(&tab, cx);

    run_sql(&tab, "SELECT * FROM public.insert_t WHERE false", cx).await;
    plus(&tab, cx).await;
    let v = view(&tab, cx);
    check("an empty result: + opens row 1", v.pending == 1 && v.editing && v.sel == Some((0, 0)), format!("pending {} editing {} sel {:?}", v.pending, v.editing, v.sel));
    key(cx, "tab");
    typ(cx, "do vazio");
    key(cx, "enter");
    commit(&tab, cx).await;
    check("…and commits", view(&tab, cx).pending == 0, String::new());
    run_sql(&tab, "SELECT count(*) FROM public.insert_t WHERE name = 'do vazio'", cx).await;
    check("the row from the empty result is in the table", first_cell(&tab, cx) == "1", first_cell(&tab, cx));

    run_sql(&tab, "SELECT 1 AS a, 'x' AS b", cx).await;
    plus(&tab, cx).await;
    let ro = cx.update(|_, cx| tab.read(cx).edit.read(cx).demo_read_only()).ok().flatten();
    check("a read-only result refuses +", ro.is_some() && view(&tab, cx).pending == 0, format!("{ro:?}"));

    let editor = cx.update(|_, cx| tab.read(cx).editor.clone()).unwrap();
    let _ = cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.set_text("SELECT i.*, pg_sleep(0.0005) AS z FROM public.insert_t i WHERE i.id <= 4000 ORDER BY i.id", cx));
        window.focus(&editor.focus_handle(cx), cx);
    });
    crate::editor::dispatch(cx, Box::new(crate::editor::RunScript));
    pause(cx, 400).await;
    let running = cx.update(|_, cx| tab.read(cx).result().is_some_and(|r| r.phase() == Phase::Running)).unwrap_or(false);
    let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
    let _ = cx.update(|_, cx| grid.update(cx, |g, cx| g.insert_row(cx)));
    pause(cx, 100).await;
    let early = view(&tab, cx).pending;
    let (phase, rows, _) = settle(&tab, cx).await;
    for _ in 0..40 {
        if view(&tab, cx).pending > 0 {
            break;
        }
        pause(cx, 50).await;
    }
    draw(cx);
    let v = view(&tab, cx);
    check("+ while running waits for the rows, then adds after them", running && early == 0 && v.pending == 1 && v.sel.map(|s| s.0) == Some(rows), format!("running {running} early {early} {phase:?} {rows} pending {} sel {:?} {:?}", v.pending, v.sel, cx.update(|_, cx| tab.read(cx).edit.read(cx).demo_read_only()).ok().flatten()));
    discard(&tab, cx);

    run_sql(&tab, "DROP TABLE IF EXISTS public.insert_t", cx).await;

    let my = "DROP TABLE IF EXISTS insert_t;\nCREATE TABLE insert_t (id int AUTO_INCREMENT PRIMARY KEY, name varchar(50) NOT NULL, n int, note varchar(20) DEFAULT 'none');\nSET SESSION cte_max_recursion_depth = 20000;\nINSERT INTO insert_t (name, n) WITH RECURSIVE s(g) AS (SELECT 1 UNION ALL SELECT g + 1 FROM s WHERE g < 15000) SELECT CONCAT('row ', g), g FROM s;\nSELECT count(*) FROM insert_t;";
    let ms = "DROP TABLE IF EXISTS dbo.insert_t;\nCREATE TABLE dbo.insert_t (id int IDENTITY PRIMARY KEY, name nvarchar(50) NOT NULL, n int, note nvarchar(20) DEFAULT 'none');\nINSERT INTO dbo.insert_t (name, n) SELECT CONCAT('row ', g), g FROM (SELECT TOP 15000 ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS g FROM sys.all_objects a CROSS JOIN sys.all_objects b) s;\nSELECT count(*) FROM dbo.insert_t;";
    for (label, (prof, pw), setup) in [("MySQL", crate::bench::mysql_profile(), my), ("SQL Server", crate::bench::mssql_profile(), ms)] {
        let r = engine_round(ws, prof, pw, setup, cx).await;
        check(&format!("{label}: + on a paused 15,000-row table, typed, committed, read back"), r.is_ok(), r.err().unwrap_or_default());
    }
    ok
}

async fn engine_round(ws: &Entity<Workspace>, prof: dataman_core::ConnectionProfile, pw: String, setup: &str, cx: &mut AsyncWindowContext) -> Result<(), String> {
    let id = prof.id.clone();
    let _ = cx.update(|_, cx| crate::bench::connect(ws, prof, pw, cx));
    let mut conn = None;
    for _ in 0..300 {
        conn = cx.update(|_, cx| ws.read(cx).store.read(cx).connection(&id)).ok().flatten();
        if conn.is_some() {
            break;
        }
        pause(cx, 50).await;
    }
    let conn = conn.ok_or("not connected")?;
    let tab = cx
        .update(|window, cx| {
            let tab = ws.update(cx, |w, cx| w.new_tab(window, cx));
            tab.update(cx, |t, cx| t.set_connection(Some(conn), cx));
            tab
        })
        .map_err(|e| format!("{e:#}"))?;
    let (phase, _, err) = run_sql(&tab, setup, cx).await;
    if phase != Some(Phase::Done) || first_cell(&tab, cx) != "15000" {
        let items = cx
            .update(|_, cx| tab.read(cx).batch_items().iter().map(|b| format!("[{:?} {:.30} {:?}]", b.state, b.sql, b.rs.as_ref().map(|r| (r.rows(), r.status().error.map(|e| e.message))))).collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        return Err(format!("setup {phase:?} {} {err} {items}", first_cell(&tab, cx)));
    }
    let (phase, page, err) = run_sql(&tab, "SELECT * FROM insert_t ORDER BY id", cx).await;
    if phase != Some(Phase::Paused) {
        return Err(format!("not paused: {phase:?} {page} {err}"));
    }
    plus(&tab, cx).await;
    let v = view(&tab, cx);
    if !(v.pending == 1 && v.editing && v.sel.map(|s| s.0) == Some(page) && (v.scroll - v.bottom).abs() < 1.) {
        let ro = cx.update(|_, cx| tab.read(cx).edit.read(cx).demo_read_only()).ok().flatten();
        return Err(format!("+: pending {} editing {} sel {:?} rows {page} scroll {} bottom {} {ro:?}", v.pending, v.editing, v.sel, v.scroll, v.bottom));
    }
    if v.sel.map(|s| s.1) == Some(0) {
        key(cx, "tab");
    }
    typ(cx, "novo");
    key(cx, "tab");
    typ(cx, "77");
    key(cx, "enter");
    commit(&tab, cx).await;
    let v = view(&tab, cx);
    if v.pending != 0 {
        let report = cx.update(|_, cx| tab.read(cx).edit.read(cx).report.as_ref().map(|r| format!("{:?}", r.error))).ok().flatten();
        return Err(format!("not committed: {report:?}"));
    }
    run_sql(&tab, "SELECT count(*) FROM insert_t WHERE name = 'novo' AND n = 77 AND note = 'none'", cx).await;
    let found = first_cell(&tab, cx);
    run_sql(&tab, "DROP TABLE insert_t", cx).await;
    if found != "1" {
        return Err(format!("read back {found}"));
    }
    Ok(())
}
