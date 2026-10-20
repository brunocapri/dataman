//! `explorer-check`: the PostgreSQL objects of seeds/explorer.sql in the tree, the filter, the
//! context menus (opened with a right click, items clicked) and their effects, and Generate ›
//! CREATE TABLE on PostgreSQL, MySQL and SQL Server read back by dropping the table and running
//! the generated DDL.

use gpui::{AsyncWindowContext, Entity, Focusable, Modifiers, MouseButton};
use dataman_core::config::Engine;

use crate::app::Workspace;
use crate::check::*;
use crate::tree::SchemaTree;
use crate::tree_menu::{Act, Gen};

fn tree(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Entity<SchemaTree> {
    cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap()
}

async fn expand(t: &Entity<SchemaTree>, id: &str, cx: &mut AsyncWindowContext) -> bool {
    let there = until(cx, 10, |cx| cx.update(|_, cx| t.read(cx).has_row(id)).unwrap_or(false)).await;
    if there {
        let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.expand_id(id, cx)));
        pause(cx, 300).await;
    } else {
        eprintln!("check: no row {id}");
    }
    there
}

/// Right-clicks the row (selecting it first so it is scrolled into view) and clicks `act`; a
/// submenu's item runs as its click would.
async fn menu(t: &Entity<SchemaTree>, row: &str, act: Act, cx: &mut AsyncWindowContext) -> bool {
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.select_id(row, cx)));
    draw(cx);
    pause(cx, 100).await;
    let Some(p) = cx.update(|_, cx| t.read(cx).demo_row_point(row)).ok().flatten() else {
        eprintln!("check: row {row} not on screen");
        return false;
    };
    click_with(cx, p, MouseButton::Right, Modifiers::default(), 1);
    pause(cx, 250).await;
    draw(cx);
    let acts = cx.update(|_, cx| t.read(cx).demo_menu_acts()).unwrap_or_default();
    if !acts.contains(&act) {
        eprintln!("check: {act:?} not in the menu of {row}: {acts:?}");
        key(cx, "escape");
        return false;
    }
    match cx.update(|_, cx| t.read(cx).demo_menu_item_point(act)).ok().flatten() {
        Some(p) => click(cx, p),
        None => {
            let _ = cx.update(|window, cx| t.update(cx, |t, cx| t.demo_menu_run(act, window, cx)));
        }
    }
    pause(cx, 300).await;
    draw(cx);
    true
}

fn editor_text(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> String {
    cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.read(cx).editor.read(cx).text().to_string())).ok().flatten().unwrap_or_default()
}

