use std::sync::Arc;
use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Focusable, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels, Point, point};
use serde_json::{Value, json};

use crate::app::Workspace;
use crate::tab::QueryTab;

pub fn is_stage(s: &str) -> bool {
    matches!(s, "mcp-check" | "mcp-tab" | "mcp-sheet")
}

pub async fn run_conns(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut check = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass || detail.is_empty() { String::new() } else { format!(": {detail}") });
        ok &= pass;
    };
    pause(cx, 300).await;
    let first_run = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).first_run(cx)).unwrap_or(false);
    check("no connection saved: the welcome screen", first_run(cx), String::new());
    let label = cx.update(|_, cx| ws.read(cx).welcome.clone().update(cx, |w, cx| w.demo_choose(1, cx))).unwrap_or_default();
    draw(cx);
    let section = cx.update(|_, cx| ws.read(cx).settings_view.as_ref().map(|v| v.read(cx).demo_section())).ok().flatten();
    check("\"Add with an agent\" opens Settings › MCP", label == "Add with an agent" && section == Some("MCP"), format!("{label} {section:?}"));
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.toggle_settings(window, cx)));
    let Ok(core) = cx.update(|_, cx| ws.read(cx).store.read(cx).core.clone()) else { return false };
    let _ = core.settings.update(|s| s.mcp_enabled = true);
    let _ = cx.update(|_, cx| crate::mcp::apply(&core, cx));
    let ipc = Arc::new(Ipc { dirs: core.dirs.clone() });
    let mut up = false;
    for _ in 0..200 {
        pause(cx, 50).await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let probe = ipc.clone();
        std::thread::spawn(move || {
            let _ = tx.send(probe.ping());
        });
        if rx.await.unwrap_or(false) {
            up = true;
            break;
        }
    }
    check("the app's socket answers", up, String::new());
    let (err, out) = call(&ipc, "create_connection", json!({"name": "first by agent", "host": "127.0.0.1", "port": 9, "user": "x", "password": "hidden-1"})).await;
    let made = serde_json::from_str::<Value>(&out).ok().and_then(|v| v["id"].as_str().map(str::to_string)).unwrap_or_default();
    pause(cx, 150).await;
    draw(cx);
    let row = cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_conn_row(&made).map(|r| r.0)).ok().flatten();
    check("an agent's connection shows at once: the welcome screen gives way to it", !err && !first_run(cx) && row.as_deref() == Some("first by agent"), format!("{out} {row:?}"));
    let mut p = dataman_core::ConnectionProfile::new("written elsewhere");
    (p.host, p.port) = ("127.0.0.1".into(), 9);
    let other = p.id.clone();
    let written = dataman_core::ConfigStore::open(&core.dirs).and_then(|s| s.upsert(p.clone()));
    let mut seen = false;
    let t0 = std::time::Instant::now();
    for _ in 0..60 {
        pause(cx, 50).await;
        if cx.update(|_, cx| ws.read(cx).store.read(cx).profile(&other).is_some()).unwrap_or(false) {
            seen = true;
            break;
        }
    }
    draw(cx);
    check("one written to the file by another process shows within ~1 s", written.is_ok() && seen, format!("{:?} after {:?}", written.err(), t0.elapsed()));
    let (err, out) = call(&ipc, "update_connection", json!({"connection": "first by agent", "name": "renamed by agent"})).await;
    pause(cx, 150).await;
    draw(cx);
    let row = cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_conn_row(&made).map(|r| r.0)).ok().flatten();
    check("an agent's edit relabels the explorer at once", !err && row.as_deref() == Some("renamed by agent") && !out.contains("hidden-1"), format!("{out} {row:?}"));
    p.name = "renamed elsewhere".into();
    let _ = dataman_core::ConfigStore::open(&core.dirs).and_then(|s| s.upsert(p));
    let mut row = None;
    for _ in 0..60 {
        pause(cx, 50).await;
        draw(cx);
        row = cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_conn_row(&other).map(|r| r.0)).ok().flatten();
        if row.as_deref() == Some("renamed elsewhere") {
            break;
        }
    }
    check("an edit in the file relabels it within ~1 s", row.as_deref() == Some("renamed elsewhere"), format!("{row:?}"));
    let _ = dataman_core::ConfigStore::open(&core.dirs).and_then(|s| s.remove(&other));
    let mut gone = false;
    for _ in 0..60 {
        pause(cx, 50).await;
        if cx.update(|_, cx| ws.read(cx).store.read(cx).profile(&other).is_none()).unwrap_or(false) {
            gone = true;
            break;
        }
    }
    check("one removed from the file goes", gone, String::new());
    ok
}

pub(crate) struct Ipc {
    dirs: dataman_core::Dirs,
}

impl Ipc {
    /// Blocking: on a thread of its own, with a runtime of its own.
    fn tool(&self, name: &str, args: Value) -> (bool, String) {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => return (true, e.to_string()),
        };
        let args = args.as_object().cloned().unwrap_or_default();
        let r = rt.block_on(dataman_mcp::ipc::call(&self.dirs, name, args, "mcp-check", Default::default()));
        match r.map(|r| serde_json::to_value(r).unwrap_or(Value::Null)) {
            Ok(v) => (v["isError"] == json!(true), v["content"][0]["text"].as_str().unwrap_or_default().to_string()),
            Err(e) => (true, e.to_string()),
        }
    }

    fn ping(&self) -> bool {
        tokio::runtime::Builder::new_current_thread().enable_all().build().is_ok_and(|rt| rt.block_on(dataman_mcp::ipc::ping(&self.dirs)).is_ok())
    }
}

pub(crate) fn start(ipc: &Arc<Ipc>, name: &'static str, args: Value) -> tokio::sync::oneshot::Receiver<(bool, String)> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let ipc = ipc.clone();
    std::thread::spawn(move || {
        let r = ipc.tool(name, args);
        let _ = tx.send(r);
    });
    rx
}

