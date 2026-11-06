//! `explorer-folders-check`: the explorer's optional folders (made from the header, named in
//! place, a connection moved in by its menu and by a drag, out to the top level, colored,
//! deleted with its connections kept, undone), the keys over folder rows, a search over every
//! folder (no folder chip), Favorites narrowed to a connection, and connections.toml written as
//! before once no folder is left. The `*-shot` stages set the same scenes up for screenshots.

use gpui::{AsyncWindowContext, Entity, Focusable, Modifiers, MouseButton, point, px};
use dataman_core::config::Engine;

use crate::app::Workspace;
use crate::check::*;
use crate::tree::{ConnDrop, MenuAnchor, SchemaTree, fid, pin_id};
use crate::tree_menu::Act;

pub fn is_stage(s: &str) -> bool {
    matches!(s, "explorer-folders-check" | "explorer-folders-shot" | "fav-scope-shot")
}

fn core_of(ws: &Entity<Workspace>, cx: &gpui::App) -> std::sync::Arc<dataman_core::Core> {
    ws.read(cx).store.read(cx).core.clone()
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
        "explorer-folders-shot" => return scene(ws, &pg_id, cx).await,
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
    r.check("a folder: still no folder chip in the search field", probed("x-scope", cx).is_none(), "");

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
    naming_field(ws, &t, &names, r, cx).await;

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

    // No folder chip: the search runs over every folder.
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.expand_id(&format!("c:{pg_id}"), cx)));
    let public = format!("c:{pg_id}/d:{}/s:public", pg.database);
    until(cx, 10, |cx| cx.update(|_, cx| t.read(cx).has_row(&public)).unwrap_or(false)).await;
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.expand_id(&public, cx)));
    pause(cx, 500).await;
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.set_filter("orders", cx)));
    let found = until(cx, 10, |cx| rows(&t, cx).iter().any(|r| r.trim_start() == "orders")).await;
    r.check("the search runs over every folder: found in Dev", found && probed("x-scope", cx).is_none(), format!("{:?}", rows(&t, cx)));
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.set_filter("", cx)));

    // Deleting a folder keeps its connections.
    let deleted = menu(&t, &fid(&dev.id), Act::DeleteFolder, cx).await;
    let h = heads(&t, &names, cx);
    let kept = cx.update(|_, cx| ws.read(cx).store.read(cx).profiles.len()).unwrap_or(0);
    r.check(
        "Delete folder: gone, its connection back at the top level (nothing deleted)",
        deleted && folder_named(ws, "Dev", cx).is_none() && kept == 3 && h == ["Live", "  Search", pg_name.as_str(), "Billing"],
        format!("{h:?} {kept}"),
    );
    focus_tree(&t, cx);
    key(cx, "cmd-z");
    let back = folder_named(ws, "Dev", cx);
    r.check("⌘Z brings the folder back with its connection", back.is_some_and(|d| d.holds(&pg_id)), format!("{:?}", folders(ws, cx)));

    // Reordering folders: a drag onto another folder's row (its top half) puts it before.
    let order = |cx: &mut AsyncWindowContext| folders(ws, cx).into_iter().map(|d| d.name).collect::<Vec<_>>();
    let live = folder_named(ws, "Live", cx);
    let (from, to) = (row_point(&t, &fid(&dev.id), cx), row_point(&t, &fid(&live.as_ref().map(|d| d.id.clone()).unwrap_or_default()), cx));
    let mut during = None;
    if let (Some(from), Some(to)) = (from, to) {
        crate::history_check::drag(cx, from, point(to.x, to.y - px(6.)), |cx| during = cx.update(|_, cx| t.read(cx).demo_folder_drop()).ok().flatten()).await;
    }
    let h = heads(&t, &names, cx);
    r.check(
        "a folder dragged onto another folder's row moves before it, with its connections",
        during == Some(live.as_ref().map(|d| d.id.clone())) && order(cx) == ["Dev", "Live"] && h == ["Dev", format!("  {pg_name}").as_str(), "Live", "  Search", "Billing"],
        format!("{during:?} {h:?}"),
    );
    focus_tree(&t, cx);
    key(cx, "cmd-z");
    r.check("⌘Z puts the folders back in their order", order(cx) == ["Live", "Dev"], format!("{:?}", order(cx)));
    let up = menu(&t, &fid(&dev.id), Act::MoveFolder(true), cx).await;
    let moved = order(cx);
    let down = menu(&t, &fid(&dev.id), Act::MoveFolder(false), cx).await;
    r.check(
        "Move up / Move down on the folder's menu",
        up && down && moved == ["Dev", "Live"] && order(cx) == ["Live", "Dev"],
        format!("{moved:?} {:?}", order(cx)),
    );

    collapse_all(ws, &t, &pg, &dev, r, cx).await;

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

    picked_rows(ws, &t, &pg, &pw, r, cx).await;

    fav_scope(ws, r, false, cx).await;
}

