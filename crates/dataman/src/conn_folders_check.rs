//! `explorer-folders-check`: the explorer's optional folders (made from the header, named in
//! place, a connection moved in by its menu and by a drag, out to the top level, colored,
//! deleted with its connections kept, undone), the keys over folder rows, the search field's
//! folder chip in the explorer and in Favorites, and connections.toml written as before once no
//! folder is left. The `*-shot` stages set the same scenes up for screenshots.

use gpui::{AsyncWindowContext, Entity, Focusable, Modifiers, MouseButton, point, px};
use dataman_core::config::Engine;

use crate::app::Workspace;
use crate::check::*;
use crate::tree::{ConnDrop, SchemaTree, fid};
use crate::tree_menu::Act;

pub fn is_stage(s: &str) -> bool {
    matches!(s, "explorer-folders-check" | "explorer-folders-shot" | "explorer-scope-shot" | "fav-scope-shot")
}

fn tree(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Entity<SchemaTree> {
    cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap()
}

fn rows(t: &Entity<SchemaTree>, cx: &mut AsyncWindowContext) -> Vec<String> {
    draw(cx);
    cx.update(|_, cx| t.read(cx).demo_rows()).unwrap_or_default().into_iter().map(|(d, l)| format!("{}{l}", "  ".repeat(d))).collect()
}

/// The top rows only (connections and folders, not what an open connection lists).
fn heads(t: &Entity<SchemaTree>, names: &[&str], cx: &mut AsyncWindowContext) -> Vec<String> {
    rows(t, cx).into_iter().filter(|r| names.iter().any(|n| r.trim_start() == *n)).collect()
}

fn folders(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Vec<dataman_core::ConnFolder> {
    core(ws, cx).config.folders()
}

fn folder_named(ws: &Entity<Workspace>, name: &str, cx: &mut AsyncWindowContext) -> Option<dataman_core::ConnFolder> {
    folders(ws, cx).into_iter().find(|d| d.name == name)
}

fn focus_tree(t: &Entity<SchemaTree>, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| window.focus(&t.focus_handle(cx), cx));
    draw(cx);
}

/// A right click on the row, then `act` (a submenu's item runs as its click would).
async fn menu(t: &Entity<SchemaTree>, row: &str, act: Act, cx: &mut AsyncWindowContext) -> bool {
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.select_id(row, cx)));
    draw(cx);
    let Some(p) = cx.update(|_, cx| t.read(cx).demo_row_point(row)).ok().flatten() else {
        eprintln!("check: row {row} not on screen");
        return false;
    };
    click_with(cx, p, MouseButton::Right, Modifiers::default(), 1);
    pause(cx, 200).await;
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
    pause(cx, 200).await;
    draw(cx);
    true
}

fn row_point(t: &Entity<SchemaTree>, id: &str, cx: &mut AsyncWindowContext) -> Option<gpui::Point<gpui::Pixels>> {
    draw(cx);
    cx.update(|_, cx| t.read(cx).demo_row_point(id)).ok().flatten()
}

fn shoot(cx: &mut AsyncWindowContext) {
    if let Ok(path) = std::env::var("DATAMAN_SHOT") {
        let _ = cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
            crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
        });
    }
}

