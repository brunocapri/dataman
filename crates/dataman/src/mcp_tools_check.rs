//! `mcp-tools-check`: the real `dataman-mcp` binary over stdio (JSON-RPC), reaching this app
//! through its socket: tools/list, then every tool on PostgreSQL, MySQL and SQL Server and the
//! NoSQL tools on MongoDB, Elasticsearch, OpenSearch and Redis; the tiers set by clicking
//! Settings › MCP; destructive writes answered with Deny and Allow on the sheet; drafts in the
//! agent tab; show_in_dataman to every destination.

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use gpui::{AsyncWindowContext, Entity};
use serde_json::{Value, json};
use dataman_core::config::{ConnectionProfile, Engine};

use crate::app::Workspace;
use crate::check::*;

const TOOLS: [&str; 28] = [
    "list_connections", "list_schemas", "list_tables", "search_objects", "describe_table", "get_ddl", "sample_rows",
    "create_connection", "update_connection", "create_agent_login", "show_in_dataman",
    "run_query", "open_query", "explain", "explain_analyze", "execute",
    "list_favorites", "run_favorite", "add_favorite", "remove_favorite", "rename_favorite", "import_favorites",
    "nosql_query", "nosql_execute",
    "activity_sessions", "activity_locks", "cancel_backend", "terminate_backend",
];

pub(crate) struct Client {
    child: Child,
    io: Mutex<(ChildStdin, BufReader<ChildStdout>, u64)>,
}

impl Client {
    /// Started and introduced, as an MCP client does; what it answered to initialize.
    pub(crate) async fn ready(config: &std::path::Path) -> Result<(Arc<Client>, Value), String> {
        let c = Arc::new(Client::start(config)?);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let t = c.clone();
        std::thread::spawn(move || {
            let i = t.request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}}));
            t.notify("notifications/initialized");
            let _ = tx.send(i);
        });
        let init = rx.await.map_err(|e| e.to_string())??;
        Ok((c, init))
    }

    fn start(config: &std::path::Path) -> Result<Client, String> {
        let bin = std::env::current_exe().map_err(|e| e.to_string())?.with_file_name(format!("dataman-mcp{}", std::env::consts::EXE_SUFFIX));
        // The binary finds the app under $XDG_CONFIG_HOME/dataman: there, a link to this run's
        // own config directory.
        let home = std::env::temp_dir().join(format!("dataman-e2e-xdg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(config, home.join("dataman")).map_err(|e| e.to_string())?;
        // A junction needs no privilege, unlike a symbolic link.
        #[cfg(windows)]
        Command::new("cmd").args(["/C", "mklink", "/J"]).arg(home.join("dataman")).arg(config).output().map_err(|e| e.to_string())?;
        let mut child = Command::new(&bin)
            .env("XDG_CONFIG_HOME", &home)
            .env("DATAMAN_MCP_SECRETS", "memory")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("{}: {e}", bin.display()))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Ok(Client { child, io: Mutex::new((stdin, stdout, 0)) })
    }

    /// Blocking: one request, its response (notifications skipped).
    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut io = self.io.lock().unwrap();
        io.2 += 1;
        let id = io.2;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        writeln!(io.0, "{line}").and_then(|_| io.0.flush()).map_err(|e| e.to_string())?;
        loop {
            let mut buf = String::new();
            if io.1.read_line(&mut buf).map_err(|e| e.to_string())? == 0 {
                return Err("the server closed stdout".into());
            }
            let Ok(v) = serde_json::from_str::<Value>(&buf) else { continue };
            if v["id"] == json!(id) {
                return if v.get("error").is_some() { Err(v["error"].to_string()) } else { Ok(v["result"].clone()) };
            }
        }
    }

    fn notify(&self, method: &str) {
        let mut io = self.io.lock().unwrap();
        let _ = writeln!(io.0, "{}", json!({"jsonrpc": "2.0", "method": method}));
        let _ = io.0.flush();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// tools/call on a thread of its own (the app answers on this one): (is_error, text).
pub(crate) async fn call(c: &Arc<Client>, tool: &str, args: Value, _: &mut AsyncWindowContext) -> (bool, String) {
    let rx = start(c, tool, args);
    rx.await.unwrap_or((true, "the call thread went away".into()))
}

fn start(c: &Arc<Client>, tool: &str, args: Value) -> tokio::sync::oneshot::Receiver<(bool, String)> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let (c, tool) = (c.clone(), tool.to_string());
    std::thread::spawn(move || {
        let r = match c.request("tools/call", json!({"name": tool, "arguments": args})) {
            Ok(v) => (v["isError"] == json!(true), v["content"][0]["text"].as_str().unwrap_or_default().to_string()),
            Err(e) => (true, e),
        };
        let _ = tx.send(r);
    });
    rx
}

