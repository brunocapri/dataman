//! `outside-check`: connections.toml edited by another program (an agent with a shell, say)
//! while Dataman runs. What the edit loosens waits for review and the approved rules apply
//! meanwhile; Not now keeps it held, the palette shows it again, ⌘↩ applies it and logs it; a
//! connection added that way stays out of the explorer until then. The approved rules live in
//! this run's memory store, as they would in the Keychain. A quickstart's connections, saved
//! through MCP with this app open or by the tool alone, wait for nothing.

use dataman_core::config::{ConfigStore, ConnectionProfile, Engine, McpAccess};
use gpui::{AsyncWindowContext, Entity};

use crate::app::Workspace;
use crate::check::{Report, click_probe, core, key, pause, probed, typ, until};
use crate::mcp_tools_check::{Client, call, forward};
use serde_json::{Value, json};

pub(crate) fn snap(name: &str, cx: &mut AsyncWindowContext) {
    let Ok(dir) = std::env::var("DATAMAN_SHOT_DIR") else { return };
    let path = format!("{dir}/outside-{name}.png");
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
    });
}

fn names(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Vec<String> {
    cx.update(|_, cx| ws.read(cx).store.read(cx).profiles.iter().map(|p| p.name.clone()).collect()).unwrap_or_default()
}

fn shown(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|_, cx| ws.read(cx).outside_review.is_some()).unwrap_or(false)
}