pub(crate) async fn call(ipc: &Arc<Ipc>, name: &'static str, args: Value) -> (bool, String) {
    start(ipc, name, args).await.unwrap_or((true, "the call thread went away".into()))
}

fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn input(cx: &mut AsyncWindowContext, e: PlatformInput) {
    let _ = cx.update(|window, cx| {
        window.dispatch_event(e, cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn click(cx: &mut AsyncWindowContext, p: Point<Pixels>) {
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: None, modifiers: Modifiers::default() }));
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: p, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: p, modifiers: Modifiers::default(), click_count: 1 }));
}

fn key(cx: &mut AsyncWindowContext, k: &str) {
    draw(cx);
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(crate::keymap::keystroke(k), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn agent_tabs(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Vec<Entity<QueryTab>> {
    cx.update(|_, cx| ws.read(cx).tabs.iter().filter(|t| t.read(cx).agent.is_some()).cloned().collect()).unwrap_or_default()
}

fn text(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> String {
    cx.update(|_, cx| tab.read(cx).editor.read(cx).text().to_string()).unwrap_or_default()
}

async fn settled(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
    for _ in 0..200 {
        pause(cx, 50).await;
        let done = cx.update(|_, cx| tab.read(cx).run_phase().is_some_and(|p| p.is_finished() || p == dataman_core::result::Phase::Paused)).unwrap_or(true);
        if done {
            break;
        }
    }
    draw(cx);
}

pub(crate) async fn sheet(cx: &mut AsyncWindowContext) -> Option<dataman_mcp::Approval> {
    for _ in 0..200 {
        let p = cx.update(|_, cx| cx.try_global::<crate::mcp::McpHost>().and_then(|h| h.pending.front().map(|p| p.req.clone()))).ok().flatten();
        if p.is_some() {
            draw(cx);
            // As a person reads it: Allow takes effect once the sheet has been up a moment.
            pause(cx, crate::mcp::ALLOW_AFTER.as_millis() as u64 + 100).await;
            draw(cx);
            return p;
        }
        pause(cx, 50).await;
    }
    None
}

pub(crate) fn answer(ws: &Entity<Workspace>, allow: bool, cx: &mut AsyncWindowContext) -> bool {
    draw(cx);
    let b = cx.update(|_, cx| ws.read(cx).mcp_buttons.borrow().get(if allow { 1 } else { 0 }).copied()).ok().flatten();
    let Some(b) = b else { return false };
    click(cx, point(b.left() + b.size.width / 2., b.top() + b.size.height / 2.));
    true
}

fn sheet_open(cx: &mut AsyncWindowContext) -> bool {
    cx.update(|_, cx| cx.try_global::<crate::mcp::McpHost>().is_some_and(|h| !h.pending.is_empty())).unwrap_or(false)
}

async fn set_tier(ws: &Entity<Workspace>, id: &str, tier: dataman_core::McpAccess, cx: &mut AsyncWindowContext) -> bool {
    let Ok(core) = cx.update(|_, cx| ws.read(cx).store.read(cx).core.clone()) else { return false };
    let Some(mut p) = core.config.get(id) else { return false };
    p.mcp = Some(tier);
    if core.config.upsert(p).is_err() {
        return false;
    }
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.store.update(cx, |s, cx| s.reload(cx))));
    pause(cx, 50).await;
    true
}

async fn edit_profile(ws: &Entity<Workspace>, id: &str, f: impl FnOnce(&mut dataman_core::ConnectionProfile), cx: &mut AsyncWindowContext) -> bool {
    let Ok(core) = cx.update(|_, cx| ws.read(cx).store.read(cx).core.clone()) else { return false };
    let Some(mut p) = core.config.get(id) else { return false };
    f(&mut p);
    if core.config.upsert(p).is_err() {
        return false;
    }
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.store.update(cx, |s, cx| s.reload(cx))));
    pause(cx, 50).await;
    true
}