fn ok(r: &(bool, String), has: &str) -> bool {
    !r.0 && r.1.contains(has)
}

fn short(r: &(bool, String)) -> String {
    format!("error={} {}", r.0, r.1.chars().take(300).collect::<String>())
}

/// The connection's sheet: its MCP tier clicked, saved with ⌘↩.
async fn tier(ws: &Entity<Workspace>, id: &str, a: dataman_core::McpAccess, cx: &mut AsyncWindowContext) -> bool {
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dialog(Some(id.to_string()), window, cx)));
    pause(cx, 150).await;
    let clicked = click_probe(&format!("mcp-{}", a.label()), cx);
    key(cx, "cmd-enter");
    let saved = until(cx, 5, |cx| cx.update(|_, cx| ws.read(cx).dialog.is_none()).unwrap_or(false)).await;
    let _ = connected(ws, id, 20, cx).await;
    let now = core(ws, cx).config.get(id).and_then(|p| p.mcp);
    clicked && saved && now == Some(a)
}

async fn sheet_answer(ws: &Entity<Workspace>, allow: bool, cx: &mut AsyncWindowContext) -> Option<String> {
    let req = crate::mcp_check::sheet(cx).await?;
    crate::mcp_check::answer(ws, allow, cx);
    Some(format!("{req:?}"))
}

pub(crate) fn forward(cx: &mut AsyncWindowContext) -> usize {
    cx.update(|_, cx| cx.try_global::<crate::mcp::McpHost>().map_or(0, |h| h.forward)).unwrap_or(0)
}

