//! `outside-check`: connections.toml edited by another program (an agent with a shell, say)
//! while Dataman runs. What the edit loosens waits for review and the approved rules apply
//! meanwhile; Not now keeps it held, the palette shows it again, ⌘↩ applies it and logs it; a
//! connection added that way stays out of the explorer until then. The approved rules live in
//! this run's memory store, as they would in the Keychain.

use gpui::{AsyncWindowContext, Entity};
use dataman_core::config::{ConfigStore, ConnectionProfile, Engine, McpAccess};

use crate::app::Workspace;
use crate::check::{Report, click_probe, core, key, pause, probed, typ, until};

fn snap(name: &str, cx: &mut AsyncWindowContext) {
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

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let core = core(ws, cx);
    r.check("the approved rules are kept", core.approved.is_on(), "");
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

    // Another program of this user writes the file.
    let outside = || ConfigStore::open(&core.dirs).unwrap();
    outside().upsert(ConnectionProfile { mcp: Some(McpAccess::Full), mcp_yolo: true, ..shop.clone() }).unwrap();
    let asked = until(cx, 5, |cx| probed("outside-review-apply", cx).is_some()).await;
    r.check("an edit outside Dataman that loosens asks first", asked && probed("outside-review-loosens", cx).is_some(), "");
    snap("review", cx);
    let p = core.config.get("shop");
    r.check("…and the approved rules apply until then", p.as_ref().is_some_and(|p| p.mcp_access() == McpAccess::Read && !p.mcp_yolo), format!("{:?}", p.map(|p| (p.mcp, p.mcp_yolo))));

    click_probe("outside-review-keep", cx);
    let kept = until(cx, 2, |cx| !shown(ws, cx)).await;
    pause(cx, 1500).await;
    r.check("Not now: held, and not asked again for the same edit", kept && !shown(ws, cx) && core.config.get("shop").is_some_and(|p| p.mcp_access() == McpAccess::Read), "");

    key(cx, "cmd-k");
    pause(cx, 100).await;
    typ(cx, ">review edits made outside");
    pause(cx, 300).await;
    key(cx, "enter");
    let again = until(cx, 2, |cx| probed("outside-review-apply", cx).is_some()).await;
    key(cx, "escape");
    let esc = until(cx, 2, |cx| !shown(ws, cx)).await;
    r.check("the palette shows it again; esc keeps it held", again && esc, "");

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.show_outside_review(window, cx)));
    until(cx, 2, |cx| probed("outside-review-apply", cx).is_some()).await;
    key(cx, "cmd-enter");
    let applied = until(cx, 5, |_| core.config.get("shop").is_some_and(|p| p.mcp_access() == McpAccess::Full && p.mcp_yolo)).await;
    let audit = std::fs::read_to_string(core.dirs.mcp_audit_file()).unwrap_or_default();
    r.check("⌘↩ applies it, logged in Audit", applied && audit.contains("outside_review"), "");

    // A connection added outside Dataman, with more than a new one gets.
    let other = dir.join("other.sqlite");
    let _ = dataman_core::import::create_database(Engine::Sqlite, &other);
    let sneaky = ConnectionProfile { id: "sneaky".into(), name: "Sneaky".into(), mcp: Some(McpAccess::Full), ..ConnectionProfile::new_file(Engine::Sqlite, other.display().to_string()) };
    outside().upsert(sneaky).unwrap();
    let asked = until(cx, 5, |cx| probed("outside-review-apply", cx).is_some()).await;
    snap("new", cx);
    r.check("a connection added outside Dataman asks first, and stays out of the explorer", asked && !names(ws, cx).contains(&"Sneaky".to_string()) && core.config.get("sneaky").is_none(), format!("{:?}", names(ws, cx)));
    key(cx, "cmd-enter");
    let joined = until(cx, 5, |cx| names(ws, cx).contains(&"Sneaky".to_string())).await;
    r.check("…until applied", joined, "");

    // Tighter is taken as it is.
    outside().upsert(ConnectionProfile { mcp: Some(McpAccess::Off), mcp_yolo: false, ..core.config.get("shop").unwrap() }).unwrap();
    let tight = until(cx, 5, |_| core.config.get("shop").is_some_and(|p| p.mcp_access() == McpAccess::Off)).await;
    r.check("an edit that only tightens applies at once", tight && !shown(ws, cx), "");
    let _ = std::fs::remove_dir_all(&dir);
}