async fn advanced_options(ws: &Entity<Workspace>, ipc: &Arc<Ipc>, id: &str, conn: &str, db: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut check = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass || detail.is_empty() { String::new() } else { format!(": {detail}") });
        ok &= pass;
    };
    let Ok(core) = cx.update(|_, cx| ws.read(cx).store.read(cx).core.clone()) else { return false };
    let s = format!("mcp_adv_{}", std::process::id());
    let yolo = edit_profile(ws, id, |p| (p.mcp, p.mcp_yolo) = (Some(dataman_core::McpAccess::Full), true), cx).await;
    let make = format!("CREATE SCHEMA {s}; CREATE TABLE {s}.t (id int PRIMARY KEY, note text); INSERT INTO {s}.t VALUES (1, 'a'), (2, 'b'), (3, NULL)");
    let (err, out) = call(ipc, "execute", json!({"connection": conn, "database": db, "sql": make})).await;
    check("YOLO: DDL runs unasked", yolo && !err && !sheet_open(cx) && out.contains("\"committed\":true"), out);
    edit_profile(ws, id, |p| p.mcp_yolo = false, cx).await;
    let update = |note: &str, id: u32| format!("UPDATE {s}.t SET note = '{note}' WHERE id = {id}");
    let entry = format!("update {s}.t set note = ? where id = ?");
    let tier_read = edit_profile(ws, id, |p| p.mcp = Some(dataman_core::McpAccess::Read), cx).await;
    let (err, out) = call(ipc, "execute", json!({"connection": conn, "database": db, "sql": update("r", 1)})).await;
    check("tier read refuses the UPDATE", tier_read && err && out.contains("reads only"), out);
    let allowed = edit_profile(ws, id, |p| _ = p.mcp_allow(&[entry.as_str()]), cx).await;
    let (err, out) = call(ipc, "execute", json!({"connection": conn, "database": db, "sql": update("r", 1)})).await;
    check("an allowed query runs in tier read, unasked", allowed && !err && out.contains("[\"UPDATE\",1]") && !sheet_open(cx), out);
    let (err, out) = call(ipc, "execute", json!({"connection": conn, "database": db, "sql": format!("UPDATE {s}.t SET note = 'r' WHERE id = 1 OR true")})).await;
    check("… its shape only: one value for each ?, not more SQL", err && out.contains("reads only"), out);
    let ro = edit_profile(ws, id, |p| p.set_read_only_for(db, true), cx).await;
    let (err, out) = call(ipc, "execute", json!({"connection": conn, "database": db, "sql": update("ro", 2)})).await;
    check("a read-only database still refuses it", ro && err && out.contains("read-only"), out);
    edit_profile(ws, id, |p| p.set_read_only_for(db, false), cx).await;
    let log = std::fs::read_to_string(core.dirs.mcp_audit_file()).unwrap_or_default();
    check("the audit says an allowed query let it through", log.contains(&format!("\"allowed_by\":{{\"by\":\"allowlist\",\"entries\":[\"{entry}\"]}}")), String::new());

    edit_profile(ws, id, |p| p.mcp = Some(dataman_core::McpAccess::Full), cx).await;
    let ddl = format!("ALTER TABLE {s}.t ADD COLUMN IF NOT EXISTS extra int");
    let rx = start(ipc, "execute", json!({"connection": conn, "database": db, "sql": ddl}));
    let q = sheet(cx).await;
    check("the sheet offers Always Allow for the statement", q.as_ref().is_some_and(|q| q.allowable == [ddl.clone()]), format!("{q:?}"));
    key(cx, "cmd-alt-enter");
    let (err, out) = rx.await.unwrap_or((true, String::new()));
    let saved = core.config.get(id).map(|p| p.mcp_allowed).unwrap_or_default();
    check("⌥⌘↩ allows it and adds it to the allowed queries", !err && out.contains("approved") && saved.contains(&ddl), format!("{out} {saved:?}"));
    let (err, out) = call(ipc, "execute", json!({"connection": conn, "database": db, "sql": ddl.to_lowercase()})).await;
    check("the same statement next time: unasked", !err && !sheet_open(cx) && out.contains("\"committed\":true"), out);

    edit_profile(ws, id, |p| p.mcp_yolo = true, cx).await;
    let (err, out) = call(ipc, "execute", json!({"connection": conn, "database": db, "sql": format!("UPDATE {s}.t SET note = 'yolo'")})).await;
    check("YOLO: an UPDATE without WHERE runs unasked", !err && !sheet_open(cx) && out.contains("[\"UPDATE\",3]"), out);
    let log = std::fs::read_to_string(core.dirs.mcp_audit_file()).unwrap_or_default();
    check("the audit says YOLO let it through", log.lines().last().is_some_and(|l| l.contains("\"allowed_by\":{\"by\":\"yolo\"}")), String::new());
    let (_, out) = call(ipc, "list_connections", json!({})).await;
    check("list_connections shows yolo and the allowed queries", out.contains("\"yolo\"") && out.contains(&ddl), String::new());
    let (err, out) = call(ipc, "execute", json!({"connection": conn, "database": db, "sql": format!("DROP SCHEMA {s} CASCADE")})).await;
    check("YOLO: DROP SCHEMA runs unasked", !err && !sheet_open(cx), out);
    let back = edit_profile(
        ws,
        id,
        |p| {
            p.mcp_yolo = false;
            p.mcp_allowed.clear();
        },
        cx,
    )
    .await;
    check("the connection stayed connected through the edits", back && cx.update(|_, cx| ws.read(cx).store.read(cx).connection(id).is_some()).unwrap_or(false), String::new());

    let Some(mut p) = core.config.get(id) else { return false };
    p.env = dataman_core::Environment::Production;
    let state = cx
        .update(|window, cx| {
            ws.update(cx, |w, cx| {
                w.open_dialog_with(Some(p), true, None, window, cx);
                let d = w.dialog.clone()?;
                Some(d.update(cx, |d, cx| {
                    d.demo_advanced(Some("BEGIN"), false, cx);
                    let refused = d.demo_advanced_state();
                    d.demo_advanced(Some("DELETE FROM sessions WHERE expires_at < ?;"), true, cx);
                    (refused, d.demo_advanced_state())
                }))
            })
        })
        .ok()
        .flatten();
    check(
        "the sheet takes one statement per entry; YOLO on production waits for a confirmation",
        state.as_ref().is_some_and(|(r, s)| r.0.is_empty() && s.0 == ["DELETE FROM sessions WHERE expires_at < ?"] && !s.1 && s.2),
        format!("{state:?}"),
    );
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.close_dialog(cx)));
    ok
}