/// Runs what the active tab's editor holds (⌘↩), answering the guard sheet's question with ⌘↩.
async fn run_editor(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<(Option<dataman_core::result::Phase>, usize, String)> {
    let tab = active(ws, cx)?;
    let _ = cx.update(|window, cx| {
        let e = tab.read(cx).editor.clone();
        window.focus(&e.focus_handle(cx), cx);
    });
    key(cx, "cmd-enter");
    pause(cx, 200).await;
    // The guard's question takes ⌘↩ only once it has been up a moment.
    if cx.update(|_, cx| matches!(tab.read(cx).confirm, Some(crate::tab::TabConfirm::Guard { .. }))).unwrap_or(false) {
        pause(cx, 500).await;
        key(cx, "cmd-enter");
    }
    until(cx, 20, |cx| idle(&tab, cx)).await;
    Some(result(&tab, cx))
}

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (pg, pw) = profile(Engine::Postgres);
    let up = connect(ws, &pg, &pw, cx).await;
    if !r.check("PostgreSQL connects", up.is_ok(), format!("{up:?}")) {
        return;
    }
    crate::table_check::run_seed(ws, crate::explorer_demo::SEED, cx).await;
    let t = tree(ws, cx);
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.refresh_connection(&pg.id, cx)));
    let s = format!("c:{}/d:{}/s:dataman_shop", pg.id, pg.database);
    expand(&t, &s, cx).await;
    for g in ["tables", "views", "matviews", "functions", "sequences", "types", "triggers"] {
        expand(&t, &format!("{s}/{g}"), cx).await;
    }
    let labels = cx.update(|_, cx| t.read(cx).demo_children(&s)).unwrap_or_default();
    let want = ["customers", "orders", "events", "open_orders", "daily_sales", "order_total", "refresh_sales", "invoice_no", "order_status", "orders_touch"];
    let missing: Vec<&str> = want.iter().copied().filter(|w| !labels.iter().any(|l| l.starts_with(w))).collect();
    r.check("the schema lists its tables, partitioned table, views, matview, routines, sequence, types and triggers", missing.is_empty(), format!("missing {missing:?} in {labels:?}"));

    let _ = cx.update(|window, cx| window.focus(&t.focus_handle(cx), cx));
    key(cx, "/");
    typ(cx, "invoice");
    pause(cx, 300).await;
    let shown = cx.update(|_, cx| t.read(cx).demo_children(&s)).unwrap_or_default();
    r.check("/ then typing filters the tree to the matches", shown.iter().any(|l| l.starts_with("invoice_no")) && !shown.iter().any(|l| l.starts_with("customers")), format!("{shown:?}"));
    key(cx, "escape");
    pause(cx, 300).await;
    let back = cx.update(|_, cx| t.read(cx).demo_children(&s)).unwrap_or_default();
    r.check("esc clears the filter", back.iter().any(|l| l.starts_with("customers")), format!("{} rows", back.len()));

    let rel = |n: &str| format!("{s}/r:{n}");
    let core = core(ws, cx);
    let count = sql1(&core, &pg, &pw, "SELECT count(*) FROM dataman_shop.orders", cx).await;
    menu(&t, &rel("orders"), Act::CountRows, cx).await;
    let tab = active(ws, cx).unwrap();
    until(cx, 10, |cx| idle(&tab, cx)).await;
    let got = cell(&tab, 0, 0, cx);
    r.check("Count rows runs count(*) on the table", got.is_some() && got == count, format!("{got:?} vs {count:?}"));

    menu(&t, &rel("orders"), Act::SelectTop, cx).await;
    let tab = active(ws, cx).unwrap();
    until(cx, 10, |cx| idle(&tab, cx)).await;
    let (_, rows, err) = result(&tab, cx);
    r.check("Select top opens the table's rows", rows > 0, format!("{rows} {err}"));

    menu(&t, &rel("orders"), Act::OpenWithoutRunning, cx).await;
    let text = editor_text(ws, cx);
    let ran = active(ws, cx).map(|t| result(&t, cx).0.is_some()).unwrap_or(true);
    r.check("Open without running: the SELECT in a tab, not run", text.contains("dataman_shop.orders") && !ran, format!("{text:?} ran {ran}"));

    for (act, want) in [(Act::CopyName, "orders"), (Act::CopyQualified, "dataman_shop.orders")] {
        set_clipboard("", cx);
        menu(&t, &rel("orders"), act, cx).await;
        let got = clipboard(cx);
        r.check(&format!("{act:?} puts {want} on the clipboard"), got == want, &got);
    }

    for (g, word) in [(Gen::Select, "SELECT"), (Gen::Insert, "INSERT INTO"), (Gen::Update, "UPDATE"), (Gen::Delete, "DELETE FROM")] {
        menu(&t, &rel("customers"), Act::Generate(g), cx).await;
        let text = editor_text(ws, cx);
        r.check(&format!("Generate › {g:?} writes its statement for the table"), text.contains(word) && text.contains("customers"), &text);
    }

    let all: Vec<String> = cx.update(|_, cx| t.read(cx).demo_row_ids(&s)).unwrap_or_default();
    let view = all.iter().find(|i| i.ends_with("open_orders")).cloned().unwrap_or_default();
    menu(&t, &view, Act::ShowDefinition, cx).await;
    until(cx, 5, |cx| editor_text(ws, cx).to_lowercase().contains("select")).await;
    let text = editor_text(ws, cx);
    r.check("Show definition of a view opens its SELECT", text.to_lowercase().contains("select") && text.contains("status"), &text);

    menu(&t, &rel("order_items"), Act::Truncate, cx).await;
    let text = editor_text(ws, cx);
    let res = run_editor(ws, cx).await;
    let left = sql1(&core, &pg, &pw, "SELECT count(*) FROM dataman_shop.order_items", cx).await;
    r.check("Truncate: a template, run through the guard's question, empties the table", text.contains("TRUNCATE") && left.as_deref() == Some("0"), format!("{text:?} {res:?} {left:?}"));

    menu(&t, &view, Act::DropView, cx).await;
    let text = editor_text(ws, cx);
    run_editor(ws, cx).await;
    let views = sql1(&core, &pg, &pw, "SELECT count(*) FROM pg_views WHERE schemaname = 'dataman_shop' AND viewname = 'open_orders'", cx).await;
    let listed = until(cx, 10, |cx| cx.update(|_, cx| t.read(cx).demo_children(&s)).unwrap_or_default().iter().any(|l| l.starts_with("customers"))).await;
    r.check("after a DROP run in a tab, the open schema lists its objects again", listed, format!("{:?}", cx.update(|_, cx| t.read(cx).demo_children(&s)).unwrap_or_default()));
    r.check("Drop view: its template drops the view", text.contains("DROP VIEW") && views.as_deref() == Some("0"), format!("{text:?} {views:?}"));

    let trg = format!("{s}/triggers");
    expand(&t, &s, cx).await;
    expand(&t, &trg, cx).await;
    until(cx, 10, |cx| cx.update(|_, cx| t.read(cx).demo_row_ids(&trg)).unwrap_or_default().iter().any(|i| i.contains("orders_touch"))).await;
    let trg_rows: Vec<String> = cx.update(|_, cx| t.read(cx).demo_row_ids(&trg)).unwrap_or_default();
    if let Some(row) = trg_rows.iter().find(|i| i.contains("orders_touch")) {
        menu(&t, row, Act::DisableTrigger, cx).await;
        let text = editor_text(ws, cx);
        if !text.is_empty() && result(&active(ws, cx).unwrap(), cx).0.is_none() {
            run_editor(ws, cx).await;
        }
        let on = sql1(&core, &pg, &pw, "SELECT tgenabled::text FROM pg_trigger WHERE tgname = 'orders_touch'", cx).await;
        r.check("Disable trigger turns it off", on.as_deref() == Some("D"), format!("{on:?} {text:?}"));
    } else {
        let near: Vec<String> = cx.update(|_, cx| t.read(cx).demo_row_ids(&s)).unwrap_or_default().into_iter().map(|i| i[s.len()..].to_string()).collect();
        r.check("the triggers group lists orders_touch", false, format!("{trg_rows:?} {near:?}"));
    }

    menu(&t, &format!("c:{}", pg.id), Act::Disconnect, cx).await;
    let off = until(cx, 5, |cx| !is_connected(ws, &pg.id, cx)).await;
    menu(&t, &format!("c:{}", pg.id), Act::Connect, cx).await;
    let on = connected(ws, &pg.id, 20, cx).await;
    r.check("Disconnect then Connect from the connection's menu", off && on.is_ok(), format!("{off} {on:?}"));

    ddl_round_trip(ws, Engine::Postgres, Some((pg, pw)), r, cx).await;
    for e in [Engine::MySql, Engine::SqlServer] {
        ddl_round_trip(ws, e, None, r, cx).await;
    }
}