/// The header's Collapse all, by a click on its item.
async fn click_collapse_all(t: &Entity<SchemaTree>, cx: &mut AsyncWindowContext) -> bool {
    let _ = cx.update(|window, cx| t.update(cx, |t, cx| t.open_menu(None, MenuAnchor::Header(point(px(60.), px(60.))), window, cx)));
    pause(cx, 200).await;
    draw(cx);
    let Some(p) = cx.update(|_, cx| t.read(cx).demo_menu_item_point(Act::CollapseAll)).ok().flatten() else { return false };
    click(cx, p);
    pause(cx, 50).await;
    true
}

/// Collapse all shuts every level: what is opened after it is closed below, whatever was open
/// before and whatever opens by itself otherwise. `pg` is in `dev`, and connected.
async fn collapse_all(ws: &Entity<Workspace>, t: &Entity<SchemaTree>, pg: &dataman_core::ConnectionProfile, dev: &dataman_core::ConnFolder, r: &mut Report, cx: &mut AsyncWindowContext) {
    let c = format!("c:{}", pg.id);
    let db = format!("{c}/d:{}", pg.database);
    let schema = format!("{db}/s:public");
    let tables = format!("{schema}/tables");
    let orders = format!("{schema}/r:orders");
    let pinned_group = format!("{c}/pinned");
    let pinned = pin_id(&pg.id, &pg.database, "public", "orders");
    let cols = format!("{orders}/cols");
    let open = |id: &str, cx: &mut AsyncWindowContext| {
        let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.expand_id(id, cx)));
    };
    let shows = async |id: &str, cx: &mut AsyncWindowContext| until(cx, 10, |cx| cx.update(|_, cx| t.read(cx).has_row(id)).unwrap_or(false)).await;
    let open_under = |id: &str, cx: &mut AsyncWindowContext| cx.update(|_, cx| t.read(cx).demo_open_under(id)).unwrap_or_default();

    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.set_advanced(true, cx)));
    open(&c, cx);
    let current = shows(&schema, cx).await;
    open(&schema, cx);
    let group = shows(&tables, cx).await;
    open(&tables, cx);
    let table = shows(&orders, cx).await;
    open(&orders, cx);
    let columns = shows(&format!("{cols}/id"), cx).await;
    r.check("before Collapse all: a connection opens its current database, a table its columns", current && group && table && columns, format!("{current} {group} {table} {columns}"));
    let was_pinned = menu(t, &orders, Act::Pin, cx).await && shows(&pinned, cx).await;
    open(&pinned, cx);
    r.check("a table pinned and opened, for what Collapse all has to shut", was_pinned && shows(&format!("{pinned}/cols"), cx).await, format!("{was_pinned}"));

    let clicked = click_collapse_all(t, cx).await;
    let top = rows(t, cx);
    r.check(
        "Collapse all: only the folders and the connections in none are left",
        clicked && top == ["Live", "Dev", "Billing"],
        format!("{clicked} {top:?}"),
    );

    let Some(p) = row_point(t, &fid(&dev.id), cx) else {
        r.check("the folder's row is on screen", false, "");
        return;
    };
    click(cx, p);
    let in_folder = shows(&c, cx).await;
    let Some(p) = row_point(t, &c, cx) else {
        r.check("the connection's row is on screen", false, "");
        return;
    };
    click(cx, p);
    let dbs = shows(&db, cx).await;
    let under = open_under(&c, cx);
    r.check(
        "the connection opened again: its database and its pinned group are listed, closed",
        in_folder && dbs && under.is_empty() && cx.update(|_, cx| t.read(cx).has_row(&pinned_group)).unwrap_or(false) && !cx.update(|_, cx| t.read(cx).has_row(&pinned)).unwrap_or(true),
        format!("{in_folder} {dbs} open {under:?}"),
    );
    open(&db, cx);
    let down = shows(&schema, cx).await;
    open(&schema, cx);
    let down = down && shows(&tables, cx).await;
    r.check("the database and the schema opened: nothing under them is open", down && open_under(&db, cx) == [schema.clone()], format!("{down} {:?}", open_under(&db, cx)));
    open(&tables, cx);
    let down = shows(&orders, cx).await;
    open(&orders, cx);
    let listed = shows(&cols, cx).await;
    r.check("the table opened: its columns are listed, closed", down && listed && open_under(&orders, cx).is_empty(), format!("{down} {listed} {:?}", open_under(&orders, cx)));

    let again = click_collapse_all(t, cx).await;
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.reveal_relation(&pg.id, &pg.database, "public", "orders", dataman_core::schema::RelKind::Table, Some("id"), cx)));
    let want = format!("{cols}/id");
    let revealed = until(cx, 10, |cx| cx.update(|_, cx| t.read(cx).selected_id() == Some(want.as_str())).unwrap_or(false)).await;
    r.check("a column revealed after Collapse all still opens the way to it", again && revealed, format!("{again} {:?}", cx.update(|_, cx| t.read(cx).selected_id().map(str::to_string)).ok().flatten()));

    let again = click_collapse_all(t, cx).await;
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.set_advanced(false, cx)));
    open(&fid(&dev.id), cx);
    let in_folder = shows(&c, cx).await;
    open(&c, cx);
    let dbs = shows(&db, cx).await;
    let under = open_under(&c, cx);
    r.check("the simple view, after Collapse all: the database is listed, closed", again && in_folder && dbs && under.is_empty(), format!("{again} {in_folder} {dbs} {under:?}"));

    let unpinned = cx.update(|_, cx| t.update(cx, |t, cx| {
        let done = core_of(ws, cx).pins.unpin(&pg.id, &pg.database, "public", "orders");
        t.rebuild(cx);
        done
    }));
    r.check("the pin taken back", matches!(unpinned, Ok(Ok(_))), "");
}