pub(crate) async fn serve(ws: &Entity<Workspace>, id: &str, cx: &mut AsyncWindowContext) -> Option<Arc<Ipc>> {
    let core = cx.update(|_, cx| ws.read(cx).store.read(cx).core.clone()).ok()?;
    let mut p = core.config.get(id)?;
    p.mcp = Some(dataman_core::McpAccess::Full);
    core.config.upsert(p).ok()?;
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.store.update(cx, |s, cx| s.reload(cx))));
    core.settings
        .update(|s| s.mcp_enabled = true)
        .ok()?;
    let _ = cx.update(|_, cx| crate::mcp::apply(&core, cx));
    let ipc = Arc::new(Ipc { dirs: core.dirs.clone() });
    for _ in 0..200 {
        pause(cx, 50).await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let probe = ipc.clone();
        std::thread::spawn(move || {
            let _ = tx.send(probe.ping());
        });
        if rx.await.unwrap_or(false) {
            return Some(ipc);
        }
    }
    println!("FAIL mcp: the app's socket did not answer");
    None
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, id: &str, db: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut check = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass || detail.is_empty() { String::new() } else { format!(": {detail}") });
        ok &= pass;
    };
    let _ = cx.update(|_, cx| {
        if let Some(t) = ws.read(cx).active_tab().cloned() {
            let e = t.read(cx).editor.clone();
            e.update(cx, |e, cx| e.set_text("", cx));
        }
    });
    let Some(agent_ipc) = serve(ws, id, cx).await else { return false };
    let conn = cx.update(|_, cx| ws.read(cx).store.read(cx).profile(id).map(|p| p.name.clone())).ok().flatten().unwrap_or_default();
    let read_sql = "SELECT p.id, p.status, p.total\nFROM   orders p\nORDER  BY p.id\nLIMIT  12";

    let (err, out) = call(&agent_ipc,"open_query", json!({"connection": conn, "database": db, "sql": read_sql, "max_rows": 5})).await;
    let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    check("open_query answers the agent with its bounded rows", !err && v["row_count"] == json!(5) && v["truncated"] == json!("rows") && v["in_tab"] == json!(true), out.clone());
    let tabs = agent_tabs(ws, cx);
    check("one agent tab, the one that was empty in front", tabs.len() == 1 && cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap_or(0) == 1, format!("{}", tabs.len()));
    let Some(agent) = tabs.first().cloned() else { return false };
    settled(&agent, cx).await;
    check("the agent tab holds the SQL", text(&agent, cx) == read_sql, text(&agent, cx));
    let rows = cx.update(|_, cx| agent.read(cx).result().map(|r| r.rows())).ok().flatten();
    check("the whole result stays in the tab", rows == Some(12), format!("{rows:?}"));
    let client = cx.update(|_, cx| agent.read(cx).agent.as_ref().map(|a| a.client.clone())).ok().flatten();
    check("the tab is the client's", client.as_deref() == Some("mcp-check"), format!("{client:?}"));
    let open_tx = cx.update(|_, cx| agent.read(cx).tx_open()).unwrap_or(true);
    check("no transaction left open in the tab", !open_tx, String::new());
    let core = cx.update(|_, cx| ws.read(cx).store.read(cx).core.clone()).unwrap();
    let hist = core.history.load(id, 5).unwrap_or_default();
    check("the history marks the agent's statement", hist.first().is_some_and(|h| h.sql == read_sql && h.source.as_deref() == Some("agent")), format!("{:?}", hist.first().map(|h| (&h.sql, &h.source))));

    if stage == "mcp-tab" {
        return true;
    }

    let (err, _) = call(&agent_ipc,"open_query", json!({"connection": conn, "database": db, "sql": "SELECT count(*) FROM orders"})).await;
    let (err2, _) = call(&agent_ipc,"run_query", json!({"connection": conn, "database": db, "sql": "SELECT count(*) AS customers FROM customer"})).await;
    let n = agent_tabs(ws, cx).len();
    check("open_query and run_query reuse the agent tab", !err && !err2 && n == 1 && text(&agent, cx).contains("customers"), format!("{n} tabs, {}", text(&agent, cx)));
    let editor = cx.update(|_, cx| agent.read(cx).editor.clone()).unwrap();
    let _ = cx.update(|window, cx| {
        let f = editor.focus_handle(cx);
        window.focus(&f, cx);
        editor.update(cx, |e, cx| {
            let end = e.text().len();
            e.set_cursor(end, cx);
        });
    });
    key(cx, "space");
    key(cx, "x");
    let edited = text(&agent, cx);
    let (err, _) = call(&agent_ipc,"open_query", json!({"connection": conn, "database": db, "sql": "SELECT 1 AS another"})).await;
    let tabs = agent_tabs(ws, cx);
    let active_is_users = cx.update(|_, cx| ws.read(cx).active_tab() == Some(&agent)).unwrap_or(false);
    check("a tab the user edited is left alone: a new agent tab", !err && tabs.len() == 2 && text(&agent, cx) == edited && edited.ends_with(" x"), format!("{} tabs", tabs.len()));
    check("the new agent tab opens behind the user's", active_is_users, String::new());

    let several = "SELECT 1 AS one;\nSELECT 'two' AS two;\nSELECT generate_series(1, 4) AS n";
    let (err, out) = call(&agent_ipc, "open_query", json!({"connection": conn, "database": db, "sql": several, "max_rows": 2})).await;
    let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    let rows: Vec<Value> = v["results"].as_array().map(|r| r.iter().map(|x| x["rows"].clone()).collect()).unwrap_or_default();
    check(
        "several reads: the agent gets every result, in order",
        !err && v["statements"] == json!(3) && rows == [json!([[1]]), json!([["two"]]), json!([[1], [2]])] && v["results"][2]["truncated"] == json!("rows") && v["in_tab"] == json!(true),
        out.clone(),
    );
    let shown = agent_tabs(ws, cx).into_iter().find(|t| text(t, cx) == several);
    let tabs_of_run = shown.as_ref().and_then(|t| cx.update(|_, cx| t.read(cx).batch_items().iter().filter(|b| b.rs.as_ref().is_some_and(|r| r.phase().is_finished())).count()).ok());
    check("the agent tab shows them as three result tabs", tabs_of_run == Some(3), format!("{tabs_of_run:?}"));
    let n_rows = shown.as_ref().and_then(|t| cx.update(|_, cx| t.read(cx).batch_items().last().and_then(|b| b.rs.as_ref().map(|r| r.rows()))).ok().flatten());
    check("the whole last result stays in the tab", n_rows == Some(4), format!("{n_rows:?}"));

    let favs = [("Orders by day", "SELECT 'day' AS per;"), ("Orders by month", "SELECT 'month' AS per;"), ("Top customers", "SELECT c.name FROM customer c ORDER BY c.id LIMIT 3;")];
    for (name, sql) in favs {
        let mut f = dataman_core::Favorite::new(name, sql);
        f.connection_id = Some(id.to_string());
        f.database = Some(db.to_string());
        let _ = core.favorites.add(f);
    }
    let (err, out) = call(&agent_ipc, "list_favorites", json!({})).await;
    check("list_favorites has them", !err && favs.iter().all(|(n, _)| out.contains(n)), out);
    let (err, out) = call(&agent_ipc, "run_favorite", json!({"name": "top custmers", "max_rows": 2})).await;
    let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    check("run_favorite runs \"top custmers\" as Top customers", !err && v["favorite"]["name"] == json!("Top customers") && v["row_count"] == json!(2) && v["in_tab"] == json!(true), out.clone());
    let fav_tab = agent_tabs(ws, cx).into_iter().find(|t| text(t, cx) == favs[2].1);
    let fav_rows = fav_tab.as_ref().and_then(|t| cx.update(|_, cx| t.read(cx).result().map(|r| r.rows())).ok().flatten());
    check("its SQL and whole result are in the agent tab", fav_rows == Some(3), format!("{fav_rows:?}"));
    let (err, out) = call(&agent_ipc, "run_favorite", json!({"name": "orders by"})).await;
    let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    let names: Vec<&str> = v["candidates"].as_array().map(|c| c.iter().filter_map(|x| x["name"].as_str()).collect()).unwrap_or_default();
    pause(cx, 100).await;
    let ran = core.history.load(id, 50).unwrap_or_default().iter().any(|h| h.sql.contains("AS per")) || agent_tabs(ws, cx).iter().any(|t| text(t, cx).contains("AS per"));
    check("an unclear name: the candidates, nothing run", !err && v["matched"] == json!(false) && names == ["Orders by day", "Orders by month"] && !ran, out);
    for f in core.favorites.list().into_iter().filter(|f| favs.iter().any(|(n, _)| *n == f.name)) {
        let _ = core.favorites.remove(&f.id);
    }

    if dataman_core::MCP_ADVANCED && !advanced_options(ws, &agent_ipc, id, &conn, db, cx).await {
        check("the advanced MCP options", false, String::new());
    }

    let s = format!("mcp_check_{}", std::process::id());
    let rx = start(&agent_ipc,"execute", json!({"connection": conn, "database": db, "sql": format!("CREATE SCHEMA {s}; CREATE TABLE {s}.t (id int PRIMARY KEY, note text)")}));
    let q = sheet(cx).await;
    check("DDL waits for Allow, asked before it runs", q.as_ref().is_some_and(|q| q.affected.is_none() && q.reasons.iter().any(|r| r == "CREATE SCHEMA") && q.destructive), format!("{q:?}"));
    check("Allow clicked", answer(ws, true, cx), String::new());
    let (err, out) = rx.await.unwrap_or((true, String::new()));
    check("the DDL committed after Allow", !err && out.contains("\"committed\":true") && out.contains("approved"), out);
    check("the sheet closed", !sheet_open(cx), String::new());
    let (err, out) = call(&agent_ipc,"execute", json!({"connection": conn, "database": db, "sql": format!("INSERT INTO {s}.t VALUES (1, 'a'), (2, 'b'), (3, NULL)")})).await;
    check("a safe INSERT commits without a question", !err && !sheet_open(cx) && out.contains("[\"INSERT\",3]"), out);
    let shown = agent_tabs(ws, cx).into_iter().find(|t| cx.update(|_, cx| t.read(cx).result().is_some_and(|r| r.sql.contains("INSERT INTO"))).unwrap_or(false));
    check("the write shows in an agent tab", shown.is_some(), String::new());
    let rx = start(&agent_ipc,"execute", json!({"connection": conn, "database": db, "sql": format!("DELETE FROM {s}.t RETURNING id")}));
    let q = sheet(cx).await;
    check("a DELETE without WHERE is held and asked about", q.as_ref().is_some_and(|q| q.affected == Some(3) && q.reasons.iter().any(|r| r == "DELETE without WHERE") && q.preview.as_ref().is_some_and(|p| p.1.len() == 3)), format!("{q:?}"));
    let front = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.entity_id().as_u64())).ok().flatten();
    check("its agent tab comes to the front", q.as_ref().and_then(|q| q.tab).is_some() && q.as_ref().and_then(|q| q.tab) == front, format!("{front:?}"));
    if stage == "mcp-sheet" {
        return true;
    }
    check("Deny clicked", answer(ws, false, cx), String::new());
    let (err, out) = rx.await.unwrap_or((false, String::new()));
    check("Deny rolls back", err && out.contains("denied") && out.contains("rolled back"), out);
    let (_, out) = call(&agent_ipc,"run_query", json!({"connection": conn, "database": db, "sql": format!("SELECT count(*) AS n FROM {s}.t")})).await;
    check("the rows are still there", out.contains("[[3]]"), out);
    let (err, out) = call(&agent_ipc, "execute", json!({"connection": conn, "database": db, "sql": format!("UPDATE {s}.t SET note = 'x'")})).await;
    check("right after a Deny the same client's next question waits", err && out.contains("just denied") && !sheet_open(cx), out);
    pause(cx, dataman_mcp::hub::AFTER_DENY.as_millis() as u64 + 100).await;
    let rx = start(&agent_ipc,"execute", json!({"connection": conn, "database": db, "sql": format!("UPDATE {s}.t SET note = 'x'")}));
    let q = sheet(cx).await;
    check("an UPDATE without WHERE is asked about", q.is_some_and(|q| q.affected == Some(3)), String::new());
    check("Allow clicked", answer(ws, true, cx), String::new());
    let (err, out) = rx.await.unwrap_or((true, String::new()));
    let (_, n) = call(&agent_ipc,"run_query", json!({"connection": conn, "database": db, "sql": format!("SELECT count(*) AS n FROM {s}.t WHERE note = 'x'")})).await;
    check("Allow commits", !err && n.contains("[[3]]"), format!("{out} / {n}"));

    let delete = format!("DELETE FROM {s}.t");
    if !set_tier(ws, id, dataman_core::McpAccess::Read, cx).await {
        check("the connection goes to tier read", false, String::new());
        return false;
    }
    let tabs_before = agent_tabs(ws, cx);
    let had_result = tabs_before.iter().any(|t| cx.update(|_, cx| t.read(cx).result().is_some()).unwrap_or(false));
    let hist_before = core.history.load(id, 50).unwrap_or_default().len();
    let (err, out) = call(&agent_ipc, "open_query", json!({"connection": conn, "database": db, "sql": delete, "run": false})).await;
    let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    check(
        "a draft answers drafted, its tab, connection and database (no rows)",
        !err && v["drafted"] == json!(true) && v["tab"].as_str().is_some_and(|t| !t.is_empty()) && v["connection"] == json!(conn) && v["database"] == json!(db) && v.get("rows").is_none(),
        out.clone(),
    );
    pause(cx, 200).await;
    draw(cx);
    let tabs = agent_tabs(ws, cx);
    let drafted = tabs.iter().find(|t| text(t, cx) == delete).cloned();
    check("the draft reuses the free agent tab", tabs.len() == tabs_before.len() && drafted.is_some() && had_result, format!("{} → {} tabs", tabs_before.len(), tabs.len()));
    let Some(drafted) = drafted else { return false };
    let facts = cx
        .update(|_, cx| {
            let t = drafted.read(cx);
            let sel = t.editor.read(cx).demo_selection();
            (t.result().is_none() && t.run_phase().is_none() && !t.has_run && !t.is_running(), sel == (delete.len()..delete.len()), t.database().to_string(), t.connection().map(|c| c.profile.id.clone()))
        })
        .ok();
    check(
        "nothing ran: no result left in the tab, the caret at the end, on the connection and database",
        facts.as_ref().is_some_and(|(empty, caret, d, c)| *empty && *caret && d == db && c.as_deref() == Some(id)),
        format!("{facts:?}"),
    );
    check("no approval sheet for a draft", !sheet_open(cx), String::new());
    let hist = core.history.load(id, 50).unwrap_or_default();
    check("no statement in the history", hist.len() == hist_before && !hist.iter().any(|h| h.sql == delete), format!("{hist_before} → {}", hist.len()));
    let (_, out) = call(&agent_ipc, "open_query", json!({"connection": conn, "database": db, "sql": format!("SELECT count(*) AS n FROM {s}.t")})).await;
    check("an agent's run leaves the draft alone; the rows are all there", out.contains("[[3]]") && text(&drafted, cx) == delete && agent_tabs(ws, cx).len() == tabs.len() + 1, out);
    let editor = cx.update(|_, cx| drafted.read(cx).editor.clone()).unwrap();
    let _ = cx.update(|window, cx| {
        ws.update(cx, |w, cx| w.activate_tab(&drafted, window, cx));
        window.focus(&editor.focus_handle(cx), cx);
    });
    key(cx, "cmd-enter");
    pause(cx, 100).await;
    draw(cx);
    let guard = cx
        .update(|_, cx| match &drafted.read(cx).confirm {
            Some(crate::tab::TabConfirm::Guard { findings, .. }) => findings.first().map(|f| f.label.clone()),
            _ => None,
        })
        .ok()
        .flatten();
    check("the user's run meets the app's guard (DELETE without WHERE)", guard.as_deref() == Some("DELETE without WHERE"), format!("{guard:?}"));
    check("not an agent's write: no approval sheet", !sheet_open(cx), String::new());
    key(cx, "escape");
    pause(cx, 100).await;
    let pending = cx.update(|_, cx| drafted.read(cx).confirm.is_some()).unwrap_or(true);
    let (_, out) = call(&agent_ipc, "open_query", json!({"connection": conn, "database": db, "sql": format!("SELECT count(*) AS n FROM {s}.t")})).await;
    check("Cancel: nothing deleted", !pending && out.contains("[[3]]"), out);
    let update = format!("UPDATE {s}.t SET note = 'y' WHERE id = 1");
    let (err, out) = call(&agent_ipc, "open_query", json!({"connection": conn, "database": db, "sql": update, "run": false})).await;
    check("a draft replaces the user's unrun draft", !err && text(&drafted, cx) == update, out);
    let _ = cx.update(|window, cx| {
        ws.update(cx, |w, cx| w.activate_tab(&drafted, window, cx));
        window.focus(&editor.focus_handle(cx), cx);
    });
    key(cx, "cmd-enter");
    settled(&drafted, cx).await;
    let (_, out) = call(&agent_ipc, "open_query", json!({"connection": conn, "database": db, "sql": format!("SELECT count(*) AS n FROM {s}.t WHERE note = 'y'")})).await;
    let hist = core.history.load(id, 50).unwrap_or_default();
    let mine = hist.iter().find(|h| h.sql == update);
    check("the user's run commits, no question asked", out.contains("[[1]]") && !sheet_open(cx), out);
    check("the history has it as the user's", mine.is_some_and(|h| h.source.is_none()), format!("{:?}", mine.map(|h| &h.source)));
    if !set_tier(ws, id, dataman_core::McpAccess::Full, cx).await {
        check("the connection goes back to tier full", false, String::new());
        return false;
    }

    // A sheet that has just appeared takes no Allow: a key pressed for the one before cannot approve it.
    let rx = start(&agent_ipc, "execute", json!({"connection": conn, "database": db, "sql": format!("CREATE TABLE {s}.quick (a int)")}));
    let mut up = false;
    for _ in 0..200 {
        if sheet_open(cx) {
            up = true;
            break;
        }
        pause(cx, 10).await;
    }
    draw(cx);
    key(cx, "cmd-enter");
    pause(cx, 100).await;
    check("⌘↩ the moment a sheet appears does not allow it", up && sheet_open(cx), String::new());
    pause(cx, crate::mcp::ALLOW_AFTER.as_millis() as u64).await;
    draw(cx);
    key(cx, "cmd-enter");
    let (err, out) = rx.await.unwrap_or((true, String::new()));
    check("… a moment later it does", !err && out.contains("approved"), out);

    let rx = start(&agent_ipc,"execute", json!({"connection": conn, "database": db, "sql": format!("DROP SCHEMA {s} CASCADE")}));
    let q = sheet(cx).await;
    key(cx, "cmd-enter");
    let (err, out) = rx.await.unwrap_or((true, String::new()));
    check("⌘↩ allows", q.is_some() && !err, out);
    let log = std::fs::read_to_string(core.dirs.mcp_audit_file()).unwrap_or_default();
    check("the audit has the approvals and the denial", log.contains("\"decision\":\"approved\"") && log.contains("\"decision\":\"denied\""), String::new());

    let update = format!("UPDATE {s}.t SET note = 'x'");
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.toggle_mcp_log(window, cx)));
    let mut found = None;
    for _ in 0..100 {
        draw(cx);
        found = cx
            .update(|_, cx| {
                let log = ws.read(cx).mcp_log()?;
                let ix = log.read(cx).demo_entries().iter().position(|e| e.query.as_deref() == Some(update.as_str()))?;
                Some((log, ix))
            })
            .ok()
            .flatten();
        if found.is_some() {
            break;
        }
        pause(cx, 50).await;
    }
    let Some((log, ix)) = found else {
        check("the log has the UPDATE as sent", false, String::new());
        return false;
    };
    let shown = cx.update(|_, cx| log.read(cx).demo_entries()[ix].sql.clone()).ok().flatten();
    check("the log row shows it redacted", shown.as_deref() == Some(format!("UPDATE {s}.t SET note = ?").as_str()), format!("{shown:?}"));
    let draft_row = cx
        .update(|_, cx| log.read(cx).demo_entries().iter().find(|e| e.draft && e.query.as_deref() == Some(delete.as_str())).map(|e| (crate::mcp::row_what(e), e.sql.clone(), e.outcome)))
        .ok()
        .flatten();
    check(
        "the draft's log row says draft, with its SQL",
        draft_row.as_ref().is_some_and(|(what, sql, o)| what == "draft" && sql.as_deref() == Some(delete.as_str()) && *o == dataman_mcp::audit::Outcome::Ok),
        format!("{draft_row:?}"),
    );
    // Audit narrowed to the UPDATE: its row at the top, its menu right under the click.
    let _ = cx.update(|_, cx| log.update(cx, |a, cx| a.search.update(cx, |s, cx| s.set_text(update.clone(), cx))));
    pause(cx, 100).await;
    draw(cx);
    let Some(row) = cx.update(|_, cx| log.read(cx).demo_row_point(ix)).ok().flatten() else {
        check("the log row is painted", false, String::new());
        return false;
    };
    press(cx, row, MouseButton::Right, 1);
    pause(cx, 30).await;
    let items = cx.update(|_, cx| log.read(cx).demo_menu_items()).unwrap_or_default();
    check("right-click opens the row's menu: Copy Query, Open in Tab", items == [("Copy Query", true), ("Open in Tab", true)], format!("{items:?}"));
    let _ = cx.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(String::new())));
    // Copy Query is the menu's first row (4 pt of padding, 26 pt rows), right under the click.
    press(cx, point(row.x + gpui::px(20.), row.y + gpui::px(4. + 13.)), MouseButton::Left, 1);
    pause(cx, 30).await;
    let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text())).ok().flatten();
    let closed = !cx.update(|_, cx| log.read(cx).demo_menu_open()).unwrap_or(true);
    check("Copy Query puts the full statement on the clipboard", copied.as_deref() == Some(update.as_str()) && closed, format!("{copied:?}"));
    let before = cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap_or(0);
    press(cx, row, MouseButton::Left, 1);
    press(cx, row, MouseButton::Left, 2);
    pause(cx, 50).await;
    draw(cx);
    let tab = cx.update(|_, cx| ws.read(cx).active_tab().cloned()).ok().flatten();
    let after = cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap_or(0);
    let facts = tab.as_ref().and_then(|t| {
        cx.update(|_, cx| {
            let t = t.read(cx);
            (t.editor.read(cx).text().to_string(), t.connection().map(|c| c.profile.id.clone()).or(t.want.clone().map(|w| w.0)), t.database().to_string(), t.has_run || t.run_phase().is_some())
        })
        .ok()
    });
    check(
        "double-click opens it in a new tab, not run, on its connection and database",
        after == before + 1 && facts.as_ref().is_some_and(|(text, conn, database, ran)| *text == format!("{update};") && conn.as_deref() == Some(id) && database == db && !ran),
        format!("{before} → {after} tabs, {facts:?}"),
    );
    let edits = connection_edits(ws, &agent_ipc, id, &conn, db, cx).await;
    ok && edits
}