/// Saved connections that are not connected, by name.
fn add_saved(ws: &Entity<Workspace>, names: &[(&str, Option<&str>)], cx: &mut AsyncWindowContext) -> Vec<String> {
    cx.update(|_, cx| {
        let core = ws.read(cx).store.read(cx).core.clone();
        let ids = names
            .iter()
            .map(|(n, color)| {
                let mut p = dataman_core::ConnectionProfile::new(*n);
                p.host = format!("{}.db.internal", n.to_lowercase().replace(' ', "-"));
                p.color = color.map(str::to_string);
                let id = p.id.clone();
                let _ = core.config.upsert(p);
                id
            })
            .collect();
        ws.read(cx).store.clone().update(cx, |s, cx| s.reload(cx));
        ids
    })
    .unwrap_or_default()
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (pg, pw) = profile(Engine::Postgres);
    let pg_id = pg.id.clone();
    if let Err(e) = connect(ws, &pg, &pw, cx).await {
        r.check("the PostgreSQL connection", false, e);
        return;
    }
    match stage {
        "explorer-folders-shot" | "explorer-scope-shot" => return scene(ws, &pg_id, stage, cx).await,
        "fav-scope-shot" => return fav_scope(ws, r, true, cx).await,
        _ => {}
    }
    let t = tree(ws, cx);
    let pg_name = cx.update(|_, cx| ws.read(cx).store.read(cx).profile(&pg_id).map(|p| p.name.clone())).ok().flatten().unwrap_or_default();
    let ids = add_saved(ws, &[("Billing", None), ("Search", None)], cx);
    let (billing, search) = (ids[0].clone(), ids[1].clone());
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.collapse_id(&format!("c:{pg_id}"), cx)));
    let names = [pg_name.as_str(), "Billing", "Search", "Prod", "Dev"];
    let file = |cx: &mut AsyncWindowContext| std::fs::read_to_string(core(ws, cx).dirs.connections_file()).unwrap_or_default();
    let plain = file(cx);
    r.check(
        "no folder: the connections at the top level, no chip in the search field",
        heads(&t, &names, cx) == [pg_name.clone(), "Billing".into(), "Search".into()] && probed("x-scope", cx).is_none() && !plain.contains("[[folder]]"),
        format!("{:?}", heads(&t, &names, cx)),
    );

    // Made from the header's button, named in place.
    let clicked = click_probe("x-new-folder", cx);
    pause(cx, 50).await;
    let naming = cx.update(|_, cx| t.read(cx).demo_naming(cx)).ok().flatten();
    r.check("the header's folder button makes a folder and names it in place", clicked && naming.as_ref().is_some_and(|n| n.1 == "New folder"), format!("{naming:?}"));
    typ(cx, "Prod");
    key(cx, "enter");
    pause(cx, 50).await;
    let prod = folder_named(ws, "Prod", cx);
    r.check(
        "typed over and ↩: the folder is named, first in the explorer, saved in connections.toml",
        prod.is_some() && heads(&t, &names, cx).first().map(String::as_str) == Some("Prod") && file(cx).contains("name = \"Prod\""),
        format!("{:?} {:?}", folders(ws, cx), heads(&t, &names, cx)),
    );
    let Some(prod) = prod else { return };
    r.check("a folder: the search field shows its folder chip (All)", probed("x-scope", cx).is_some(), "");

    // Into the folder through the connection's menu.
    let moved = menu(&t, &format!("c:{billing}"), Act::MoveToFolder(0), cx).await;
    let h = heads(&t, &names, cx);
    r.check(
        "Move to folder › Prod: the connection is listed in it, one level in",
        moved && folder_named(ws, "Prod", cx).is_some_and(|d| d.connections == [billing.clone()]) && h == ["Prod", "  Billing", pg_name.as_str(), "Search"],
        format!("{h:?}"),
    );

    // Into the folder by a drag onto its row.
    let (from, to) = (row_point(&t, &format!("c:{search}"), cx), row_point(&t, &fid(&prod.id), cx));
    let mut during = None;
    if let (Some(from), Some(to)) = (from, to) {
        crate::history_check::drag(cx, from, to, |cx| during = cx.update(|_, cx| t.read(cx).demo_conn_drop()).ok().flatten()).await;
    }
    let h = heads(&t, &names, cx);
    r.check(
        "a connection dragged onto the folder's row targets it, and dropped it is in it",
        during == Some(ConnDrop::Into(prod.id.clone())) && folder_named(ws, "Prod", cx).is_some_and(|d| d.holds(&search)) && h == ["Prod", "  Billing", "  Search", pg_name.as_str()],
        format!("{during:?} {h:?}"),
    );

    // Out to the top level: dragged under the last row.
    let last = cx.update(|_, cx| t.read(cx).demo_rows().len()).unwrap_or(0);
    let bottom = row_point(&t, &format!("c:{pg_id}"), cx).map(|p| point(p.x, p.y + px(22. * (last as f32) + 60.)));
    let (from, mut during) = (row_point(&t, &format!("c:{billing}"), cx), None);
    if let (Some(from), Some(to)) = (from, bottom) {
        crate::history_check::drag(cx, from, to, |cx| during = cx.update(|_, cx| t.read(cx).demo_conn_drop()).ok().flatten()).await;
    }
    let h = heads(&t, &names, cx);
    r.check(
        "dragged under the rows, it leaves its folder for the end of the top level",
        during == Some(ConnDrop::At { folder: None, before: None }) && h == ["Prod", "  Search", pg_name.as_str(), "Billing"] && folder_named(ws, "Prod", cx).is_some_and(|d| !d.holds(&billing)),
        format!("{during:?} {h:?}"),
    );
    focus_tree(&t, cx);
    key(cx, "cmd-z");
    let back = heads(&t, &names, cx);
    key(cx, "cmd-shift-z");
    let again = heads(&t, &names, cx);
    r.check(
        "⌘Z puts it back in the folder (and in its place), ⇧⌘Z moves it out again",
        back == ["Prod", "  Billing", "  Search", pg_name.as_str()] && again == h,
        format!("{back:?} / {again:?}"),
    );

    // Color.
    let colored = menu(&t, &fid(&prod.id), Act::FolderColor(Some(4)), cx).await;
    r.check("Color › Sky on the folder's menu: saved by the swatch's name", colored && folder_named(ws, "Prod", cx).and_then(|d| d.color).as_deref() == Some("sky"), format!("{:?}", folders(ws, cx)));

    // The keys over folder rows.
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.select_id(&fid(&prod.id), cx)));
    focus_tree(&t, cx);
    key(cx, "left");
    let shut = heads(&t, &names, cx);
    key(cx, "right");
    let open = heads(&t, &names, cx);
    key(cx, "down");
    let sel = cx.update(|_, cx| t.read(cx).selected_id().map(str::to_string)).ok().flatten();
    key(cx, "left");
    let up = cx.update(|_, cx| t.read(cx).selected_id().map(str::to_string)).ok().flatten();
    r.check(
        "← shuts the folder, → opens it, ↓ goes to its connection, ← back up to the folder",
        shut == ["Prod", pg_name.as_str(), "Billing"] && open == ["Prod", "  Search", pg_name.as_str(), "Billing"] && sel == Some(format!("c:{search}")) && up == Some(fid(&prod.id)),
        format!("{shut:?} {open:?} {sel:?} {up:?}"),
    );

    // Rename from the menu.
    let renaming = menu(&t, &fid(&prod.id), Act::RenameFolder, cx).await;
    key(cx, "cmd-a");
    typ(cx, "Live");
    key(cx, "enter");
    pause(cx, 50).await;
    r.check("Rename: named in place, saved", renaming && folder_named(ws, "Live", cx).is_some(), format!("{:?}", folders(ws, cx)));
    let names = [pg_name.as_str(), "Billing", "Search", "Live", "Dev"];

    // A second folder, by the connection's "New folder…": the connection is in it.
    let made = menu(&t, &format!("c:{pg_id}"), Act::MoveToNewFolder, cx).await;
    pause(cx, 50).await;
    typ(cx, "Dev");
    key(cx, "enter");
    pause(cx, 50).await;
    let dev = folder_named(ws, "Dev", cx);
    r.check(
        "Move to folder › New folder…: a folder named in place, the connection in it",
        made && dev.as_ref().is_some_and(|d| d.holds(&pg_id)) && heads(&t, &names, cx) == ["Live", "  Search", "Dev", format!("  {pg_name}").as_str(), "Billing"],
        format!("{:?}", heads(&t, &names, cx)),
    );
    let Some(dev) = dev else { return };

    // The search chip: a folder picked there is the only one shown and searched.
    let opened = click_probe("x-scope", cx);
    pause(cx, 150).await;
    let menu_open = cx.update(|_, cx| t.read(cx).demo_scope().1).unwrap_or(false);
    r.check("a click on the chip opens its folder list", opened && menu_open, "");
    let picked = click_probe(&format!("xscope:{}", dev.id), cx);
    pause(cx, 100).await;
    let (scope, still) = cx.update(|_, cx| t.read(cx).demo_scope()).unwrap_or_default();
    r.check(
        "a folder picked: the explorer shows that folder alone",
        picked && scope.as_deref() == Some(dev.id.as_str()) && !still && heads(&t, &names, cx) == ["Dev", format!("  {pg_name}").as_str()],
        format!("{scope:?} {:?}", heads(&t, &names, cx)),
    );
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.expand_id(&format!("c:{pg_id}"), cx)));
    let public = format!("c:{pg_id}/d:{}/s:public", pg.database);
    until(cx, 10, |cx| cx.update(|_, cx| t.read(cx).has_row(&public)).unwrap_or(false)).await;
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.expand_id(&public, cx)));
    pause(cx, 500).await;
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.set_filter("orders", cx)));
    let found = until(cx, 10, |cx| rows(&t, cx).iter().any(|r| r.trim_start() == "orders")).await;
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.set_scope(Some(prod.id.clone()), cx)));
    let elsewhere = rows(&t, cx);
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.set_scope(None, cx)));
    let all = rows(&t, cx);
    r.check(
        "the search runs in the picked folder only: found in Dev, nothing in Live, found again with All",
        found && !elsewhere.iter().any(|r| r.trim_start() == "orders") && all.iter().any(|r| r.trim_start() == "orders"),
        format!("{elsewhere:?} / {all:?}"),
    );
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.set_filter("", cx)));

    // Deleting a folder keeps its connections; a scope on it goes back to All.
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.set_scope(Some(dev.id.clone()), cx)));
    let deleted = menu(&t, &fid(&dev.id), Act::DeleteFolder, cx).await;
    let (scope, _) = cx.update(|_, cx| t.read(cx).demo_scope()).unwrap_or_default();
    let h = heads(&t, &names, cx);
    let kept = cx.update(|_, cx| ws.read(cx).store.read(cx).profiles.len()).unwrap_or(0);
    r.check(
        "Delete folder: gone, its connection back at the top level (nothing deleted), the chip back on All",
        deleted && folder_named(ws, "Dev", cx).is_none() && scope.is_none() && kept == 3 && h == ["Live", "  Search", pg_name.as_str(), "Billing"],
        format!("{scope:?} {h:?} {kept}"),
    );
    focus_tree(&t, cx);
    key(cx, "cmd-z");
    let back = folder_named(ws, "Dev", cx);
    r.check("⌘Z brings the folder back with its connection", back.is_some_and(|d| d.holds(&pg_id)), format!("{:?}", folders(ws, cx)));

    for d in folders(ws, cx) {
        let _ = menu(&t, &fid(&d.id), Act::DeleteFolder, cx).await;
    }
    let after = file(cx);
    let h = heads(&t, &names, cx);
    r.check(
        "every folder deleted: the explorer as before (no chip), connections.toml without folders",
        folders(ws, cx).is_empty() && probed("x-scope", cx).is_none() && !after.contains("[[folder]]") && h.iter().all(|r| !r.starts_with(' ')),
        format!("{h:?}"),
    );

    fav_scope(ws, r, false, cx).await;
}