/// A right click on the row; what its menu lists.
async fn right_click(t: &Entity<SchemaTree>, row: &str, cx: &mut AsyncWindowContext) -> Vec<Act> {
    let Some(p) = row_point(t, row, cx) else {
        eprintln!("check: row {row} not on screen");
        return vec![];
    };
    click_with(cx, p, MouseButton::Right, Modifiers::default(), 1);
    pause(cx, 200).await;
    draw(cx);
    cx.update(|_, cx| t.read(cx).demo_menu_acts()).unwrap_or_default()
}

/// A click on an item of the menu that is open.
async fn menu_click(t: &Entity<SchemaTree>, act: Act, cx: &mut AsyncWindowContext) -> bool {
    let Some(p) = cx.update(|_, cx| t.read(cx).demo_menu_item_point(act)).ok().flatten() else { return false };
    click(cx, p);
    pause(cx, 50).await;
    true
}

fn label(t: &Entity<SchemaTree>, act: Act, cx: &mut AsyncWindowContext) -> Option<String> {
    cx.update(|_, cx| t.read(cx).demo_menu_label(act)).ok().flatten()
}

/// `first` picked, then `second` with a ⌘-click.
fn pick_two(t: &Entity<SchemaTree>, first: &str, second: &str, cx: &mut AsyncWindowContext) -> bool {
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| {
        t.demo_unpick();
        t.select_id(first, cx);
    }));
    let Some(p) = row_point(t, second, cx) else { return false };
    click_with(cx, p, MouseButton::Left, Modifiers { platform: true, ..Default::default() }, 1);
    draw(cx);
    true
}