async fn connection_edits(ws: &Entity<Workspace>, ipc: &Arc<Ipc>, id: &str, conn: &str, db: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut check = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass || detail.is_empty() { String::new() } else { format!(": {detail}") });
        ok &= pass;
    };
    let secret = "pw-never-echoed-42";
    let (err, out) = call(ipc, "create_connection", json!({"name": "agent made", "host": "127.0.0.1", "port": 9, "user": "x", "password": secret})).await;
    let made = serde_json::from_str::<Value>(&out).ok().and_then(|v| v["id"].as_str().map(str::to_string)).unwrap_or_default();
    pause(cx, 150).await;
    draw(cx);
    let shown = cx.update(|_, cx| ws.read(cx).store.read(cx).profile(&made).map(|p| p.name.clone())).ok().flatten();
    let row = cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_conn_row(&made).map(|r| r.0)).ok().flatten();
    check("create_connection: the new connection shows at once (explorer too), no restart", !err && shown.as_deref() == Some("agent made") && row.as_deref() == Some("agent made"), format!("{out} {shown:?} {row:?}"));
    check("… its password is not in the answer", !out.contains(secret), out.clone());
    let (err, out) = call(ipc, "update_connection", json!({"connection": "agent made", "name": "agent renamed", "color": "violet", "password": "another-secret"})).await;
    pause(cx, 150).await;
    draw(cx);
    let row = cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_conn_row(&made).map(|r| r.0)).ok().flatten();
    let v: Value = serde_json::from_str(&out).unwrap_or(Value::Null);
    check(
        "update_connection: the explorer's label follows at once",
        !err && row.as_deref() == Some("agent renamed") && v["changed"] == json!(["name", "color"]) && v["password"] == json!("set"),
        format!("{out} {row:?}"),
    );
    check("… and the password stays out of every answer", !out.contains("another-secret") && !out.contains(secret), out.clone());
    let (_, list) = call(ipc, "list_connections", json!({})).await;
    check("list_connections never shows a password", !list.to_lowercase().contains("password") && !list.contains(secret), String::new());

    let old = cx.update(|_, cx| ws.read(cx).store.read(cx).connection(id)).ok().flatten();
    let tab = cx.update(|_, cx| ws.read(cx).tabs.iter().find(|t| t.read(cx).connection().is_some_and(|c| c.profile.id == id) && !t.read(cx).tx_open()).cloned()).ok().flatten();
    let (err, out) = call(ipc, "update_connection", json!({"connection": conn, "color": "amber"})).await;
    let mut fresh = None;
    for _ in 0..200 {
        pause(cx, 50).await;
        fresh = cx.update(|_, cx| ws.read(cx).store.read(cx).connection(id)).ok().flatten();
        if fresh.as_ref().zip(old.as_ref()).is_some_and(|(a, b)| !Arc::ptr_eq(a, b)) {
            break;
        }
    }
    draw(cx);
    let reconnected = fresh.as_ref().zip(old.as_ref()).is_some_and(|(a, b)| !Arc::ptr_eq(a, b) && a.profile.color.as_deref() == Some("amber"));
    check("a live connection edited reconnects with its new settings", !err && reconnected, out);
    let moved = tab.as_ref().and_then(|t| cx.update(|_, cx| t.read(cx).connection().cloned()).ok().flatten()).zip(fresh.clone()).is_some_and(|(a, b)| Arc::ptr_eq(&a, &b));
    check("… and its idle tabs move to the new connection", moved, String::new());

    let user_tab = cx.update(|window, cx| ws.update(cx, |w, cx| w.new_tab(window, cx))).unwrap();
    let editor = cx.update(|_, cx| user_tab.read(cx).editor.clone()).unwrap();
    let _ = cx.update(|window, cx| {
        user_tab.update(cx, |t, cx| {
            t.set_connection(fresh.clone(), cx);
            t.set_database(db.to_string(), cx);
        });
        editor.update(cx, |e, cx| e.set_text("BEGIN", cx));
        window.focus(&editor.focus_handle(cx), cx);
    });
    key(cx, "cmd-enter");
    settled(&user_tab, cx).await;
    let open = cx.update(|_, cx| user_tab.read(cx).tx_open()).unwrap_or(false);
    let (err, out) = call(ipc, "update_connection", json!({"connection": conn, "color": "teal"})).await;
    check("an edit while a transaction is open on it is refused, with the tab named", open && err && out.contains("transaction open") && out.contains("roll back"), format!("open {open}: {out}"));
    let still = cx.update(|_, cx| ws.read(cx).store.read(cx).profile(id).and_then(|p| p.color.clone())).ok().flatten();
    check("… nothing changed", still.as_deref() == Some("amber") && cx.update(|_, cx| user_tab.read(cx).tx_open()).unwrap_or(false), format!("{still:?}"));
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.end_active_tx(false, cx)));
    for _ in 0..100 {
        pause(cx, 50).await;
        if !cx.update(|_, cx| user_tab.read(cx).tx_open()).unwrap_or(true) {
            break;
        }
    }
    let (err, out) = call(ipc, "update_connection", json!({"connection": conn, "color": "teal"})).await;
    check("rolled back: the edit goes through", !err && out.contains("color"), out);

    // Moving a connection to another server, and reading files of this Mac, wait for the user.
    let rx = start(ipc, "update_connection", json!({"connection": "agent renamed", "host": "10.9.9.9"}));
    let q = sheet(cx).await;
    check("a move to another server asks, showing the change", q.as_ref().is_some_and(|q| q.sql.contains("host: 127.0.0.1 → 10.9.9.9") && q.reasons.iter().any(|r| r.contains("another server"))), format!("{q:?}"));
    answer(ws, false, cx);
    let (err, out) = rx.await.unwrap_or((false, String::new()));
    let host = cx.update(|_, cx| ws.read(cx).store.read(cx).profile(&made).map(|p| p.host.clone())).ok().flatten();
    check("… denied, it stays where it was", err && host.as_deref() == Some("127.0.0.1"), format!("{out} {host:?}"));
    pause(cx, dataman_mcp::hub::AFTER_DENY.as_millis() as u64 + 100).await;

    let dir = std::env::temp_dir().join(format!("dataman-mcp-check-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("imported by agent.sql");
    let _ = std::fs::write(&file, "SELECT 'from a file'");
    let rx = start(ipc, "import_favorites", json!({"paths": [file.display().to_string()]}));
    let q = sheet(cx).await;
    check("import_favorites asks before reading, naming the file", q.as_ref().is_some_and(|q| q.connection == "Import favorites" && q.connection_id.is_empty() && q.sql.contains("imported by agent.sql")), format!("{q:?}"));
    answer(ws, false, cx);
    let (err, out) = rx.await.unwrap_or((false, String::new()));
    let (_, list) = call(ipc, "list_favorites", json!({})).await;
    check("… denied, nothing is read", err && out.contains("denied") && !list.contains("from a file"), out);
    pause(cx, dataman_mcp::hub::AFTER_DENY.as_millis() as u64 + 100).await;
    let rx = start(ipc, "import_favorites", json!({"paths": [file.display().to_string()]}));
    let _ = sheet(cx).await;
    answer(ws, true, cx);
    let (err, out) = rx.await.unwrap_or((true, String::new()));
    let (_, list) = call(ipc, "list_favorites", json!({})).await;
    check("… allowed, it is a favorite", !err && list.contains("from a file"), out);
    let _ = std::fs::remove_dir_all(&dir);
    ok
}

fn press(cx: &mut AsyncWindowContext, p: Point<Pixels>, button: MouseButton, count: usize) {
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: None, modifiers: Modifiers::default() }));
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button, position: p, modifiers: Modifiers::default(), click_count: count, first_mouse: false }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button, position: p, modifiers: Modifiers::default(), click_count: count }));
}
