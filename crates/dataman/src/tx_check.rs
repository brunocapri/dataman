use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Focusable};
use dataman_core::{ConnectionProfile, Environment, TxStatus};

use crate::app::Workspace;
use crate::tab::{QueryTab, TabConfirm};

pub fn is_stage(s: &str) -> bool {
    s.starts_with("tx") || s.starts_with("guard") || s.starts_with("prod")
}

pub fn prepare(stage: &str, p: &mut ConnectionProfile) {
    if stage.starts_with("prod") {
        p.name = "prod-orders".into();
        p.env = Environment::Production;
    }
}

const TX_SQL: &str = "BEGIN;\nUPDATE orders SET status = status WHERE id <= 3;\nSELECT p.id, p.status, p.total, c.name\nFROM   orders p\nJOIN   customer c ON c.id = p.customer\nWHERE  p.id <= 3\nORDER  BY p.id;";
const FAILED_SQL: &str = "BEGIN;\nUPDATE orders SET status = status WHERE id <= 3;\nSELECT count(*) FROM ordrs;";
const GUARD_SQL: &str = "-- clean up the test orders\nDELETE FROM orders;";
const PROD_SQL: &str = "UPDATE orders\n   SET total = total * 1.1\n WHERE customer = 42;";

fn key(cx: &mut AsyncWindowContext, k: &str) {
    draw(cx);
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(crate::keymap::keystroke(k), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn active(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Entity<QueryTab> {
    cx.update(|_, cx| ws.read(cx).active_tab().unwrap().clone()).unwrap()
}

async fn idle(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
    pause(cx, 60).await;
    for _ in 0..300 {
        let busy = cx.update(|_, cx| {
            let t = tab.read(cx);
            t.is_running() || t.ending_tx() || t.result().is_some_and(|r| !r.phase().is_finished())
        });
        if !busy.unwrap_or(false) {
            return;
        }
        pause(cx, 50).await;
    }
}

async fn type_run(tab: &Entity<QueryTab>, sql: &str, script: bool, cx: &mut AsyncWindowContext) {
    let editor = cx.update(|_, cx| tab.read(cx).editor.clone()).unwrap();
    let _ = cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.set_text(sql, cx));
        window.focus(&editor.focus_handle(cx), cx);
    });
    draw(cx);
    crate::editor::dispatch(cx, if script { Box::new(crate::editor::RunScript) } else { Box::new(crate::editor::RunStatement) });
    idle(tab, cx).await;
}

fn first_cell(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> Option<String> {
    cx.update(|_, cx| {
        let s = tab.read(cx).result()?.snapshot();
        (s.rows() > 0 && !s.columns.is_empty()).then(|| s.text(0, 0).map(|v| v.into_owned())).flatten()
    })
    .ok()
    .flatten()
}

fn tx(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> (TxStatus, u32) {
    cx.update(|_, cx| {
        let s = tab.read(cx).tx();
        (s.status, s.statements)
    })
    .unwrap_or_default()
}

fn confirm(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> Option<&'static str> {
    cx.update(|_, cx| match &tab.read(cx).confirm {
        Some(TabConfirm::Guard { .. }) => Some("guard"),
        Some(TabConfirm::Failed { .. }) => Some("failed"),
        Some(TabConfirm::Monitor(_)) => Some("monitor"),
        None => None,
    })
    .ok()
    .flatten()
}

async fn extra_connection(ws: &Entity<Workspace>, name: &str, f: impl FnOnce(&mut ConnectionProfile), cx: &mut AsyncWindowContext) -> Option<String> {
    let (mut p, pw) = crate::bench::bench_profile(None);
    p.name = name.into();
    f(&mut p);
    let id = p.id.clone();
    let _ = cx.update(|_, cx| crate::bench::connect(ws, p, pw, cx));
    for _ in 0..200 {
        if cx.update(|_, cx| ws.read(cx).store.read(cx).connection(&id).is_some()).unwrap_or(false) {
            return Some(id);
        }
        pause(cx, 50).await;
    }
    None
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, cx: &mut AsyncWindowContext) -> bool {
    let tab = active(ws, cx);
    match stage {
        "tx" | "tx-close" => {
            type_run(&tab, TX_SQL, true, cx).await;
            if stage == "tx-close" {
                key(cx, "cmd-w");
            }
            true
        }
        "tx-failed" => {
            type_run(&tab, FAILED_SQL, true, cx).await;
            let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.run(vec!["SELECT p.id, p.status FROM orders p LIMIT 20".into()], cx)));
            true
        }
        "guard" => {
            type_run(&tab, GUARD_SQL, false, cx).await;
            true
        }
        "prod" => {
            type_run(&tab, "-- production: the red accent, read with care\nSELECT p.id, p.status, p.total, c.name\nFROM   orders p\nJOIN   customer c ON c.id = p.customer\nORDER  BY p.id;", false, cx).await;
            true
        }
        "prod-guard" => {
            type_run(&tab, PROD_SQL, false, cx).await;
            true
        }
        _ => check(ws, &tab, cx).await,
    }
}