/// Rows picked together, folders among them: the menu lists what applies to all that they stand
/// for, and each item acts on all of it, as one undo step.
async fn picked_rows(ws: &Entity<Workspace>, t: &Entity<SchemaTree>, pg: &dataman_core::ConnectionProfile, pw: &str, r: &mut Report, cx: &mut AsyncWindowContext) {
    let saved: Vec<String> = cx.update(|_, cx| {
        let core = core_of(ws, cx);
        let ids = ["Alpha", "Beta", "Gamma"].map(|n| {
            let mut p = pg.clone();
            p.id = dataman_core::ConnectionProfile::new(n).id;
            p.name = n.to_string();
            let id = p.id.clone();
            core.save_connection(p, Some(pw), None).expect("saved");
            id
        });
        ws.read(cx).store.clone().update(cx, |s, cx| s.reload(cx));
        ids.to_vec()
    }).unwrap_or_default();
    let (alpha, beta, gamma) = (saved[0].clone(), saved[1].clone(), saved[2].clone());
    let (team_id, side_id) = cx.update(|_, cx| {
        let store = ws.read(cx).store.clone();
        let mut made = (String::new(), String::new());
        let done = store.update(cx, |s, cx| s.edit_folders(|f| {
            let mut team = dataman_core::ConnFolder::new("Team", f);
            team.connections = vec![alpha.clone(), beta.clone()];
            made.0 = team.id.clone();
            f.push(team);
            let side = dataman_core::ConnFolder::new("Side", f);
            made.1 = side.id.clone();
            f.push(side);
        }, cx));
        done.expect("the folders saved");
        made
    }).unwrap();
    let (team, side, pg_row, gamma_row) = (fid(&team_id), fid(&side_id), format!("c:{}", pg.id), format!("c:{gamma}"));
    let up = |id: &str, cx: &mut AsyncWindowContext| is_connected(ws, id, cx);
    let holds = |name: &str, conns: &[&String], cx: &mut AsyncWindowContext| folder_named(ws, name, cx).is_some_and(|d| d.connections.len() == conns.len() && conns.iter().all(|c| d.holds(c)));

    // A shift range from a folder: the folder and the connection stand for three connections.
    let Some(p) = row_point(t, &team, cx) else {
        r.check("the folder's row is on screen", false, "");
        return;
    };
    click(cx, p);
    let alpha_row = format!("c:{alpha}");
    let shut = until(cx, 5, |cx| !cx.update(|_, cx| t.read(cx).has_row(&alpha_row)).unwrap_or(true)).await;
    let Some(p) = row_point(t, &pg_row, cx) else {
        r.check("the connection's row is on screen", false, "");
        return;
    };
    click_with(cx, p, MouseButton::Left, Modifiers { shift: true, ..Default::default() }, 1);
    draw(cx);
    let (rows_picked, conns) = cx.update(|_, cx| (t.read(cx).demo_picked().to_vec(), t.read(cx).demo_picked_conns(cx))).unwrap();
    r.check(
        "shift-click from a folder: the rows between are picked, the folder standing for its connections",
        shut && rows_picked == [team.clone(), side.clone(), pg_row.clone()] && conns.len() == 3 && [&alpha, &beta, &pg.id].iter().all(|c| conns.contains(*c)),
        format!("{rows_picked:?} {conns:?}"),
    );

    // One connected, two not.
    let acts = right_click(t, &pg_row, cx).await;
    let (connect, disconnect, delete) = (label(t, Act::Connect, cx), label(t, Act::Disconnect, cx), label(t, Act::DeleteConnection, cx));
    r.check(
        "the menu inside the selection: Connect and Disconnect say how many, nothing that needs one connection",
        connect.as_deref() == Some("Connect 2 connections")
            && disconnect.as_deref() == Some("Disconnect 1 connection")
            && delete.as_deref() == Some("Delete 3 connections…")
            && acts.contains(&Act::MoveToNewFolder)
            && [Act::EditConnection, Act::DuplicateConnection, Act::NewTab, Act::History, Act::CopyUrl, Act::Reconnect, Act::Reload, Act::RenameFolder, Act::DeleteFolder].iter().all(|a| !acts.contains(a)),
        format!("{connect:?} {disconnect:?} {delete:?} {acts:?}"),
    );
    let still = cx.update(|_, cx| t.read(cx).demo_picked().len()).unwrap_or(0);
    r.check("a right click inside the selection keeps it", still == 3, format!("{still}"));
    let clicked = menu_click(t, Act::Connect, cx).await;
    let both = until(cx, 30, |cx| up(&alpha, cx) && up(&beta, cx)).await;
    r.check("Connect connects those that were not", clicked && both && up(&pg.id, cx), format!("{clicked} {both}"));

    // All connected; a transaction open on one of them asks first, once, for all.
    let tab = cx.update(|_, cx| ws.read(cx).active_tab().cloned()).ok().flatten();
    let Some(tab) = tab else {
        r.check("a tab to hold a transaction", false, "");
        return;
    };
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.run(vec!["BEGIN".to_string()], cx)));
    let open = until(cx, 10, |cx| cx.update(|_, cx| tab.read(cx).tx_open()).unwrap_or(false)).await;
    let acts = right_click(t, &pg_row, cx).await;
    let disconnect = label(t, Act::Disconnect, cx);
    r.check(
        "all connected: Disconnect, Reconnect and Reload for all, and no Connect",
        disconnect.as_deref() == Some("Disconnect 3 connections") && [Act::Reconnect, Act::Reload].iter().all(|a| acts.contains(a)) && !acts.contains(&Act::Connect),
        format!("{disconnect:?} {acts:?}"),
    );
    let clicked = menu_click(t, Act::Disconnect, cx).await;
    draw(cx);
    let asked = cx.update(|_, cx| ws.read(cx).tx_prompt.as_ref().map(|p| (p.then.verb(), p.tabs.len()))).ok().flatten();
    let held = up(&alpha, cx) && up(&beta, cx) && up(&pg.id, cx);
    r.check("a transaction open: it asks before disconnecting any", open && clicked && asked == Some(("disconnect", 1)) && held, format!("{open} {clicked} {asked:?} {held}"));
    pause(cx, 400).await;
    key(cx, "cmd-backspace");
    let all_off = until(cx, 10, |cx| !up(&alpha, cx) && !up(&beta, cx) && !up(&pg.id, cx)).await;
    r.check("rolled back: all of them disconnect", all_off, "");

    // All off: Connect for all.
    let acts = right_click(t, &pg_row, cx).await;
    let connect = label(t, Act::Connect, cx);
    r.check("all off: Connect for all, no Disconnect", connect.as_deref() == Some("Connect 3 connections") && !acts.contains(&Act::Disconnect), format!("{connect:?} {acts:?}"));
    let clicked = menu_click(t, Act::Connect, cx).await;
    let all_up = until(cx, 30, |cx| up(&alpha, cx) && up(&beta, cx) && up(&pg.id, cx)).await;
    r.check("Connect brings them all back", clicked && all_up, format!("{clicked} {all_up}"));

    // Reconnected together: each tab goes back to its own, whichever comes up first.
    let tabs: Vec<_> = [&alpha, &beta, &pg.id]
        .into_iter()
        .map(|id| (id.clone(), cx.update(|window, cx| ws.update(cx, |w, cx| w.open_sql_tab(id, None, "", false, window, cx))).ok().flatten()))
        .collect();
    let on_its_own = |cx: &mut AsyncWindowContext| {
        tabs.iter().all(|(id, tab)| tab.as_ref().is_some_and(|tab| cx.update(|_, cx| tab.read(cx).connection().is_some_and(|c| c.profile.id == *id)).unwrap_or(false)))
    };
    let before_reconnect = on_its_own(cx);
    let _ = right_click(t, &pg_row, cx).await;
    let clicked = menu_click(t, Act::Reconnect, cx).await;
    let back = until(cx, 30, |cx| up(&alpha, cx) && up(&beta, cx) && up(&pg.id, cx) && on_its_own(cx)).await;
    let holding: Vec<_> = tabs.iter().map(|(_, tab)| tab.as_ref().and_then(|tab| cx.update(|_, cx| tab.read(cx).connection().map(|c| c.profile.name.clone())).ok().flatten())).collect();
    r.check("Reconnect: all of them come back, each tab on its own connection", before_reconnect && clicked && back, format!("{before_reconnect} {clicked} {holding:?}"));

    // Outside the selection: that row's own menu, and the selection is gone.
    let acts = right_click(t, &gamma_row, cx).await;
    let (own, left) = (label(t, Act::Connect, cx), cx.update(|_, cx| t.read(cx).demo_picked().len()).unwrap_or(9));
    r.check("a right click outside the selection: that row's menu, nothing picked", acts.contains(&Act::EditConnection) && own.as_deref() == Some("Connect") && left == 0, format!("{own:?} {left} {acts:?}"));
    key(cx, "escape");

    // Folders alone: what they hold, and deleting the folders themselves.
    let picked = pick_two(t, &side, &team, cx);
    let acts = right_click(t, &team, cx).await;
    let (folders_delete, conns_delete, disconnect) = (label(t, Act::DeleteFolder, cx), label(t, Act::DeleteConnection, cx), label(t, Act::Disconnect, cx));
    r.check(
        "two folders picked: their connections' actions, and Delete for the folders",
        picked
            && folders_delete.as_deref() == Some("Delete 2 folders only")
            && conns_delete.as_deref() == Some("Delete 2 connections…")
            && disconnect.as_deref() == Some("Disconnect 2 connections")
            && !acts.contains(&Act::RenameFolder),
        format!("{picked} {folders_delete:?} {conns_delete:?} {disconnect:?} {acts:?}"),
    );
    let total = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).store.read(cx).profiles.len()).unwrap_or(0);
    let before = total(cx);
    let clicked = menu_click(t, Act::DeleteFolder, cx).await;
    let gone = folder_named(ws, "Team", cx).is_none() && folder_named(ws, "Side", cx).is_none();
    r.check("Delete 2 folders: gone, their connections kept", clicked && gone && total(cx) == before, format!("{clicked} {gone} {}", total(cx)));
    focus_tree(t, cx);
    key(cx, "cmd-z");
    r.check("⌘Z brings both back, with what they held", holds("Team", &[&alpha, &beta], cx) && holds("Side", &[], cx), format!("{:?}", folders(ws, cx)));

    // One folder, not picked: its own menu, with what its connections can do.
    let acts = right_click(t, &team, cx).await;
    let disconnect = label(t, Act::Disconnect, cx);
    r.check(
        "a folder's own menu: its connections' actions over its usual items",
        disconnect.as_deref() == Some("Disconnect 2 connections") && [Act::Reconnect, Act::Reload, Act::RenameFolder, Act::DeleteFolder].iter().all(|a| acts.contains(a)) && !acts.contains(&Act::Connect),
        format!("{disconnect:?} {acts:?}"),
    );
    let clicked = menu_click(t, Act::Disconnect, cx).await;
    let off = until(cx, 10, |cx| !up(&alpha, cx) && !up(&beta, cx)).await;
    r.check("Disconnect on the folder: those in it, not the others", clicked && off && up(&pg.id, cx), format!("{clicked} {off}"));

    // Moved together: from the menu, and by a drag.
    let index_of = |name: &str, cx: &mut AsyncWindowContext| folders(ws, cx).iter().position(|d| d.name == name).unwrap_or(usize::MAX);
    let picked = pick_two(t, &team, &gamma_row, cx);
    let acts = right_click(t, &gamma_row, cx).await;
    let to = Act::MoveToFolder(index_of("Side", cx));
    let listed = acts.contains(&to);
    let _ = cx.update(|window, cx| t.update(cx, |t, cx| t.demo_menu_run(to, window, cx)));
    draw(cx);
    r.check("Move to folder: the folder's connections and the other, together", picked && listed && holds("Side", &[&alpha, &beta, &gamma], cx) && holds("Team", &[], cx), format!("{picked} {listed} {:?}", folders(ws, cx)));
    focus_tree(t, cx);
    key(cx, "cmd-z");
    r.check("⌘Z moves them all back in one step", holds("Team", &[&alpha, &beta], cx) && holds("Side", &[], cx), format!("{:?}", folders(ws, cx)));

    let picked = pick_two(t, &team, &gamma_row, cx);
    let (from, to) = (row_point(t, &gamma_row, cx), row_point(t, &side, cx));
    let mut during = None;
    if let (Some(from), Some(to)) = (from, to) {
        crate::history_check::drag(cx, from, to, |cx| during = cx.update(|_, cx| t.read(cx).demo_conn_drop()).ok().flatten()).await;
    }
    r.check("dragged onto a folder, all that is picked goes in it", picked && during == Some(ConnDrop::Into(side_id.clone())) && holds("Side", &[&alpha, &beta, &gamma], cx), format!("{picked} {during:?} {:?}", folders(ws, cx)));
    focus_tree(t, cx);
    key(cx, "cmd-z");

    // Deleted together: asked first, as one connection is.
    let picked = pick_two(t, &team, &gamma_row, cx);
    let _ = right_click(t, &gamma_row, cx).await;
    let clicked = menu_click(t, Act::DeleteConnection, cx).await;
    draw(cx);
    let asked = cx.update(|_, cx| ws.read(cx).confirm_delete.clone()).ok().flatten();
    r.check(
        "Delete asks first, for all three, and deletes nothing yet",
        picked && clicked && asked.as_ref().is_some_and(|a| a.len() == 3) && total(cx) == before,
        format!("{picked} {clicked} {asked:?} {}", total(cx)),
    );
    key(cx, "escape");
    draw(cx);
    let kept = total(cx) == before && cx.update(|_, cx| ws.read(cx).confirm_delete.is_none()).unwrap_or(false);
    r.check("esc: nothing deleted", kept, format!("{}", total(cx)));
    let _ = right_click(t, &gamma_row, cx).await;
    let _ = menu_click(t, Act::DeleteConnection, cx).await;
    draw(cx);
    let confirmed = click_probe("dc-delete", cx);
    draw(cx);
    let store_has = |id: &str, cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).store.read(cx).profile(id).is_some()).unwrap_or(false);
    let deleted = !store_has(&alpha, cx) && !store_has(&beta, cx) && !store_has(&gamma, cx);
    r.check(
        "confirmed: the three are gone, the folder stays, the others stay",
        confirmed && deleted && total(cx) == before - 3 && holds("Team", &[], cx) && store_has(&pg.id, cx) && up(&pg.id, cx),
        format!("{confirmed} {deleted} {} {:?}", total(cx), folders(ws, cx)),
    );
    let picked_after = cx.update(|_, cx| t.read(cx).demo_picked().len()).unwrap_or(9);
    r.check("nothing deleted stays picked", picked_after <= 1, format!("{picked_after}"));
    focus_tree(t, cx);
    key(cx, "cmd-z");
    draw(cx);
    r.check(
        "⌘Z brings all three back in one step, in their folder",
        store_has(&alpha, cx) && store_has(&beta, cx) && store_has(&gamma, cx) && holds("Team", &[&alpha, &beta], cx),
        format!("{} {:?}", total(cx), folders(ws, cx)),
    );

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.request_delete_connections(saved.clone(), true, window, cx)));
    let _ = cx.update(|_, cx| {
        ws.read(cx).store.clone().update(cx, |s, cx| s.edit_folders(|f| f.clear(), cx))
    });
    draw(cx);
}