/// create_connection as the tool process runs it with no app: its own hub, with no front, and the
/// keyring the app reads.
async fn saved_alone(core: &std::sync::Arc<dataman_core::Core>, args: Value, cx: &mut AsyncWindowContext) -> Value {
    let (dirs, secrets, approved) = (core.dirs.clone(), core.secrets.clone(), core.approved.clone());
    cx.background_executor()
        .spawn(async move {
            let hub = dataman_mcp::Hub::with_approved(dirs, secrets, approved).map_err(|e| e.to_string()).unwrap();
            let reg = dataman_mcp::registry(&hub.dirs);
            let server = dataman_mcp::Server::new(hub.clone(), reg.clone(), "stdio");
            let tool = reg.find("create_connection").unwrap();
            let r = hub.handle().block_on(server.run_tool(tool, args.as_object().unwrap().clone(), "agent".into(), Default::default()));
            // Its runtime must not end from here.
            std::mem::forget(hub);
            serde_json::to_value(r).unwrap()
        })
        .await
}

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let core = core(ws, cx);
    r.check("the approved rules are kept", core.approved.is_on(), "");

    let _ = cx.update(|_, cx| crate::mcp::apply(&core, cx));
    let up = until(cx, 10, |cx| cx.update(|_, cx| crate::mcp::listening(cx)).unwrap_or(false)).await;
    let client = match Client::ready(&core.dirs.config).await {
        Ok((c, _)) if up => c,
        other => {
            r.check("dataman-mcp reaches this app", false, format!("socket {up}; {:?}", other.err()));
            return;
        }
    };
    let quick = |i: usize| {
        let env = ["dev", "staging", "production"][i % 3];
        json!({"name": format!("quick {i}"), "host": format!("db{i}.example.internal"), "user": "app", "database": "shop", "env": env})
    };
    let mut made = true;
    for i in 0..3 {
        let res = call(&client, "create_connection", quick(i), cx).await;
        made &= !res.0;
    }
    let listed = until(cx, 5, |cx| (0..3).all(|i| names(ws, cx).contains(&format!("quick {i}")))).await;
    // Long enough for the file watcher to have seen every write.
    pause(cx, 1500).await;
    core.config.refresh();
    r.check(
        "a quickstart through MCP, the app open: every connection there, no sheet about edits outside",
        made && listed && !shown(ws, cx) && core.config.held().is_empty(),
        format!("{made} {listed} {:?}", core.config.held()),
    );
    let mut made = true;
    for i in 3..6 {
        made &= saved_alone(&core, quick(i), cx).await["isError"] != json!(true);
    }
    let listed = until(cx, 5, |cx| (3..6).all(|i| names(ws, cx).contains(&format!("quick {i}")))).await;
    pause(cx, 1500).await;
    core.config.refresh();
    r.check(
        "…and written by the tool alone, as with the app closed: none either",
        made && listed && !shown(ws, cx) && core.config.held().is_empty(),
        format!("{made} {listed} {:?}", core.config.held()),
    );

    let dir = std::env::temp_dir().join(format!("dataman-outside-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("shop.sqlite");
    let _ = dataman_core::import::create_database(Engine::Sqlite, &db);
    let shop = ConnectionProfile { id: "shop".into(), mcp: Some(McpAccess::Read), ..ConnectionProfile::new_file(Engine::Sqlite, db.display().to_string()) };
    let shop = ConnectionProfile { name: "Shop".into(), ..shop };
    core.config.upsert(shop.clone()).unwrap();
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.connections_on_disk(window, cx)));
    until(cx, 5, |cx| names(ws, cx).contains(&"Shop".to_string())).await;

    let outside = || ConfigStore::open(&core.dirs).unwrap();
    outside().upsert(ConnectionProfile { mcp: Some(McpAccess::Full), mcp_yolo: true, ..shop.clone() }).unwrap();
    let asked = until(cx, 5, |cx| probed("outside-review-apply", cx).is_some()).await;
    r.check("an edit outside Dataman that loosens asks first", asked && probed("outside-review-loosens", cx).is_some(), "");
    snap("review", cx);
    let p = core.config.get("shop");
    r.check(
        "…and the approved rules apply until then",
        p.as_ref().is_some_and(|p| p.mcp_access() == McpAccess::Read && !p.mcp_yolo),
        format!("{:?}", p.map(|p| (p.mcp, p.mcp_yolo))),
    );

    click_probe("outside-review-keep", cx);
    let kept = until(cx, 2, |cx| !shown(ws, cx)).await;
    pause(cx, 1500).await;
    r.check(
        "Not now: held, and not asked again for the same edit",
        kept && !shown(ws, cx) && core.config.get("shop").is_some_and(|p| p.mcp_access() == McpAccess::Read),
        "",
    );

    key(cx, "cmd-k");
    pause(cx, 100).await;
    typ(cx, ">review edits made outside");
    pause(cx, 300).await;
    key(cx, "enter");
    let again = until(cx, 2, |cx| probed("outside-review-apply", cx).is_some()).await;
    key(cx, "escape");
    let esc = until(cx, 2, |cx| !shown(ws, cx)).await;
    r.check("the palette shows it again; esc keeps it held", again && esc, "");

    let fronted = forward(cx);
    let res = call(&client, "show_in_dataman", json!({"destination": "outside_review"}), cx).await;
    let there = until(cx, 2, |cx| probed("outside-review-apply", cx).is_some()).await;
    let p = core.config.get("shop");
    r.check(
        "show_in_dataman outside_review opens the sheet, window forward, and applies nothing",
        !res.0 && there && forward(cx) == fronted + 1 && p.as_ref().is_some_and(|p| p.mcp_access() == McpAccess::Read && !p.mcp_yolo),
        format!("{res:?} {there} {:?}", p.map(|p| p.mcp)),
    );
    key(cx, "escape");
    until(cx, 2, |cx| !shown(ws, cx)).await;

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.show_outside_review(window, cx)));
    until(cx, 2, |cx| probed("outside-review-apply", cx).is_some()).await;
    key(cx, "cmd-enter");
    let applied = until(cx, 5, |_| core.config.get("shop").is_some_and(|p| p.mcp_access() == McpAccess::Full && p.mcp_yolo)).await;
    let audit = std::fs::read_to_string(core.dirs.mcp_audit_file()).unwrap_or_default();
    r.check("⌘↩ applies it, logged in Audit", applied && audit.contains("outside_review"), "");

    // A connection added outside Dataman, with more than a new one gets.
    let other = dir.join("other.sqlite");
    let _ = dataman_core::import::create_database(Engine::Sqlite, &other);
    let sneaky = ConnectionProfile {
        id: "sneaky".into(),
        name: "Sneaky".into(),
        mcp: Some(McpAccess::Full),
        ..ConnectionProfile::new_file(Engine::Sqlite, other.display().to_string())
    };
    outside().upsert(sneaky).unwrap();
    let asked = until(cx, 5, |cx| probed("outside-review-apply", cx).is_some()).await;
    snap("new", cx);
    r.check(
        "a connection added outside Dataman asks first, and stays out of the explorer",
        asked && !names(ws, cx).contains(&"Sneaky".to_string()) && core.config.get("sneaky").is_none(),
        format!("{:?}", names(ws, cx)),
    );
    key(cx, "cmd-enter");
    let joined = until(cx, 5, |cx| names(ws, cx).contains(&"Sneaky".to_string())).await;
    r.check("…until applied", joined, "");

    // Tighter is taken as it is.
    outside().upsert(ConnectionProfile { mcp: Some(McpAccess::Off), mcp_yolo: false, ..core.config.get("shop").unwrap() }).unwrap();
    let tight = until(cx, 5, |_| core.config.get("shop").is_some_and(|p| p.mcp_access() == McpAccess::Off)).await;
    r.check("an edit that only tightens applies at once", tight && !shown(ws, cx), "");
    drop(client);
    let _ = std::fs::remove_dir_all(&dir);
}