fn dialog(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Entity<crate::conn_dialog::ConnDialog>> {
    cx.update(|_, cx| ws.read(cx).dialog.clone()).ok().flatten()
}

async fn close_dialog(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    key(cx, "escape");
    until(cx, 5, |cx| dialog(ws, cx).is_none()).await
}

fn agents_at(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<(crate::agents::Page, Option<String>)> {
    cx.update(|_, cx| (ws.read(cx).view == dataman_core::layout::NavView::Agents).then(|| ws.read(cx).agents.read(cx)).map(|a| (a.page, a.conn.clone()))).ok().flatten()
}

/// Each destination opens its place with the window brought forward, and the file of connections
/// stays as it was until the user's click. What is closed to agents goes nowhere.
async fn show_destinations(ws: &Entity<Workspace>, client: &Arc<Client>, pg: &ConnectionProfile, my: &ConnectionProfile, r: &mut Report, cx: &mut AsyncWindowContext) {
    use crate::agents::Page;
    let core = core(ws, cx);
    let Some(hub) = cx.update(|_, cx| crate::mcp::shared(&core, cx).hub().ok().map(|h| h.0)).ok().flatten() else {
        r.check("the app's hub", false, "");
        return;
    };
    hub.set_show_limit(100, std::time::Duration::from_secs(60));
    let show = |dest: &str| json!({"destination": dest});
    let on = |dest: &str, c: &str| json!({"destination": dest, "connection": c});
    let file = || std::fs::read_to_string(core.dirs.connections_file()).unwrap_or_default();

    let res = call(client, "create_connection", json!({"name": "e2e no password", "host": "127.0.0.1", "port": 5998, "user": "x"}), cx).await;
    r.check("create_connection without a password", !res.0, short(&res));
    until(cx, 5, |cx| cx.update(|_, cx| ws.read(cx).store.read(cx).profiles.iter().any(|p| p.name == "e2e no password")).unwrap_or(false)).await;
    let saved = file();

    let fronted = forward(cx);
    let res = call(client, "show_in_dataman", json!({"destination": "new_connection", "name": "e2e filled", "engine": "mysql", "host": "db.example.internal", "port": 3307, "user": "app", "database": "orders"}), cx).await;
    let open = until(cx, 5, |cx| dialog(ws, cx).is_some() && probed("dlg-agent-made", cx).is_some()).await;
    let shown = dialog(ws, cx).and_then(|d| cx.update(|_, cx| d.read(cx).current(cx)).ok());
    let right = shown.as_ref().is_some_and(|(p, pw)| (p.name.as_str(), p.engine, p.host.as_str(), p.port, p.user.as_str(), p.database.as_str()) == ("e2e filled", Engine::MySql, "db.example.internal", 3307, "app", "orders") && pw.is_empty());
    let step = dialog(ws, cx).and_then(|d| cx.update(|_, cx| d.read(cx).step()).ok());
    let string = dialog(ws, cx).and_then(|d| cx.update(|_, cx| d.read(cx).demo_conn_str(cx)).ok()).unwrap_or_default();
    r.check("…its connection string says the TLS its fields say", string.contains("db.example.internal:3307") && string.contains("ssl-mode=REQUIRED"), &string);
    r.check("new_connection: the dialog filled in, no password, says an agent filled it, window forward", !res.0 && open && right && step == Some(crate::conn_dialog::Step::Connection) && forward(cx) == fronted + 1, format!("{} {open} {step:?} {:?}", short(&res), shown.map(|s| s.0.name)));
    crate::outside_check::snap("agent-filled", cx);
    // The user's sheet is theirs: no agent replaces it or moves them away.
    let res = call(client, "show_in_dataman", show("agents_general"), cx).await;
    let still = dialog(ws, cx).and_then(|d| cx.update(|_, cx| d.read(cx).current(cx).0.name).ok());
    r.check("…and another show_in_dataman while it is open is refused, the dialog as it was", res.0 && res.1.contains("sheet is open") && still.as_deref() == Some("e2e filled") && agents_at(ws, cx).is_none() && forward(cx) == fronted + 1, short(&res));
    let closed = close_dialog(ws, cx).await;
    r.check("…closed without saving, nothing is saved", closed && file() == saved && !core.config.connections().iter().any(|p| p.name == "e2e filled"), "");

    // A saved connection's dialog: at its password where it has none.
    let res = call(client, "show_in_dataman", on("connection", "e2e no password"), cx).await;
    let open = until(cx, 5, |cx| dialog(ws, cx).is_some()).await;
    let d = dialog(ws, cx);
    let asks = d.as_ref().and_then(|d| cx.update(|window, cx| (d.read(cx).demo_asks_password(), d.read(cx).password_focus(cx).is_focused(window), d.read(cx).current(cx).0.name)).ok());
    r.check("connection: its dialog at the password, for one with none saved, marked as an agent's", !res.0 && open && asks == Some((true, true, "e2e no password".into())) && probed("dlg-agent-made", cx).is_some() && forward(cx) == fronted + 2, format!("{} {asks:?}", short(&res)));
    r.check("…closed, nothing saved", close_dialog(ws, cx).await && file() == saved, "");
    let res = call(client, "show_in_dataman", on("connection", "e2e renamed by agent"), cx).await;
    let open = until(cx, 5, |cx| dialog(ws, cx).is_some()).await;
    let asks = dialog(ws, cx).and_then(|d| cx.update(|_, cx| d.read(cx).demo_asks_password()).ok());
    r.check("connection: with a password saved, the dialog as it is", !res.0 && open && asks == Some(false), format!("{} {asks:?}", short(&res)));
    close_dialog(ws, cx).await;
    let res = call(client, "show_in_dataman", on("connection", &pg.name), cx).await;
    let open = until(cx, 5, |cx| dialog(ws, cx).is_some()).await;
    r.check("…and one the user saved has no agent's mark", !res.0 && open && probed("dlg-agent-made", cx).is_none(), short(&res));
    close_dialog(ws, cx).await;

    let fronted = forward(cx);
    let res = call(client, "show_in_dataman", on("agents_protection", &pg.name), cx).await;
    let there = until(cx, 5, |cx| agents_at(ws, cx) == Some((Page::Protection, Some(pg.id.clone())))).await;
    r.check("agents_protection: Protection, on that connection, window forward", !res.0 && there && forward(cx) == fronted + 1, format!("{} {:?}", short(&res), agents_at(ws, cx)));
    let res = call(client, "show_in_dataman", on("agent_login", &my.name), cx).await;
    let there = until(cx, 5, |cx| agents_at(ws, cx) == Some((Page::Protection, Some(my.id.clone()))) && probed("agents-login-section", cx).is_some()).await;
    r.check("agent_login: Protection on that connection, at its agent login", !res.0 && there && forward(cx) == fronted + 2, format!("{} {:?}", short(&res), agents_at(ws, cx)));
    let res = call(client, "show_in_dataman", show("agents_general"), cx).await;
    let there = until(cx, 5, |cx| agents_at(ws, cx).is_some_and(|a| a.0 == Page::General)).await;
    r.check("agents_general: Agents › General", !res.0 && there && forward(cx) == fronted + 3, short(&res));
    let res = call(client, "show_in_dataman", show("settings_mcp"), cx).await;
    let at = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).settings_view.as_ref().map(|v| v.read(cx).demo_section())).ok().flatten();
    let there = until(cx, 5, |cx| at(cx) == Some("MCP")).await;
    r.check("settings_mcp: Settings › MCP", !res.0 && there && forward(cx) == fronted + 4, short(&res));
    key(cx, "escape");
    until(cx, 5, |cx| at(cx).is_none()).await;
    let res = call(client, "show_in_dataman", show("outside_review"), cx).await;
    let up = cx.update(|_, cx| ws.read(cx).outside_review.is_some()).unwrap_or(true);
    r.check("outside_review with nothing waiting says so, and nothing comes forward", res.0 && res.1.contains("nothing edited") && !up && forward(cx) == fronted + 4, short(&res));
    r.check("nothing any of these did saved anything", file() == saved, "");

    // Closed to agents: nowhere to go, and nothing moves.
    let res = call(client, "update_connection", json!({"connection": "e2e renamed by agent", "mcp": "off"}), cx).await;
    let shut = core.config.connections().into_iter().find(|p| p.name == "e2e no password");
    if let Some(mut p) = shut {
        p.agents_refused = true;
        let _ = core.config.upsert(p);
    }
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.connections_on_disk(window, cx)));
    r.check("a connection turned off", !res.0, short(&res));
    let (fronted, at_before) = (forward(cx), agents_at(ws, cx));
    let mut went = Vec::new();
    for name in ["e2e renamed by agent", "e2e no password", "no such connection"] {
        for dest in ["connection", "agents_protection", "agent_login"] {
            let res = call(client, "show_in_dataman", on(dest, name), cx).await;
            if !res.0 {
                went.push(format!("{dest} {name}"));
            }
        }
    }
    r.check("MCP off, agents refused or no such name: refused everywhere, the app where it was", went.is_empty() && dialog(ws, cx).is_none() && agents_at(ws, cx) == at_before && forward(cx) == fronted, format!("{went:?}"));

    // A twin of a closed connection: the same answer as any other, and it starts closed. Only the
    // user's click saves it, and Skip keeps it closed.
    let res = call(client, "show_in_dataman", json!({"destination": "new_connection", "name": "e2e twin", "host": "127.0.0.1", "port": 5999, "user": "x"}), cx).await;
    let open = until(cx, 5, |cx| dialog(ws, cx).is_some()).await;
    let d = dialog(ws, cx);
    let tier = d.as_ref().and_then(|d| cx.update(|_, cx| d.read(cx).demo_tier()).ok());
    r.check("the twin of a closed connection: answered like any other, MCP off in the dialog", !res.0 && open && tier == Some(dataman_core::McpAccess::Off), format!("{} {tier:?}", short(&res)));
    let walked = match &d {
        Some(d) => sheet_to_last_step(d, cx).await,
        None => false,
    };
    let skipped = walked && click_probe("dlg-skip", cx);
    let saved_twin = until(cx, 10, |_| core.config.connections().iter().any(|p| p.name == "e2e twin")).await;
    let twin = core.config.connections().into_iter().find(|p| p.name == "e2e twin");
    r.check("…the user's click saves it, Skip leaves agents out, and it is the user's now", skipped && saved_twin && twin.as_ref().is_some_and(|p| p.mcp_access() == dataman_core::McpAccess::Off && !p.agent_made), format!("{walked} {skipped} {:?}", twin.map(|p| (p.mcp, p.agent_made))));
    until(cx, 5, |cx| dialog(ws, cx).is_none()).await;

    let fronted = forward(cx);
    let _ = core.settings.update(|s| s.mcp_enabled = false);
    let res = call(client, "show_in_dataman", show("settings_mcp"), cx).await;
    let up = cx.update(|_, cx| ws.read(cx).settings_view.is_some()).unwrap_or(true);
    r.check("MCP switched off in Settings: refused too", res.0 && res.1.contains("turned off") && !up && forward(cx) == fronted, short(&res));
    let _ = core.settings.update(|s| s.mcp_enabled = true);

    // A looping agent: past the limit it stops taking the screen.
    let at_before = agents_at(ws, cx);
    hub.set_show_limit(1, std::time::Duration::from_secs(60));
    let res = call(client, "show_in_dataman", show("settings_mcp"), cx).await;
    let up = cx.update(|_, cx| ws.read(cx).settings_view.is_some()).unwrap_or(true);
    r.check("past the limit: refused, nothing comes forward, nothing moves", res.0 && res.1.contains("came forward") && !up && agents_at(ws, cx) == at_before && forward(cx) == fronted, short(&res));
    hub.set_show_limit(dataman_mcp::hub::SHOWS, dataman_mcp::hub::SHOWS_WITHIN);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.show_nav(dataman_core::layout::NavView::Explorer, window, cx)));
}

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let mut profiles: Vec<(Engine, ConnectionProfile, String)> = Vec::new();
    for e in [Engine::Postgres, Engine::MySql, Engine::SqlServer, Engine::MongoDb, Engine::Elasticsearch, Engine::OpenSearch, Engine::Redis] {
        let (mut p, pw) = profile(e);
        p.name = format!("e2e-{}", e.label().to_lowercase().replace(' ', ""));
        let up = connect(ws, &p, &pw, cx).await;
        r.check(&format!("{} connects", e.label()), up.is_ok(), format!("{up:?}"));
        profiles.push((e, p, pw));
    }
    let core = core(ws, cx);
    let (pg, pgpw) = (profiles[0].1.clone(), profiles[0].2.clone());
    let _ = sql(&core, &pg, &pgpw, "DROP SCHEMA IF EXISTS e2e_mcp CASCADE", cx).await;
    let _ = sql(&core, &pg, &pgpw, "CREATE SCHEMA e2e_mcp", cx).await;
    let _ = sql(&core, &pg, &pgpw, "CREATE TABLE e2e_mcp.t (id int PRIMARY KEY, note text)", cx).await;
    let (mo, mopw) = (profiles[3].1.clone(), profiles[3].2.clone());
    let _ = nosql(&core, &mo, &mopw, "e2e_mcp", "db.dropDatabase()", cx).await;
    let _ = nosql(&core, &mo, &mopw, "e2e_mcp", "db.items.insertMany([{n: 1}, {n: 2}, {n: 3}])", cx).await;
    let (rd, rdpw) = (profiles[6].1.clone(), profiles[6].2.clone());
    let _ = nosql(&core, &rd, &rdpw, "0", "SET e2e:mcp hello", cx).await;
    for (e, p, pw) in &profiles {
        if matches!(e, Engine::Elasticsearch | Engine::OpenSearch) {
            let _ = nosql(&core, p, pw, "", "DELETE /e2e_mcp", cx).await;
            let _ = nosql(&core, p, pw, "", "POST /e2e_mcp/_doc/1?refresh=true\n{\"n\": 1}", cx).await;
        }
    }

    key(cx, "cmd-,");
    pause(cx, 150).await;
    click_probe("MCP", cx);
    click_probe("mcp-on", cx);
    click_probe("mcp-tabs-on", cx);
    key(cx, "escape");
    let enabled = core.settings.get().mcp_enabled;
    let _ = cx.update(|_, cx| crate::mcp::apply(&core, cx));
    let mut tiers = true;
    for (_, p, _) in &profiles {
        tiers &= tier(ws, &p.id, dataman_core::McpAccess::Write, cx).await;
    }
    r.check("Settings › MCP turns agent access on; each connection's sheet sets its tier to Write", enabled && tiers, format!("{enabled} {tiers}"));

    let (client, i) = match Client::ready(&core.dirs.config).await {
        Ok(c) => c,
        Err(e) => {
            r.check("dataman-mcp starts", false, e);
            return;
        }
    };
    let (tools, prompt) = {
        let c = client.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send((c.request("tools/list", json!({})), c.request("prompts/get", json!({"name": "quickstart"}))));
        });
        match rx.await {
            Ok(got) => got,
            Err(e) => (Err(e.to_string()), Err(String::new())),
        }
    };
    let Ok(t) = tools else {
        r.check("dataman-mcp answers tools/list", false, format!("{tools:?}"));
        return;
    };
    r.check("the quickstart prompt, also in the instructions", prompt.as_ref().is_ok_and(|p| p["messages"][0]["content"]["text"].as_str().is_some_and(|q| q.contains("show_in_dataman") && i["instructions"].as_str().is_some_and(|s| s.contains(q)))), format!("{prompt:?}"));
    let mut names: Vec<String> = t["tools"].as_array().map(|a| a.iter().filter_map(|t| t["name"].as_str().map(str::to_string)).collect()).unwrap_or_default();
    names.sort();
    let mut want: Vec<String> = TOOLS.iter().map(|s| s.to_string()).collect();
    want.sort();
    r.check("dataman-mcp over stdio: initialize, and tools/list has all 28 tools", i["serverInfo"]["name"].is_string() && names == want, format!("{names:?}"));

    let listed = call(&client, "list_connections", json!({}), cx).await;
    r.check("list_connections (through the app) lists the app's connections", profiles.iter().all(|(_, p, _)| listed.1.contains(&p.name)), short(&listed));

    for (e, p, _) in profiles.iter().filter(|(e, _, _)| e.is_sql()) {
        let (db, schema, table) = match e {
            Engine::Postgres => (p.database.as_str(), "public", "customer"),
            Engine::MySql => (p.database.as_str(), p.database.as_str(), "customer"),
            _ => (p.database.as_str(), "dbo", "customer"),
        };
        let c = json!(p.name);
        let label = e.label();
        let res = call(&client, "list_schemas", json!({"connection": c, "database": db}), cx).await;
        r.check(&format!("{label}: list_schemas"), ok(&res, schema), short(&res));
        let res = call(&client, "list_tables", json!({"connection": c, "database": db, "schema": schema}), cx).await;
        r.check(&format!("{label}: list_tables"), ok(&res, table), short(&res));
        let res = call(&client, "search_objects", json!({"connection": c, "database": db, "pattern": "cust"}), cx).await;
        r.check(&format!("{label}: search_objects"), ok(&res, table), short(&res));
        let res = call(&client, "describe_table", json!({"connection": c, "database": db, "table": format!("{schema}.{table}")}), cx).await;
        r.check(&format!("{label}: describe_table"), ok(&res, "\"name\""), short(&res));
        let res = call(&client, "get_ddl", json!({"connection": c, "database": db, "object": format!("{schema}.{table}")}), cx).await;
        r.check(&format!("{label}: get_ddl"), ok(&res, "CREATE TABLE"), short(&res));
        let res = call(&client, "sample_rows", json!({"connection": c, "database": db, "table": format!("{schema}.{table}"), "n": 3}), cx).await;
        r.check(&format!("{label}: sample_rows"), ok(&res, "rows"), short(&res));
        let res = call(&client, "run_query", json!({"connection": c, "database": db, "sql": format!("SELECT count(*) AS n FROM {schema}.{table}")}), cx).await;
        r.check(&format!("{label}: run_query"), ok(&res, "rows"), short(&res));
        let res = call(&client, "explain", json!({"connection": c, "database": db, "sql": format!("SELECT * FROM {schema}.{table}")}), cx).await;
        // Plans are PostgreSQL's (the app has no MySQL / SQL Server plan view): said plainly.
        let want = if *e == Engine::Postgres { !res.0 && !res.1.is_empty() } else { res.0 && res.1.contains("not available") };
        r.check(&format!("{label}: explain"), want, short(&res));
        let res = call(&client, "run_query", json!({"connection": c, "database": db, "sql": format!("DELETE FROM {schema}.{table}")}), cx).await;
        r.check(&format!("{label}: run_query refuses a write"), res.0, short(&res));
    }

    let c = json!(pg.name);
    let res = call(&client, "explain_analyze", json!({"connection": c, "sql": "SELECT count(*) FROM orders"}), cx).await;
    r.check("PostgreSQL: explain_analyze", ok(&res, "actual"), short(&res));
    let res = call(&client, "execute", json!({"connection": c, "sql": "INSERT INTO e2e_mcp.t VALUES (1, 'by agent')"}), cx).await;
    let n = sql1(&core, &pg, &pgpw, "SELECT count(*) FROM e2e_mcp.t", cx).await;
    r.check("tier Write: execute runs an INSERT", !res.0 && n.as_deref() == Some("1"), format!("{} {n:?}", short(&res)));
    let res = call(&client, "execute", json!({"connection": c, "sql": "DROP TABLE e2e_mcp.t"}), cx).await;
    r.check("tier Write: a DROP is refused", res.0 && sql1(&core, &pg, &pgpw, "SELECT count(*) FROM e2e_mcp.t", cx).await.is_some(), short(&res));

    tier(ws, &pg.id, dataman_core::McpAccess::Read, cx).await;
    let res = call(&client, "execute", json!({"connection": c, "sql": "INSERT INTO e2e_mcp.t VALUES (2, 'x')"}), cx).await;
    r.check("tier Read: execute is refused", res.0, short(&res));
    tier(ws, &pg.id, dataman_core::McpAccess::Off, cx).await;
    let res = call(&client, "run_query", json!({"connection": c, "sql": "SELECT 1"}), cx).await;
    r.check("tier Off: even a read is refused", res.0, short(&res));

    tier(ws, &pg.id, dataman_core::McpAccess::Full, cx).await;
    let rx = start(&client, "execute", json!({"connection": c, "sql": "TRUNCATE e2e_mcp.t"}));
    let asked = sheet_answer(ws, false, cx).await;
    let res = rx.await.unwrap_or((true, String::new()));
    let n = sql1(&core, &pg, &pgpw, "SELECT count(*) FROM e2e_mcp.t", cx).await;
    r.check("tier Full: a TRUNCATE waits on the sheet; Deny refuses it", asked.is_some() && res.0 && n.as_deref() == Some("1"), format!("{asked:?} {} {n:?}", short(&res)));
    // Right after a Deny the same client waits before asking again.
    pause(cx, dataman_mcp::hub::AFTER_DENY.as_millis() as u64 + 100).await;
    let rx = start(&client, "execute", json!({"connection": c, "sql": "TRUNCATE e2e_mcp.t"}));
    let asked = sheet_answer(ws, true, cx).await;
    let res = rx.await.unwrap_or((true, String::new()));
    let n = sql1(&core, &pg, &pgpw, "SELECT count(*) FROM e2e_mcp.t", cx).await;
    r.check("…and Allow runs it", asked.is_some() && !res.0 && n.as_deref() == Some("0"), format!("{} {n:?}", short(&res)));

    let before = tabs(ws, cx).len();
    let res = call(&client, "open_query", json!({"connection": c, "sql": "SELECT 'agent' AS who"}), cx).await;
    pause(cx, 300).await;
    let agent = cx.update(|_, cx| ws.read(cx).tabs.iter().find(|t| t.read(cx).agent.is_some()).cloned()).ok().flatten();
    let shown = agent.as_ref().map(|t| cell(t, 0, 0, cx));
    r.check("open_query runs in an agent tab the user sees", !res.0 && agent.is_some() && shown == Some(Some("agent".into())), format!("{} tabs {} → {} {shown:?}", short(&res), before, tabs(ws, cx).len()));
    for (e, p, _) in profiles.iter().filter(|(e, _, _)| matches!(e, Engine::MySql | Engine::Redis)) {
        let stmt = if *e == Engine::Redis { "DEL e2e:mcp" } else { "DELETE FROM customer WHERE id = 1" };
        let res = call(&client, "open_query", json!({"connection": p.name, "sql": stmt, "run": false}), cx).await;
        pause(cx, 200).await;
        let text = cx.update(|_, cx| ws.read(cx).tabs.iter().filter(|t| t.read(cx).agent.is_some()).map(|t| t.read(cx).editor.read(cx).text().to_string()).collect::<Vec<_>>()).unwrap_or_default();
        r.check(&format!("{}: open_query run:false drafts it in the agent tab, unrun", e.label()), !res.0 && text.iter().any(|t| t.contains(stmt)), format!("{} {text:?}", short(&res)));
    }

    for (e, p, _) in profiles.iter().filter(|(e, _, _)| !e.is_sql()) {
        let (db, read, write, has) = match e {
            Engine::MongoDb => ("e2e_mcp", "db.items.find({n: {$gt: 1}})", "db.items.insertOne({n: 4})", "\"n\""),
            Engine::Redis => ("0", "GET e2e:mcp", "SET e2e:mcp2 x", "hello"),
            _ => ("", "GET /e2e_mcp/_search", "POST /e2e_mcp/_doc/2?refresh=true\n{\"n\": 2}", "\"n\""),
        };
        let label = e.label();
        let res = call(&client, "nosql_query", json!({"connection": p.name, "database": db, "query": read}), cx).await;
        r.check(&format!("{label}: nosql_query"), ok(&res, has), short(&res));
        let res = call(&client, "nosql_execute", json!({"connection": p.name, "database": db, "query": write}), cx).await;
        r.check(&format!("{label}: nosql_execute writes (tier Write)"), !res.0, short(&res));
    }

    let res = call(&client, "add_favorite", json!({"name": "e2e fav", "sql": "SELECT 7 AS seven", "connection": pg.name}), cx).await;
    let list = call(&client, "list_favorites", json!({}), cx).await;
    let id = core.favorites.list().into_iter().find(|f| f.name == "e2e fav").map(|f| f.id);
    r.check("add_favorite then list_favorites", !res.0 && list.1.contains("e2e fav") && id.is_some(), short(&list));
    let id = id.unwrap_or_default();
    let res = call(&client, "rename_favorite", json!({"id": id, "name": "e2e seven"}), cx).await;
    r.check("rename_favorite", !res.0 && core.favorites.list().iter().any(|f| f.name == "e2e seven"), short(&res));
    tier(ws, &pg.id, dataman_core::McpAccess::Write, cx).await;
    let res = call(&client, "run_favorite", json!({"name": "e2e seven"}), cx).await;
    r.check("run_favorite runs it by name", ok(&res, "7"), short(&res));
    let res = call(&client, "remove_favorite", json!({"id": id}), cx).await;
    r.check("remove_favorite", !res.0 && !core.favorites.list().iter().any(|f| f.name == "e2e seven"), short(&res));
    let file = std::env::temp_dir().join(format!("e2e-imported-{}.sql", std::process::id()));
    let _ = std::fs::write(&file, "SELECT 'imported' AS origin");
    let rx = start(&client, "import_favorites", json!({"paths": [file.display().to_string()]}));
    let asked = sheet_answer(ws, true, cx).await;
    let res = rx.await.unwrap_or((true, String::new()));
    r.check("import_favorites asks, then adds the .sql file", asked.is_some() && !res.0 && core.favorites.list().iter().any(|f| f.sql.contains("'imported'")), short(&res));
    let _ = std::fs::remove_file(&file);

    let res = call(&client, "activity_sessions", json!({"connection": pg.name}), cx).await;
    r.check("activity_sessions", ok(&res, "pid"), short(&res));
    let res = call(&client, "activity_locks", json!({"connection": pg.name}), cx).await;
    r.check("activity_locks", !res.0, short(&res));
    // A session of another program (cancel_backend refuses Dataman's own): psql here, or in
    // the container that serves the port.
    let sleep_sql = "SELECT pg_sleep(30) /* e2e sleeper */";
    let mut sleeper = Command::new("psql")
        .env("PGPASSWORD", &pgpw)
        .args(["-h", &pg.host, "-p", &pg.port.to_string(), "-U", &pg.user, "-d", &pg.database, "-c", sleep_sql])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .or_else(|_| {
            let out = Command::new("docker").args(["ps", "--filter", &format!("publish={}", pg.port), "--format", "{{.Names}}"]).output()?;
            let name = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or_default().to_string();
            Command::new("docker").args(["exec", &name, "psql", "-U", &pg.user, "-d", &pg.database, "-c", sleep_sql]).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
        })
        .ok();
    pause(cx, 1000).await;
    let pid = sql1(&core, &pg, &pgpw, "SELECT pid FROM pg_stat_activity WHERE query LIKE '%e2e sleeper%' AND query NOT LIKE '%pg_stat_activity%'", cx).await;
    let rx = start(&client, "cancel_backend", json!({"connection": pg.name, "pid": pid.as_deref().and_then(|p| p.parse::<i64>().ok()).unwrap_or(0)}));
    let asked = sheet_answer(ws, true, cx).await;
    let res = rx.await.unwrap_or((true, String::new()));
    let mut ended = None;
    for _ in 0..40 {
        if let Some(Ok(Some(st))) = sleeper.as_mut().map(|c| c.try_wait()) {
            ended = Some(st);
            break;
        }
        pause(cx, 100).await;
    }
    if let Some(c) = sleeper.as_mut() {
        let _ = c.kill();
    }
    r.check("cancel_backend stops another program's query", !res.0 && ended.is_some_and(|s| !s.success()), format!("pid {pid:?} asked {} {} psql {ended:?}", asked.is_some(), short(&res)));
    let res = call(&client, "terminate_backend", json!({"connection": pg.name, "pid": 1}), cx).await;
    r.check("terminate_backend refuses a pid that is not a client backend", res.0, short(&res));

    let res = call(&client, "create_connection", json!({"name": "e2e by agent", "host": "127.0.0.1", "port": 5999, "user": "x", "password": "pw"}), cx).await;
    let made = core.config.connections().into_iter().any(|p| p.name == "e2e by agent");
    r.check("create_connection saves a connection the app lists", !res.0 && made, short(&res));
    let res = call(&client, "update_connection", json!({"connection": "e2e by agent", "name": "e2e renamed by agent"}), cx).await;
    r.check("update_connection renames it", !res.0 && core.config.connections().into_iter().any(|p| p.name == "e2e renamed by agent"), short(&res));

    // An agent login an agent asks for: made from the rules, only after Allow, checked, its
    // password never in the reply nor on the sheet.
    let rx = start(&client, "create_agent_login", json!({"connection": pg.name}));
    let asked = sheet_answer(ws, true, cx).await.unwrap_or_default();
    let res = rx.await.unwrap_or((true, String::new()));
    pause(cx, 200).await;
    let made = core.config.get(&pg.id).and_then(|p| p.agent_login);
    let pw = core.secrets.get(&dataman_core::agent_login::secret_account(&pg.id)).ok().flatten().unwrap_or_default();
    r.check("create_agent_login waits for Allow (the sheet shows what runs), then creates the login and checks it", asked.contains("CREATE ROLE") && !res.0 && made.as_ref().is_some_and(|l| l.verified.is_some()), format!("{} {}", asked.len(), short(&res)));
    r.check("…its password in the Keychain, never in the reply nor on the sheet", !pw.is_empty() && !res.1.contains(&pw) && !asked.contains(&pw), format!("{}", pw.len()));
    let res = call(&client, "create_agent_login", json!({"connection": pg.name}), cx).await;
    r.check("…and asking again is refused: it already has one", res.0 && res.1.contains("already has"), short(&res));
    if let Some(l) = made {
        let (core2, prof, pw2) = (core.clone(), ConnectionProfile { agent_login: None, ..pg.clone() }, pgpw.clone());
        let gone = cx
            .background_executor()
            .spawn(async move {
                let c = core2.block_on(core2.connect(prof.clone(), Some(pw2))).map_err(|e| format!("{e:#}"))?;
                let plan = core2.block_on(dataman_core::agent_login::live::removal(&c, &prof, &l)).map_err(|e| format!("{e:#}"))?;
                core2.block_on(dataman_core::agent_login::live::apply(&c, &plan, &prof.database)).map_err(|e| format!("{e:#}"))
            })
            .await;
        let _ = core.secrets.delete(&dataman_core::agent_login::secret_account(&pg.id));
        if let Some(mut p) = core.config.get(&pg.id) {
            p.agent_login = None;
            let _ = core.config.upsert(p);
        }
        r.check("…removed again after", gone.is_ok(), format!("{gone:?}"));
    }

    show_destinations(ws, &client, &pg, &profiles[1].1, r, cx).await;

    let _ = sql(&core, &pg, &pgpw, "DROP SCHEMA IF EXISTS e2e_mcp CASCADE", cx).await;
    drop(client);
}