/// The field a folder is named in, from a new folder until the focus leaves it.
async fn naming_field(ws: &Entity<Workspace>, t: &Entity<SchemaTree>, names: &[&str], r: &mut Report, cx: &mut AsyncWindowContext) {
    let text = |cx: &mut AsyncWindowContext| cx.update(|_, cx| t.read(cx).demo_naming(cx)).ok().flatten().map(|n| n.1);
    let in_field = |cx: &mut AsyncWindowContext| cx.update(|window, cx| t.read(cx).demo_naming_focused(window, cx)).unwrap_or(false);
    let in_tree = |cx: &mut AsyncWindowContext| cx.update(|window, cx| t.read(cx).demo_has_focus(window)).unwrap_or(false);
    let points = |cx: &mut AsyncWindowContext| cx.update(|_, cx| t.read(cx).demo_naming_points(cx)).ok().flatten();
    let live = folder_named(ws, "Live", cx).map(|d| fid(&d.id)).unwrap_or_default();

    click_probe("x-new-folder", cx);
    pause(cx, 50).await;
    typ(cx, "Tem");
    let Some((field, row)) = points(cx) else {
        r.check("the field being named has bounds", false, "");
        return;
    };
    click(cx, field);
    pause(cx, 30).await;
    typ(cx, "p");
    r.check("a click inside the field keeps the focus: what is typed next goes in", in_field(cx) && text(cx).as_deref() == Some("Temp"), format!("{:?} focused {}", text(cx), in_field(cx)));
    click(cx, row);
    pause(cx, 30).await;
    typ(cx, "1");
    r.check("a click on the row around the field leaves it being named", in_field(cx) && text(cx).as_deref() == Some("Temp1"), format!("{:?} focused {}", text(cx), in_field(cx)));

    let rows_n = cx.update(|_, cx| t.read(cx).demo_rows().len()).unwrap_or(0);
    if let Some(top) = row_point(t, &live, cx) {
        click(cx, point(top.x, top.y + px(22. * rows_n as f32 + 60.)));
    }
    pause(cx, 30).await;
    r.check(
        "a click on the empty tree names the folder with what was typed, and the tree has the keys",
        text(cx).is_none() && folder_named(ws, "Temp1", cx).is_some() && in_tree(cx),
        format!("{:?} {:?} tree focus {}", text(cx), folders(ws, cx), in_tree(cx)),
    );

    click_probe("x-new-folder", cx);
    pause(cx, 50).await;
    typ(cx, "Elsewhere");
    let _ = cx.update(|window, cx| t.update(cx, |t, cx| t.demo_focus_filter(window, cx)));
    draw(cx);
    pause(cx, 30).await;
    r.check(
        "the focus moved out of the tree: the folder is named, and the focus stays where it went",
        text(cx).is_none() && folder_named(ws, "Elsewhere", cx).is_some() && !in_tree(cx),
        format!("{:?} {:?} tree focus {}", text(cx), folders(ws, cx), in_tree(cx)),
    );

    let _ = menu(t, &live, Act::RenameFolder, cx).await;
    pause(cx, 30).await;
    key(cx, "enter");
    pause(cx, 30).await;
    let accepted = heads(t, names, cx);
    key(cx, "enter");
    let shut = heads(t, names, cx);
    key(cx, "enter");
    r.check(
        "↩ in the field accepts the name and leaves the folder open; the next ↩ shuts it",
        text(cx).is_none() && accepted.iter().any(|h| h == "  Search") && !shut.iter().any(|h| h == "  Search") && heads(t, names, cx) == accepted,
        format!("{accepted:?} {shut:?}"),
    );

    click_probe("x-new-folder", cx);
    pause(cx, 50).await;
    typ(cx, "First");
    click_probe("x-new-folder", cx);
    pause(cx, 50).await;
    r.check(
        "another new folder while one is named: the first keeps what was typed, the second is named",
        folder_named(ws, "First", cx).is_some() && text(cx).is_some_and(|n| n.starts_with("New folder")) && in_field(cx),
        format!("{:?} {:?}", text(cx), folders(ws, cx)),
    );
    typ(cx, "whoops");
    key(cx, "escape");
    pause(cx, 30).await;
    r.check("esc keeps the old name, and the tree has the keys", text(cx).is_none() && folder_named(ws, "whoops", cx).is_none() && in_tree(cx), format!("{:?} tree focus {}", folders(ws, cx), in_tree(cx)));

    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.set_filter("nothing like this", cx)));
    draw(cx);
    click_probe("x-new-folder", cx);
    pause(cx, 50).await;
    let filter = cx.update(|_, cx| t.read(cx).demo_filter().read(cx).text().to_string()).unwrap_or_default();
    r.check("a new folder under a search clears it and is named on its row", text(cx).is_some() && in_field(cx) && filter.is_empty(), format!("{:?} filter {filter:?}", text(cx)));
    key(cx, "escape");
    pause(cx, 30).await;

    for d in folders(ws, cx).into_iter().filter(|d| d.name != "Live") {
        let _ = menu(t, &fid(&d.id), Act::DeleteFolder, cx).await;
    }
}