/// Favorites in two folders and one at the top level: the search field's chip lists the
/// folders, and a folder picked is the only one listed and searched.
async fn fav_scope(ws: &Entity<Workspace>, r: &mut Report, shot: bool, cx: &mut AsyncWindowContext) {
    let history = cx.update(|_, cx| ws.read(cx).history.clone()).unwrap();
    let conn = cx.update(|_, cx| ws.read(cx).store.read(cx).profiles.first().map(|p| p.id.clone())).ok().flatten();
    let _ = cx.update(|_, cx| {
        let core = ws.read(cx).store.read(cx).core.clone();
        let reports = core.favorites.add_folder("Monthly reports").ok();
        let ops = core.favorites.add_folder("Ops").ok();
        for (name, sql, folder) in [
            ("Orders by day", "SELECT date_trunc('day', created) AS day, count(*) FROM orders GROUP BY 1 ORDER BY 1", &reports),
            ("Revenue by month", "SELECT date_trunc('month', created), sum(total) FROM orders GROUP BY 1 ORDER BY 1", &reports),
            ("Locks waiting", "SELECT * FROM pg_locks WHERE NOT granted", &ops),
            ("Top customers", "SELECT customer, count(*) FROM orders GROUP BY 1 ORDER BY 2 DESC LIMIT 10", &None),
        ] {
            let mut f = dataman_core::Favorite::new(name, sql);
            f.connection_id = conn.clone();
            f.folder = folder.as_ref().map(|d| d.id.clone());
            let _ = core.favorites.add(f);
        }
        ws.update(cx, |w, cx| w.demo_favorites_view(cx));
        history.update(cx, |h, cx| h.reload_favorites(cx));
    });
    pause(cx, 300).await;
    draw(cx);
    let listed = crate::history_check::rows(&history, cx);
    r.check("Favorites with folders: its search field shows the folder chip", probed("fav-scope", cx).is_some(), format!("{listed:?}"));
    let opened = click_probe("fav-scope", cx);
    pause(cx, 150).await;
    let ops = cx.update(|_, cx| ws.read(cx).store.read(cx).core.favorites.folders().into_iter().find(|d| d.name == "Ops")).ok().flatten();
    let reports = cx.update(|_, cx| ws.read(cx).store.read(cx).core.favorites.folders().into_iter().find(|d| d.name == "Monthly reports")).ok().flatten();
    let (Some(ops), Some(reports)) = (ops, reports) else {
        r.check("the favorites' folders", false, "");
        return;
    };
    if shot {
        let _ = click_probe(&format!("fscope:{}", reports.id), cx);
        pause(cx, 200).await;
        let _ = cx.update(|window, cx| history.update(cx, |h, cx| h.demo_search("by", window, cx)));
        pause(cx, 400).await;
        shoot(cx);
        return;
    }
    let menu_open = cx.update(|_, cx| history.read(cx).demo_fav_scope().1).unwrap_or(false);
    r.check("a click on it opens the folder list", opened && menu_open, "");
    let picked = click_probe(&format!("fscope:{}", ops.id), cx);
    pause(cx, 100).await;
    let shown = crate::history_check::rows(&history, cx);
    let (scope, still, _) = cx.update(|_, cx| history.read(cx).demo_fav_scope()).unwrap_or_default();
    r.check(
        "a folder picked: Favorites lists that folder alone",
        picked && scope.as_deref() == Some("Ops") && !still && shown == ["▾ Ops", "  Locks waiting"],
        format!("{scope:?} {shown:?}"),
    );
    let _ = cx.update(|window, cx| history.update(cx, |h, cx| h.demo_search("orders", window, cx)));
    let none = crate::history_check::rows(&history, cx);
    let _ = cx.update(|window, cx| history.update(cx, |h, cx| h.demo_search("locks", window, cx)));
    let some = crate::history_check::rows(&history, cx);
    r.check("the search runs in that folder only", none.is_empty() && some == ["▾ Ops", "  Locks waiting"], format!("{none:?} / {some:?}"));
    let _ = cx.update(|window, cx| history.update(cx, |h, cx| h.demo_search("", window, cx)));
    let _ = cx.update(|_, cx| {
        let core = ws.read(cx).store.read(cx).core.clone();
        let _ = core.favorites.remove_folder(&ops.id);
        history.update(cx, |h, cx| h.reload_favorites(cx));
    });
    let (scope, _, _) = cx.update(|_, cx| history.read(cx).demo_fav_scope()).unwrap_or_default();
    let back = crate::history_check::rows(&history, cx);
    r.check(
        "its folder deleted, the chip goes back to All and everything is listed",
        scope.is_none() && back.contains(&"Top customers".to_string()) && back.iter().any(|r| r.ends_with("Monthly reports")),
        format!("{scope:?} {back:?}"),
    );
}