async fn check(ws: &Entity<Workspace>, tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    type_run(tab, "CREATE TEMP TABLE tx_demo AS SELECT g AS id FROM generate_series(1, 5) g", false, cx).await;

    type_run(tab, "DELETE FROM tx_demo", false, cx).await;
    let shown = confirm(tab, cx);
    let focused = cx.update(|window, cx| ws.read(cx).sheet_focus.is_focused(window)).unwrap_or(false);
    report("DELETE without WHERE waits for the sheet, which has the keys", shown == Some("guard") && focused, format!("{shown:?} focused {focused}"));
    key(cx, "escape");
    report("esc cancels it", confirm(tab, cx).is_none(), format!("{:?}", confirm(tab, cx)));
    type_run(tab, "SELECT count(*) FROM tx_demo", false, cx).await;
    report("nothing was deleted", first_cell(tab, cx).as_deref() == Some("5"), format!("{:?}", first_cell(tab, cx)));
    type_run(tab, "DELETE FROM tx_demo", false, cx).await;
    key(cx, "cmd-enter");
    report("a ⌘↩ right after the run does not confirm", confirm(tab, cx) == Some("guard"), format!("{:?}", confirm(tab, cx)));
    pause(cx, 400).await;
    key(cx, "cmd-enter");
    idle(tab, cx).await;
    let affected = cx.update(|_, cx| tab.read(cx).result().and_then(|r| r.status().affected)).ok().flatten();
    report("⌘↩ runs it", confirm(tab, cx).is_none() && affected == Some(5), format!("{affected:?}"));
    type_run(tab, "DELETE FROM tx_demo WHERE id > 100", false, cx).await;
    report("with a WHERE it runs at once", confirm(tab, cx).is_none(), String::new());
    type_run(tab, "INSERT INTO tx_demo SELECT generate_series(1, 5)", false, cx).await;

    type_run(tab, "INSERT INTO tx_demo VALUES (6)", false, cx).await;
    report("a statement autocommits (no transaction left)", tx(tab, cx).0 == TxStatus::Idle, format!("{:?}", tx(tab, cx)));
    key(cx, "ctrl-cmd-backspace");
    idle(tab, cx).await;
    type_run(tab, "SELECT count(*) FROM tx_demo", false, cx).await;
    report("…and stays: ⌃⌘⌫ ends nothing", first_cell(tab, cx).as_deref() == Some("6"), format!("{:?}", first_cell(tab, cx)));
    type_run(tab, "DELETE FROM tx_demo WHERE id = 6", false, cx).await;

    type_run(tab, "BEGIN", false, cx).await;
    type_run(tab, "INSERT INTO tx_demo VALUES (6)", false, cx).await;
    report("BEGIN opens a transaction (TX 1)", tx(tab, cx) == (TxStatus::Open, 1), format!("{:?}", tx(tab, cx)));
    key(cx, "ctrl-cmd-backspace");
    idle(tab, cx).await;
    report("⌃⌘⌫ rolls back", tx(tab, cx).0 == TxStatus::Idle, format!("{:?}", tx(tab, cx)));
    type_run(tab, "BEGIN", false, cx).await;
    type_run(tab, "INSERT INTO tx_demo VALUES (7)", false, cx).await;
    type_run(tab, "SELECT count(*) FROM tx_demo", false, cx).await;
    report("statements add up (TX 2)", tx(tab, cx) == (TxStatus::Open, 2) && first_cell(tab, cx).as_deref() == Some("6"), format!("{:?} {:?}", tx(tab, cx), first_cell(tab, cx)));
    key(cx, "ctrl-cmd-enter");
    idle(tab, cx).await;
    report("⌃⌘↩ commits", tx(tab, cx).0 == TxStatus::Idle, format!("{:?}", tx(tab, cx)));

    type_run(tab, "BEGIN", false, cx).await;
    type_run(tab, "SELECT 1/0", false, cx).await;
    report("an error fails the transaction", tx(tab, cx).0 == TxStatus::Failed, format!("{:?}", tx(tab, cx)));
    type_run(tab, "SELECT 2", false, cx).await;
    report("the next run asks first", confirm(tab, cx) == Some("failed"), format!("{:?}", confirm(tab, cx)));
    pause(cx, 400).await;
    key(cx, "cmd-backspace");
    idle(tab, cx).await;
    report("⌘⌫ rolls back without running", tx(tab, cx).0 == TxStatus::Idle && first_cell(tab, cx).is_none(), format!("{:?} {:?}", tx(tab, cx), first_cell(tab, cx)));
    type_run(tab, "BEGIN", false, cx).await;
    type_run(tab, "SELECT 1/0", false, cx).await;
    type_run(tab, "SELECT 2", false, cx).await;
    pause(cx, 400).await;
    key(cx, "cmd-enter");
    idle(tab, cx).await;
    idle(tab, cx).await;
    report("⌘↩ rolls back and runs (autocommit)", first_cell(tab, cx).as_deref() == Some("2") && tx(tab, cx).0 == TxStatus::Idle, format!("{:?} {:?}", first_cell(tab, cx), tx(tab, cx)));
    type_run(tab, "BEGIN", false, cx).await;
    type_run(tab, "SELECT 1/0", false, cx).await;
    type_run(tab, "ROLLBACK", false, cx).await;
    report("ROLLBACK typed in the editor runs in a failed transaction", tx(tab, cx).0 == TxStatus::Idle, format!("{:?}", tx(tab, cx)));

    type_run(tab, "BEGIN", false, cx).await;
    type_run(tab, "INSERT INTO tx_demo VALUES (8)", false, cx).await;
    key(cx, "cmd-w");
    let (prompt, still) = cx.update(|_, cx| (ws.read(cx).tx_prompt.as_ref().map(|p| p.tabs.len()), ws.read(cx).tabs.contains(tab))).unwrap();
    report("⌘W with a transaction open asks", prompt == Some(1) && still, format!("{prompt:?} {still}"));
    key(cx, "escape");
    let (prompt, still) = cx.update(|_, cx| (ws.read(cx).tx_prompt.is_some(), ws.read(cx).tabs.contains(tab))).unwrap();
    report("esc keeps the tab and its transaction", !prompt && still && tx(tab, cx).0 == TxStatus::Open, format!("{prompt} {still}"));
    let quits = cx.update(|window, cx| ws.update(cx, |w, cx| w.request_quit(window, cx))).unwrap_or(true);
    let listed = cx.update(|_, cx| ws.read(cx).tx_prompt.as_ref().map(|p| p.tabs.len())).ok().flatten();
    report("quit with a transaction open asks, listing it", !quits && listed == Some(1), format!("{quits} {listed:?}"));
    key(cx, "escape");
    key(cx, "cmd-w");
    pause(cx, 400).await;
    key(cx, "cmd-enter");
    for _ in 0..100 {
        if !cx.update(|_, cx| ws.read(cx).tabs.contains(tab)).unwrap_or(false) {
            break;
        }
        pause(cx, 50).await;
    }
    let closed = !cx.update(|_, cx| ws.read(cx).tabs.contains(tab)).unwrap_or(true);
    report("⌘↩ commits and closes", closed, String::new());

    if let Some(ro) = extra_connection(ws, "read-only", |p| p.read_only = true, cx).await {
        let t = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_sql_tab(&ro, None, "INSERT INTO orders (id) VALUES (-1)", true, window, cx))).ok().flatten();
        if let Some(t) = t {
            idle(&t, cx).await;
            let err = cx.update(|_, cx| t.read(cx).result().and_then(|r| r.status().error)).ok().flatten();
            report(
                "a read-only connection refuses an INSERT (25006, client side)",
                err.as_ref().is_some_and(|e| e.code.as_deref() == Some("25006") && e.message.contains("read-only")),
                format!("{err:?}"),
            );
            type_run(&t, "DROP TABLE orders", false, cx).await;
            report("…and asks nothing about a DROP it refuses anyway", confirm(&t, cx).is_none(), format!("{:?}", confirm(&t, cx)));
        }
    } else {
        report("read-only connection", false, "could not connect".into());
    }

    if let Some(prod) = extra_connection(ws, "production", |p| p.env = Environment::Production, cx).await {
        let t = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_sql_tab(&prod, None, "SELECT 1", true, window, cx))).ok().flatten();
        if let Some(t) = t {
            idle(&t, cx).await;
            report("production: a SELECT runs", confirm(&t, cx).is_none() && first_cell(&t, cx).as_deref() == Some("1"), format!("{:?}", first_cell(&t, cx)));
            type_run(&t, "CREATE TEMP TABLE prod_t (x int)", false, cx).await;
            report("production: DDL waits for the sheet", confirm(&t, cx) == Some("guard"), format!("{:?}", confirm(&t, cx)));
            key(cx, "escape");
        }
    } else {
        report("production connection", false, "could not connect".into());
    }

    if let Some(id) = extra_connection(ws, "switch-save-delete", |_| {}, cx).await {
        let new_tab = |cx: &mut AsyncWindowContext| cx.update(|window, cx| ws.update(cx, |w, cx| w.open_sql_tab(&id, None, "", false, window, cx))).ok().flatten();
        let prompt = |cx: &mut AsyncWindowContext| {
            cx.update(|_, cx| ws.read(cx).tx_prompt.as_ref().map(|p| (p.then.verb(), p.tabs.len()))).ok().flatten()
        };
        if let Some(t) = new_tab(cx) {
            type_run(&t, "BEGIN", false, cx).await;
            type_run(&t, "SELECT 1", false, cx).await;
            let conn = cx.update(|_, cx| t.read(cx).connection().cloned()).ok().flatten();
            if let Some(conn) = conn.clone() {
                let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.retarget(&t, conn, Some("postgres".into()), None, window, cx)));
            }
            report("another database with a transaction open asks", prompt(cx) == Some(("switch", 1)), format!("{:?}", prompt(cx)));
            key(cx, "escape");
            let db = cx.update(|_, cx| t.read(cx).database().to_string()).unwrap_or_default();
            report("esc keeps the database and the transaction", db == "dataman" && tx(&t, cx).0 == TxStatus::Open, format!("{db} {:?}", tx(&t, cx)));
            if let Some(conn) = conn {
                let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.retarget(&t, conn, Some("postgres".into()), None, window, cx)));
            }
            pause(cx, 400).await;
            key(cx, "cmd-backspace");
            idle(&t, cx).await;
            let db = cx.update(|_, cx| t.read(cx).database().to_string()).unwrap_or_default();
            report("⌘⌫ rolls back and switches", db == "postgres" && prompt(cx).is_none() && tx(&t, cx).0 == TxStatus::Idle, format!("{db} {:?}", prompt(cx)));

            type_run(&t, "BEGIN", false, cx).await;
            type_run(&t, "SELECT 1", false, cx).await;
            let before = cx.update(|_, cx| t.read(cx).connection().cloned()).ok().flatten();
            let _ = cx.update(|window, cx| {
                ws.update(cx, |w, cx| w.open_dialog(Some(id.clone()), window, cx));
                if let Some(d) = ws.read(cx).dialog.clone() {
                    window.focus(&d.focus_handle(cx), cx);
                }
            });
            key(cx, "cmd-enter");
            report("saving the connection with a transaction open asks", prompt(cx) == Some(("save", 1)), format!("{:?}", prompt(cx)));
            key(cx, "escape");
            let dialog = cx.update(|_, cx| ws.read(cx).dialog.is_some()).unwrap_or(false);
            report("esc goes back to the connection sheet, the transaction open", dialog && tx(&t, cx).0 == TxStatus::Open, format!("{dialog} {:?}", tx(&t, cx)));
            key(cx, "cmd-enter");
            pause(cx, 400).await;
            key(cx, "cmd-enter");
            let mut adopted = false;
            for _ in 0..100 {
                pause(cx, 50).await;
                adopted = cx
                    .update(|_, cx| {
                        let now = t.read(cx).connection().cloned();
                        matches!((&before, &now), (Some(a), Some(b)) if !std::sync::Arc::ptr_eq(a, b))
                    })
                    .unwrap_or(false);
                if adopted {
                    break;
                }
            }
            let dialog = cx.update(|_, cx| ws.read(cx).dialog.is_some()).unwrap_or(true);
            report("⌘↩ commits, saves and the tab takes the new connection", adopted && !dialog && tx(&t, cx).0 == TxStatus::Idle, format!("{adopted} {dialog}"));

            type_run(&t, "BEGIN", false, cx).await;
            type_run(&t, "SELECT 1", false, cx).await;
            let _idle_tab = new_tab(cx);
            let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.request_delete_connection(id.clone(), false, window, cx)));
            report("deleting it asks, listing the tab with the transaction", prompt(cx) == Some(("delete", 1)), format!("{:?}", prompt(cx)));
            key(cx, "escape");
            let kept = cx.update(|_, cx| ws.read(cx).store.read(cx).profile(&id).is_some()).unwrap_or(false);
            report("esc deletes nothing", kept && tx(&t, cx).0 == TxStatus::Open, format!("{kept}"));
            let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.request_delete_connection(id.clone(), false, window, cx)));
            pause(cx, 400).await;
            key(cx, "cmd-backspace");
            pause(cx, 300).await;
            let (gone, orphan) = cx.update(|_, cx| (ws.read(cx).store.read(cx).profile(&id).is_none(), t.read(cx).connection().is_none())).unwrap_or_default();
            report("⌘⌫ rolls back and deletes", gone && orphan, format!("{gone} {orphan}"));
        }
    } else {
        report("switch / save / delete connection", false, "could not connect".into());
    }
    ok
}