/// A table with a key, a unique, a default, a check and an index; its generated CREATE TABLE
/// must rebuild it (same columns, same constraints) once it is dropped.
async fn ddl_round_trip(ws: &Entity<Workspace>, e: Engine, connected: Option<(dataman_core::ConnectionProfile, String)>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let fresh = connected.is_none();
    let (p, pw) = connected.unwrap_or_else(|| profile(e));
    if fresh {
        let up = connect(ws, &p, &pw, cx).await;
        if !r.check(&format!("{} connects", e.label()), up.is_ok(), format!("{up:?}")) {
            return;
        }
    }
    let core = core(ws, cx);
    let (schema, make, cols, cons) = match e {
        Engine::Postgres => (
            "public".to_string(),
            "CREATE TABLE public.e2e_ddl (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, code text NOT NULL UNIQUE, qty integer NOT NULL DEFAULT 1 CHECK (qty > 0), note varchar(40), made timestamptz DEFAULT now())",
            "SELECT count(*) FROM information_schema.columns WHERE table_schema = 'public' AND table_name = 'e2e_ddl'",
            "SELECT count(*) FROM information_schema.table_constraints WHERE table_schema = 'public' AND table_name = 'e2e_ddl' AND constraint_type IN ('PRIMARY KEY', 'UNIQUE', 'CHECK')",
        ),
        Engine::MySql => (
            p.database.clone(),
            "CREATE TABLE e2e_ddl (id BIGINT AUTO_INCREMENT PRIMARY KEY, code VARCHAR(20) NOT NULL UNIQUE, qty INT NOT NULL DEFAULT 1 CHECK (qty > 0), note VARCHAR(40), made DATETIME DEFAULT CURRENT_TIMESTAMP, KEY e2e_ddl_note (note))",
            "SELECT count(*) FROM information_schema.columns WHERE table_schema = DATABASE() AND table_name = 'e2e_ddl'",
            "SELECT count(*) FROM information_schema.table_constraints WHERE table_schema = DATABASE() AND table_name = 'e2e_ddl' AND constraint_type IN ('PRIMARY KEY', 'UNIQUE', 'CHECK')",
        ),
        _ => (
            "dbo".to_string(),
            "CREATE TABLE dbo.e2e_ddl (id bigint IDENTITY(1,1) PRIMARY KEY, code nvarchar(20) NOT NULL UNIQUE, qty int NOT NULL DEFAULT 1 CHECK (qty > 0), note nvarchar(40), made datetime2 DEFAULT SYSUTCDATETIME())",
            "SELECT count(*) FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_SCHEMA = 'dbo' AND TABLE_NAME = 'e2e_ddl'",
            "SELECT count(*) FROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS WHERE TABLE_SCHEMA = 'dbo' AND TABLE_NAME = 'e2e_ddl' AND CONSTRAINT_TYPE IN ('PRIMARY KEY', 'UNIQUE', 'CHECK')",
        ),
    };
    let drop = match e {
        Engine::Postgres => "DROP TABLE IF EXISTS public.e2e_ddl",
        Engine::MySql => "DROP TABLE IF EXISTS e2e_ddl",
        _ => "DROP TABLE IF EXISTS dbo.e2e_ddl",
    };
    let _ = sql(&core, &p, &pw, drop, cx).await;
    let made = sql(&core, &p, &pw, make, cx).await;
    let before = (sql1(&core, &p, &pw, cols, cx).await, sql1(&core, &p, &pw, cons, cx).await);
    let t = tree(ws, cx);
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.refresh_connection(&p.id, cx)));
    let s = format!("c:{}/d:{}/s:{schema}", p.id, p.database);
    expand(&t, &format!("c:{}/d:{}", p.id, p.database), cx).await;
    expand(&t, &s, cx).await;
    expand(&t, &format!("{s}/tables"), cx).await;
    let opened = menu(&t, &format!("{s}/r:e2e_ddl"), Act::Generate(Gen::CreateTable), cx).await;
    until(cx, 10, |cx| editor_text(ws, cx).contains("CREATE TABLE")).await;
    let ddl = editor_text(ws, cx);
    let _ = sql(&core, &p, &pw, drop, cx).await;
    let ran = run_editor(ws, cx).await;
    let after = (sql1(&core, &p, &pw, cols, cx).await, sql1(&core, &p, &pw, cons, cx).await);
    r.check(
        &format!("{}: Generate › CREATE TABLE rebuilds the table it describes (columns, key, unique, check)", e.label()),
        made.is_ok() && opened && before.0.as_deref() == Some("5") && before == after,
        format!("{made:?} before {before:?} after {after:?} ran {ran:?}\n{ddl}"),
    );
    let _ = sql(&core, &p, &pw, drop, cx).await;
}