/// A folder's or a favorite's row, without its arrow or indent.
fn bare(rows: Vec<String>) -> Vec<String> {
    rows.into_iter().map(|r| r.trim_start_matches(['▾', '▸', ' ']).to_string()).collect()
}

/// Favorites on two connections (and one on none), in folders and at the top level: the
/// connection dropdown under the search field narrows them as History's narrows its runs.
async fn fav_scope(ws: &Entity<Workspace>, r: &mut Report, shot: bool, cx: &mut AsyncWindowContext) {
    let history = cx.update(|_, cx| ws.read(cx).history.clone()).unwrap();
    let pg = cx.update(|_, cx| ws.read(cx).store.read(cx).profiles.first().map(|p| p.id.clone())).ok().flatten();
    let other = add_saved(ws, &[("Reporting", Some("mint"))], cx).first().cloned();
    let (Some(pg), Some(other)) = (pg, other) else {
        r.check("the favorites' connections", false, "");
        return;
    };
    let _ = cx.update(|_, cx| {
        let core = ws.read(cx).store.read(cx).core.clone();
        let reports = core.favorites.add_folder("Monthly reports").ok();
        let ops = core.favorites.add_folder("Ops").ok();
        for (name, sql, conn, db, folder) in [
            ("Orders by day", "SELECT date_trunc('day', created) AS day, count(*) FROM orders GROUP BY 1 ORDER BY 1", Some(&other), Some("sales"), &reports),
            ("Revenue by month", "SELECT date_trunc('month', created), sum(total) FROM orders GROUP BY 1 ORDER BY 1", Some(&other), Some("finance"), &reports),
            ("Locks waiting", "SELECT * FROM pg_locks WHERE NOT granted", Some(&pg), None, &ops),
            ("Top customers", "SELECT customer, count(*) FROM orders GROUP BY 1 ORDER BY 2 DESC LIMIT 10", Some(&pg), None, &None),
            ("Scratch", "SELECT 1", None, None, &None),
        ] {
            let mut f = dataman_core::Favorite::new(name, sql);
            f.connection_id = conn.cloned();
            f.database = db.map(str::to_string);
            f.folder = folder.as_ref().map(|d| d.id.clone());
            let _ = core.favorites.add(f);
        }
        ws.update(cx, |w, cx| w.demo_favorites_view(cx));
        history.update(cx, |h, cx| h.reload_favorites(cx));
    });
    pause(cx, 300).await;
    draw(cx);
    let listed = crate::history_check::rows(&history, cx);
    let all = cx.update(|_, cx| history.read(cx).demo_fav_scope().0.is_none()).unwrap_or(false);
    r.check("Favorites: the connection dropdown under the search field, on All connections", probed("fav-scope", cx).is_some() && all, format!("{listed:?}"));
    let opened = click_probe("fav-scope", cx);
    pause(cx, 150).await;
    let menu_open = cx.update(|_, cx| history.read(cx).demo_fav_scope().1).unwrap_or(false);
    let listed = probed(&format!("fscope:{pg}"), cx).is_some() && probed(&format!("fscope:{other}"), cx).is_some() && probed(&format!("fscope:{other}:sales"), cx).is_some();
    r.check("a click opens it: every connection, each with its favorites' databases", opened && menu_open && listed, "");
    // In a folder: the dropdown groups it as the explorer does, and a click on the folder folds it.
    let _ = cx.update(|_, cx| {
        let store = ws.read(cx).store.clone();
        store.update(cx, |s, cx| {
            s.edit_folders(
                |all| {
                    let mut d = dataman_core::ConnFolder::new("Reports", all);
                    d.connections.push(other.clone());
                    all.push(d);
                },
                cx,
            )
        })
    });
    pause(cx, 150).await;
    draw(cx);
    let fid = folder_named(ws, "Reports", cx).map(|d| d.id).unwrap_or_default();
    let head = format!("fscope:folder:{fid}");
    r.check("its folder heads it in the dropdown, the connection under it", probed(&head, cx).is_some() && probed(&format!("fscope:{other}"), cx).is_some(), &head);
    if shot {
        shoot(cx);
        return;
    }
    click_probe(&head, cx);
    pause(cx, 150).await;
    draw(cx);
    let still = cx.update(|_, cx| history.read(cx).demo_fav_scope().1).unwrap_or(false);
    r.check(
        "a click on the folder folds it: its connection goes, the others stay, the menu stays open",
        still && probed(&format!("fscope:{other}"), cx).is_none() && probed(&format!("fscope:{pg}"), cx).is_some(),
        "",
    );
    click_probe(&head, cx);
    pause(cx, 150).await;
    draw(cx);
    r.check("a second click opens it again", probed(&format!("fscope:{other}:sales"), cx).is_some(), "");
    let picked = click_probe(&format!("fscope:{pg}"), cx);
    pause(cx, 100).await;
    let shown = bare(crate::history_check::rows(&history, cx));
    let (scope, still, _) = cx.update(|_, cx| history.read(cx).demo_fav_scope()).unwrap_or_default();
    r.check(
        "a connection picked: Favorites lists its favorites alone (a folder of only others' goes)",
        picked && scope == Some((pg.clone(), None)) && !still && shown.iter().any(|r| r == "Ops") && shown.iter().any(|r| r == "Top customers") && !shown.iter().any(|r| r == "Monthly reports" || r == "Scratch"),
        format!("{scope:?} {shown:?}"),
    );
    let _ = cx.update(|window, cx| history.update(cx, |h, cx| h.demo_search("orders", window, cx)));
    let some = crate::history_check::rows(&history, cx);
    r.check("the search runs in that connection only", some == ["Top customers"], format!("{some:?}"));
    let _ = cx.update(|window, cx| history.update(cx, |h, cx| h.demo_search("", window, cx)));
    click_probe("fav-scope", cx);
    pause(cx, 150).await;
    let picked = click_probe(&format!("fscope:{other}:sales"), cx);
    pause(cx, 100).await;
    let shown = bare(crate::history_check::rows(&history, cx));
    r.check(
        "a database picked: that database's favorites alone",
        picked && !shown.is_empty() && shown.iter().all(|r| r == "Monthly reports" || r == "Orders by day"),
        format!("{shown:?}"),
    );
    let history_scope = cx.update(|_, cx| history.read(cx).demo_scope().0).ok().flatten();
    r.check("History's own connection filter is left as it was", history_scope.is_none(), format!("{history_scope:?}"));
    click_probe("fav-scope", cx);
    pause(cx, 150).await;
    let picked = click_probe("fscope:all", cx);
    pause(cx, 100).await;
    let (scope, _, _) = cx.update(|_, cx| history.read(cx).demo_fav_scope()).unwrap_or_default();
    let back = bare(crate::history_check::rows(&history, cx));
    r.check(
        "All connections: everything is listed again, a favorite of no connection too",
        picked && scope.is_none() && ["Monthly reports", "Ops", "Top customers", "Scratch"].iter().all(|n| back.iter().any(|r| r == n)),
        format!("{scope:?} {back:?}"),
    );
}

/// Two colored folders (one open, one shut) and loose connections.
async fn scene(ws: &Entity<Workspace>, pg_id: &str, cx: &mut AsyncWindowContext) {
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
            t.arrangement_changed(cx);
            t.collapse_id(&fid(&stage.id), cx);
            t.collapse_id(&format!("c:{pg_id}"), cx);
        });
    });
    pause(cx, 1200).await;
    let _ = cx.update(|_, cx| t.update(cx, |t, cx| t.select_id(&format!("c:{}", ids[0]), cx)));
    pause(cx, 400).await;
    shoot(cx);
}