/// Two colored folders (one open, one shut) and loose connections; with the chip's list open.
async fn scene(ws: &Entity<Workspace>, pg_id: &str, stage: &str, cx: &mut AsyncWindowContext) {
    let t = tree(ws, cx);
    let ids = add_saved(ws, &[("billing-eu", Some("coral")), ("billing-us", Some("sand")), ("search-prod", Some("mint")), ("analytics", None), ("staging", Some("sky")), ("local-dev", Some("slate"))], cx);
    let _ = cx.update(|_, cx| {
        let store = ws.read(cx).store.clone();
        let mut prod = dataman_core::ConnFolder::new("Production", &[]);
        prod.color = Some("rose".into());
        prod.connections = vec![pg_id.to_string(), ids[0].clone(), ids[1].clone(), ids[2].clone()];
        let mut stage = dataman_core::ConnFolder::new("Staging", std::slice::from_ref(&prod));
        stage.color = Some("cobalt".into());
        stage.connections = vec![ids[4].clone()];
        let _ = store.update(cx, |s, cx| s.edit_folders(|f| *f = vec![prod, stage.clone()], cx));
        t.update(cx, |t, cx| {
            t.folders_changed(cx);
            t.collapse_id(&fid(&stage.id), cx);
            t.collapse_id(&format!("c:{pg_id}"), cx);
        });
    });
    pause(cx, 1200).await;
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.select_id(&format!("c:{}", ids[0]), cx)));
    if stage == "explorer-scope-shot" {
        click_probe("x-scope", cx);
        pause(cx, 300).await;
    }
    pause(cx, 400).await;
    shoot(cx);
}
