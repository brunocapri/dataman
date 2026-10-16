//! `undo-check` (`--demo`): the app's undo and redo (`undo`), driven with real key events through
//! the window — ⌘Z / ⌃Z and ⇧⌘Z / ⌃⇧Z with the focus on the explorer, the grid or the favorites —
//! for each kind of step: a favorite added with ⌘D, renamed, removed with its star; a folder made
//! and named (one step), a favorite dragged into it, the folder renamed, moved and deleted with
//! its query (after its "Sure?"); a table pinned and unpinned; the connections reordered by a
//! drag; a connection deleted (its saved password, history and pins back with it); a panel moved
//! (the palette's move), the panel layout reset, a seam dragged; a tab closed with ⌘W and an
//! editor group closed (reopened with their script and connection). Also: nothing to undo says
//! so, a step whose favorite is gone is dropped with a notice, the palette names the next undo
//! and redo, Edit ▸ Undo is the app's outside text, and in the SQL editor and a text field ⌘Z is
//! the text's own undo (⌃Z nothing), the app's steps untouched. PASS/FAIL lines like
//! `folders-check`; with `DATAMAN_SHOT_DIR`, `undo-notice.png` (the notice after an undo).

use gpui::{AsyncWindowContext, Entity, Focusable, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, point, px};

use crate::app::{Region, Workspace};
use crate::history::HistoryPanel;
use crate::history_check::{click, draw, drag, filed, input, key, menu_at, menu_click, named_point, pause, rows, typ};
use crate::tree_menu::Act;

pub fn is_stage(s: &str) -> bool {
    s == "undo-check"
}

struct Report(bool);

impl Report {
    fn check(&mut self, name: &str, pass: bool, detail: String) {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        self.0 &= pass;
    }
}

fn next_undo(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> (Option<String>, Option<String>) {
    cx.update(|_, cx| {
        let u = &ws.read(cx).undo;
        (u.next_undo().map(str::to_string), u.next_redo().map(str::to_string))
    })
    .unwrap_or_default()
}

fn notice(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> String {
    cx.update(|_, cx| ws.read(cx).flash_notice.as_ref().map(|n| n.0.clone())).ok().flatten().unwrap_or_default()
}

fn fav_names(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Vec<String> {
    cx.update(|_, cx| ws.read(cx).store.read(cx).core.favorites.list().into_iter().map(|f| f.name).collect()).unwrap_or_default()
}

fn folder_names(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Vec<String> {
    cx.update(|_, cx| ws.read(cx).store.read(cx).core.favorites.folders().into_iter().map(|d| d.name).collect()).unwrap_or_default()
}

fn conn_ids(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> (Vec<String>, Vec<String>) {
    cx.update(|_, cx| {
        let s = ws.read(cx).store.read(cx);
        (s.profiles.iter().map(|p| p.id.clone()).collect(), s.core.config.connections().into_iter().map(|p| p.id).collect())
    })
    .unwrap_or_default()
}

/// The focus on a region, the way the keys or a click put it there: ⌘⇧E, ⌘⇧G (from the editor:
/// the keys always start somewhere), the favorites as a click on one of their rows focuses them.
fn focus(ws: &Entity<Workspace>, r: Region, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.focus_editor(window, cx)));
    match r {
        Region::Explorer => key(cx, "cmd-shift-e"),
        Region::Grid => key(cx, "cmd-shift-g"),
        _ => {
            let _ = cx.update(|window, cx| {
                let f = ws.read(cx).history.focus_handle(cx);
                window.focus(&f, cx);
            });
            draw(cx);
        }
    }
}

/// The keys go to `r` (and are outside text there).
fn focused_on(ws: &Entity<Workspace>, r: Region, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|window, cx| ws.read(cx).focused_region(window, cx) == Some(r) && crate::undo::outside_text(window)).unwrap_or(false)
}

pub async fn run(ws: &Entity<Workspace>, conn: &str, db: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut r = Report(true);
    let history = cx.update(|_, cx| ws.read(cx).history.clone()).unwrap();
    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    let core = cx.update(|_, cx| ws.read(cx).store.read(cx).core.clone()).unwrap();
    // The demo's query has its rows (the grid can take the focus), the favorites are shown.
    for _ in 0..200 {
        if cx.update(|_, cx| ws.read(cx).active_tab().is_some_and(|t| t.read(cx).result().is_some_and(|rs| rs.rows() > 0))).unwrap_or(false) {
            break;
        }
        pause(cx, 50).await;
    }
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.toggle_history_for_demo(cx)));
    pause(cx, 400).await;

    // Nothing done yet: ⌘Z says so.
    focus(ws, Region::Explorer, cx);
    let on_explorer = focused_on(ws, Region::Explorer, cx);
    key(cx, "cmd-z");
    r.check("⌘Z with nothing done says so", on_explorer && notice(ws, cx) == "Nothing to undo", format!("explorer {on_explorer} notice {:?}", notice(ws, cx)));

    favorites(ws, &history, &mut r, cx).await;
    folders(ws, &history, &mut r, cx).await;
    pins(ws, &tree, conn, db, &mut r, cx).await;
    connections(ws, &tree, &core, conn, db, &mut r, cx).await;
    panels(ws, &mut r, cx).await;
    tabs(ws, conn, &mut r, cx).await;
    text(ws, &tree, &mut r, cx).await;
    println!("{}", if r.0 { "ALL PASS" } else { "SOME CHECKS FAILED" });
    r.0
}

/// ⌘D, a rename, the star; the palette's entries; a step whose favorite is gone.
async fn favorites(ws: &Entity<Workspace>, history: &Entity<HistoryPanel>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let sql = "SELECT 'undo-check' AS marker";
    let _ = cx.update(|window, cx| {
        let editor = ws.read(cx).active_tab().unwrap().read(cx).editor.clone();
        editor.update(cx, |e, cx| e.set_text(sql, cx));
        window.focus(&editor.focus_handle(cx), cx);
    });
    key(cx, "cmd-d");
    let added = fav_names(ws, cx);
    let label = next_undo(ws, cx).0;
    focus(ws, Region::Explorer, cx);
    key(cx, "cmd-z");
    let undone = fav_names(ws, cx);
    let said = notice(ws, cx);
    if std::env::var_os("DATAMAN_SHOT_DIR").is_some() {
        let path = format!("{}/undo-notice.png", std::env::var("DATAMAN_SHOT_DIR").unwrap_or_default());
        let _ = cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
            crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
        });
    }
    r.check(
        "⌘D adds a favorite; ⌘Z on the explorer takes it back, the notice says what was undone",
        added.contains(&"SELECT 'undo-check' AS marker".to_string()) && label.as_deref() == Some("Add favorite 'SELECT 'undo-check' AS marker'") && undone.is_empty() && said == "Undid: Add favorite 'SELECT 'undo-check' AS marker'",
        format!("added {added:?} label {label:?} undone {undone:?} notice {said:?}"),
    );
    // The palette names what Redo would do.
    let items: Vec<(String, bool)> = cx.update(|_, cx| ws.read(cx).palette_items(cx).into_iter().filter(|i| i.label.starts_with("Undo") || i.label.starts_with("Redo")).map(|i| (i.label, i.disabled)).collect()).unwrap_or_default();
    r.check(
        "the palette offers \"Redo Add favorite …\" (and a dim Undo, nothing left)",
        items == [("Undo".to_string(), true), ("Redo Add favorite 'SELECT 'undo-check' AS marker'".to_string(), false)],
        format!("{items:?}"),
    );
    key(cx, "cmd-shift-z");
    let redone = fav_names(ws, cx);
    r.check("⇧⌘Z adds it again", redone == ["SELECT 'undo-check' AS marker"] && notice(ws, cx).starts_with("Redid: Add favorite"), format!("{redone:?} {:?}", notice(ws, cx)));

    // A rename, undone and redone with ⌃Z / ⌃⇧Z on the favorites.
    let _ = cx.update(|window, cx| history.update(cx, |h, cx| h.demo_rename(0, "Marker", window, cx)));
    focus(ws, Region::Side, cx);
    let on_panel = focused_on(ws, Region::Side, cx);
    key(cx, "ctrl-z");
    let back = fav_names(ws, cx);
    key(cx, "ctrl-shift-z");
    let again = fav_names(ws, cx);
    r.check(
        "a favorite renamed: ⌃Z on the favorites names it back, ⌃⇧Z renames it again",
        on_panel && back == ["SELECT 'undo-check' AS marker"] && again == ["Marker"],
        format!("panel {on_panel} {back:?} {again:?}"),
    );

    // Its star: removed, ⌘Z puts it back in its place.
    let _ = cx.update(|_, cx| {
        let core = ws.read(cx).store.read(cx).core.clone();
        let _ = core.favorites.add(dataman_core::Favorite::new("Second", "SELECT 2"));
        history.update(cx, |h, cx| h.demo_show_favorites(cx));
    });
    let mut star = None;
    for _ in 0..20 {
        draw(cx);
        star = cx.update(|_, cx| history.read(cx).demo_item_named("Marker").and_then(|ix| history.read(cx).demo_star_point(ix))).ok().flatten();
        if star.is_some() {
            break;
        }
        pause(cx, 20).await;
    }
    if let Some(p) = star {
        click(cx, p, MouseButton::Left, Modifiers::default());
    }
    let removed = fav_names(ws, cx);
    focus(ws, Region::Side, cx);
    key(cx, "cmd-z");
    let restored = fav_names(ws, cx);
    r.check(
        "a favorite's star removes it; ⌘Z puts it back where it was",
        star.is_some() && removed == ["Second"] && restored == ["Marker", "Second"],
        format!("star {star:?} {removed:?} {restored:?}"),
    );

    // Renamed, then removed by other means (an agent): its undo is dropped with a notice.
    let _ = cx.update(|window, cx| history.update(cx, |h, cx| h.demo_rename(1, "Second renamed", window, cx)));
    let before = next_undo(ws, cx).0;
    let _ = cx.update(|_, cx| {
        let favs = ws.read(cx).store.read(cx).core.favorites.clone();
        if let Some(f) = favs.list().into_iter().find(|f| f.name == "Second renamed") {
            let _ = favs.remove(&f.id);
        }
    });
    focus(ws, Region::Grid, cx);
    let on_grid = focused_on(ws, Region::Grid, cx);
    key(cx, "cmd-z");
    let (said, after) = (notice(ws, cx), next_undo(ws, cx));
    r.check(
        "a step whose favorite is gone is dropped with a notice (⌘Z on the grid); the one before is next",
        on_grid
            && before.as_deref() == Some("Rename favorite 'Second'")
            && said == "Cannot undo Rename favorite 'Second': the favorite is gone"
            && after.0.as_deref() == Some("Rename favorite 'SELECT 'undo-check' AS marker'")
            && after.1.is_none(),
        format!("grid {on_grid} before {before:?} notice {said:?} now {after:?}"),
    );
}

/// "+ folder" named (one step), a favorite dragged in, renamed, moved, deleted after its
/// "Sure?" (an empty one asks too).
async fn folders(ws: &Entity<Workspace>, history: &Entity<HistoryPanel>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|_, cx| history.update(cx, |h, cx| h.demo_show_favorites(cx)));
    draw(cx);
    let new_folder = |cx: &mut AsyncWindowContext| {
        draw(cx);
        if let Some(b) = cx.update(|_, cx| history.read(cx).demo_new_folder_bounds()).ok().flatten() {
            click(cx, b.center(), MouseButton::Left, Modifiers::default());
        }
    };
    new_folder(cx);
    pause(cx, 30).await;
    typ(cx, "Reports");
    key(cx, "enter");
    let label = next_undo(ws, cx).0;
    r.check("\"+ folder\" named in place is one step: \"New folder 'Reports'\"", label.as_deref() == Some("New folder 'Reports'") && folder_names(ws, cx) == ["Reports"], format!("{label:?} {:?}", folder_names(ws, cx)));

    // Marker dragged into it.
    if let (Some(from), Some(to)) = (named_point(history, "Marker", cx).await, named_point(history, "Reports", cx).await) {
        drag(cx, from, to, |_| {}).await;
    }
    let in_it = filed(history, "Marker", cx);
    focus(ws, Region::Side, cx);
    key(cx, "cmd-z");
    let out = filed(history, "Marker", cx);
    key(cx, "cmd-z");
    let gone = folder_names(ws, cx);
    key(cx, "cmd-shift-z");
    let back = (folder_names(ws, cx), filed(history, "Marker", cx));
    key(cx, "cmd-shift-z");
    let refiled = filed(history, "Marker", cx);
    r.check(
        "a favorite dragged into a folder: ⌘Z takes it out, ⌘Z again the folder; ⇧⌘Z ⇧⌘Z both back",
        in_it == Some(Some("Reports".into())) && out == Some(None) && gone.is_empty() && back == (vec!["Reports".to_string()], Some(None)) && refiled == Some(Some("Reports".into())),
        format!("in {in_it:?} out {out:?} gone {gone:?} back {back:?} refiled {refiled:?}"),
    );

    // Renamed from its menu.
    if let Some(p) = named_point(history, "Reports", cx).await {
        click(cx, p, MouseButton::Right, Modifiers::default());
        pause(cx, 30).await;
        menu_click(history, "Rename", cx);
        pause(cx, 30).await;
        key(cx, "cmd-a");
        typ(cx, "Monthly");
        key(cx, "enter");
    }
    let renamed = folder_names(ws, cx);
    focus(ws, Region::Explorer, cx);
    key(cx, "cmd-z");
    let named_back = folder_names(ws, cx);
    key(cx, "cmd-shift-z");
    r.check(
        "a folder renamed: ⌘Z on the explorer names it back, ⇧⌘Z again",
        renamed == ["Monthly"] && named_back == ["Reports"] && folder_names(ws, cx) == ["Monthly"],
        format!("{renamed:?} {named_back:?} {:?}", folder_names(ws, cx)),
    );

    // A second folder, moved above the first.
    new_folder(cx);
    pause(cx, 30).await;
    typ(cx, "Archive");
    key(cx, "enter");
    if let (Some(from), Some(to)) = (named_point(history, "Archive", cx).await, named_point(history, "Monthly", cx).await) {
        drag(cx, from, point(to.x, to.y - px(8.)), |_| {}).await;
    }
    let moved = folder_names(ws, cx);
    focus(ws, Region::Side, cx);
    key(cx, "cmd-z");
    let order_back = folder_names(ws, cx);
    key(cx, "cmd-shift-z");
    r.check(
        "folders reordered by a drag: ⌘Z puts the order back, ⇧⌘Z moves it again",
        moved == ["Archive", "Monthly"] && order_back == ["Monthly", "Archive"] && folder_names(ws, cx) == ["Archive", "Monthly"],
        format!("{moved:?} {order_back:?} {:?}", folder_names(ws, cx)),
    );

    // Deleted after its "Sure?": the folder and its query go; ⌘Z brings both back, filed.
    if let Some(p) = named_point(history, "Monthly", cx).await {
        click(cx, p, MouseButton::Right, Modifiers::default());
        pause(cx, 30).await;
        menu_click(history, "Delete folder", cx);
        pause(cx, 30).await;
        menu_click(history, "Sure? (deletes 1 query)", cx);
        pause(cx, 30).await;
    }
    let deleted = (folder_names(ws, cx), fav_names(ws, cx));
    let label = next_undo(ws, cx).0;
    focus(ws, Region::Grid, cx);
    key(cx, "ctrl-z");
    let restored = (folder_names(ws, cx), filed(history, "Marker", cx));
    r.check(
        "a folder deleted with its query: ⌃Z on the grid brings both back, the query filed in it",
        deleted == (vec!["Archive".to_string()], vec![]) && label.as_deref() == Some("Delete folder 'Monthly'") && restored == (vec!["Archive".to_string(), "Monthly".to_string()], Some(Some("Monthly".into()))),
        format!("deleted {deleted:?} label {label:?} restored {restored:?} rows {:?}", rows(history, cx)),
    );
    key(cx, "ctrl-shift-z");
    let redone = (folder_names(ws, cx), fav_names(ws, cx));
    key(cx, "ctrl-z");
    r.check(
        "⌃⇧Z deletes them again, ⌃Z brings them back again",
        redone == (vec!["Archive".to_string()], vec![]) && folder_names(ws, cx) == ["Archive", "Monthly"],
        format!("{redone:?} {:?}", folder_names(ws, cx)),
    );

    // An empty folder asks too ("Sure?" alone), and goes.
    let mut asked = false;
    if let Some(p) = named_point(history, "Archive", cx).await {
        click(cx, p, MouseButton::Right, Modifiers::default());
        pause(cx, 30).await;
        menu_click(history, "Delete folder", cx);
        pause(cx, 30).await;
        asked = menu_at(history, "Sure?", false, cx).is_some() && folder_names(ws, cx).contains(&"Archive".to_string());
        menu_click(history, "Sure?", cx);
        pause(cx, 30).await;
    }
    r.check("an empty folder asks \"Sure?\" too, then goes", asked && folder_names(ws, cx) == ["Monthly"], format!("asked {asked} {:?}", folder_names(ws, cx)));
}

/// Right-clicks explorer row `id` and picks `act` in its menu.
async fn choose(tree: &Entity<crate::tree::SchemaTree>, id: &str, act: Act, cx: &mut AsyncWindowContext) {
    for _ in 0..40 {
        draw(cx);
        if let Some(p) = cx.update(|_, cx| tree.read(cx).demo_row_point(id)).ok().flatten() {
            click(cx, p, MouseButton::Right, Modifiers::default());
            pause(cx, 50).await;
            match cx.update(|_, cx| tree.read(cx).demo_menu_item_point(act)).ok().flatten() {
                Some(at) => click(cx, at, MouseButton::Left, Modifiers::default()),
                None => key(cx, "escape"),
            }
            pause(cx, 50).await;
            return;
        }
        pause(cx, 50).await;
    }
}

/// A table pinned, then unpinned, from its menu.
async fn pins(ws: &Entity<Workspace>, tree: &Entity<crate::tree::SchemaTree>, conn: &str, db: &str, r: &mut Report, cx: &mut AsyncWindowContext) {
    let rel = format!("c:{conn}/d:{db}/s:public/r:orders");
    let pinned = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).store.read(cx).core.pins.of(conn).into_iter().map(|p| p.name).collect::<Vec<_>>()).unwrap_or_default();
    choose(tree, &rel, Act::Pin, cx).await;
    let on = pinned(cx);
    focus(ws, Region::Explorer, cx);
    key(cx, "cmd-z");
    let off = pinned(cx);
    key(cx, "cmd-shift-z");
    let again = pinned(cx);
    r.check("a table pinned: ⌘Z on the explorer unpins it, ⇧⌘Z pins it again", on == ["orders"] && off.is_empty() && again == ["orders"], format!("{on:?} {off:?} {again:?}"));
    choose(tree, &rel, Act::Unpin, cx).await;
    let off = pinned(cx);
    focus(ws, Region::Explorer, cx);
    key(cx, "ctrl-z");
    r.check("unpinned: ⌃Z pins it back", off.is_empty() && pinned(cx) == ["orders"], format!("{off:?} {:?}", pinned(cx)));
}

/// The connections reordered by a drag; one deleted with its password, history and pins.
async fn connections(ws: &Entity<Workspace>, tree: &Entity<crate::tree::SchemaTree>, core: &std::sync::Arc<dataman_core::Core>, conn: &str, db: &str, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (mut p2, pw2) = crate::bench::bench_profile(None);
    p2.name = "second".into();
    p2.save_password = true;
    let second = p2.id.clone();
    let _ = cx.update(|_, cx| {
        core.save_connection(p2, Some(&pw2), None).unwrap();
        let store = ws.read(cx).store.clone();
        store.update(cx, |s, cx| s.reload(cx));
        let ids: Vec<String> = store.read(cx).profiles.iter().map(|p| format!("c:{}", p.id)).collect();
        tree.update(cx, |t, cx| ids.iter().for_each(|i| t.collapse_id(i, cx)));
    });
    draw(cx);
    draw(cx);
    let row = |cx: &mut AsyncWindowContext, id: &str| {
        draw(cx);
        cx.update(|_, cx| tree.read(cx).conn_bounds.borrow().get(format!("c:{id}").as_str()).copied()).ok().flatten()
    };
    let before = conn_ids(ws, cx);
    if let (Some(a), Some(b)) = (row(cx, conn), row(cx, &second)) {
        let from = point(b.left() + px(40.), b.center().y);
        let to = point(a.left() + px(40.), a.top() + px(4.));
        input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: from, pressed_button: None, modifiers: Modifiers::default() }));
        input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: from, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }));
        for p in [point(from.x, from.y - px(6.)), point(to.x, to.y + px(6.)), to] {
            input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: Some(MouseButton::Left), modifiers: Modifiers::default() }));
            draw(cx);
        }
        input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: to, modifiers: Modifiers::default(), click_count: 1 }));
        draw(cx);
    }
    let moved = conn_ids(ws, cx);
    focus(ws, Region::Explorer, cx);
    key(cx, "cmd-z");
    let back = conn_ids(ws, cx);
    key(cx, "cmd-shift-z");
    let again = conn_ids(ws, cx);
    r.check(
        "a connection dragged above another: ⌘Z puts the order back (the file too), ⇧⌘Z moves it again",
        before.0 == [conn.to_string(), second.clone()] && moved.0 == [second.clone(), conn.to_string()] && back == before && again == moved,
        format!("before {before:?} moved {moved:?} back {back:?} again {again:?}"),
    );

    // Deleted (its saved password, its history and a pinned table go with it): ⌃Z brings all of
    // it back, at its place.
    let _ = cx.update(|_, _| {
        let entry = dataman_core::HistoryEntry { ts: chrono::Utc::now(), database: db.into(), sql: "SELECT 'second'".into(), duration_ms: 1., rows: Some(1), outcome: dataman_core::Outcome::Ok, error: None, source: None };
        let _ = core.history.append(&second, &entry);
        let _ = core.pins.pin(dataman_core::pins::Pin::new(&second, db, "public", "customer", dataman_core::schema::RelKind::Table));
    });
    let kept = |cx: &mut AsyncWindowContext| -> (bool, Option<String>, usize, usize) {
        cx.update(|_, cx| {
            let s = ws.read(cx).store.read(cx);
            (s.profile(&second).is_some(), s.core.secrets.get(&second).ok().flatten(), s.core.history.load(&second, 10).unwrap_or_default().len(), s.core.pins.of(&second).len())
        })
        .unwrap_or_default()
    };
    let had = kept(cx);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.request_delete_connection(second.clone(), true, window, cx)));
    draw(cx);
    let gone = kept(cx);
    focus(ws, Region::Explorer, cx);
    key(cx, "ctrl-z");
    let restored = kept(cx);
    let order = conn_ids(ws, cx);
    r.check(
        "a connection deleted (password, history, pins gone with it): ⌃Z brings it back at its place, the password in the keychain again",
        had == (true, Some(pw2.clone()), 1, 1) && gone == (false, None, 0, 0) && restored == had && order == moved,
        format!("had {:?} gone {gone:?} restored {:?} order {order:?}", (had.0, had.1.is_some(), had.2, had.3), (restored.0, restored.1.is_some(), restored.2, restored.3)),
    );
    key(cx, "ctrl-shift-z");
    let redone = kept(cx);
    key(cx, "cmd-z");
    r.check("⌃⇧Z deletes it again (password too), ⌘Z restores it again", redone == (false, None, 0, 0) && kept(cx) == had, format!("{:?}", (redone.0, redone.1.is_some())));
}

/// The history moved to the left dock (the palette's move), the layout reset, a seam dragged.
async fn panels(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let layout = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).layout.clone()).unwrap();
    let home = layout(cx);
    let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::dock::MoveHistoryLeft), cx));
    pause(cx, 400).await;
    let moved = layout(cx);
    focus(ws, Region::Grid, cx);
    key(cx, "cmd-z");
    let back = layout(cx);
    key(cx, "cmd-shift-z");
    let again = layout(cx);
    r.check(
        "a panel moved (the palette's move): ⌘Z on the grid puts it back, ⇧⌘Z moves it again",
        moved != home && back == home && again == moved && next_undo(ws, cx).0.as_deref() == Some("Move history"),
        format!("{:?}", next_undo(ws, cx)),
    );
    let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::dock::ResetPanelLayout), cx));
    let reset = layout(cx);
    focus(ws, Region::Grid, cx);
    key(cx, "ctrl-z");
    r.check("Reset panel layout: ⌃Z brings the layout before it back", reset == home && layout(cx) == moved, String::new());
    key(cx, "ctrl-z");
    pause(cx, 400).await;

    // The explorer's seam dragged 40 pt wider.
    let width = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).explorer_w).unwrap();
    let seam = cx.update(|_, cx| ws.read(cx).frame_now.dock(dataman_core::layout::Dock::Left).cols.first().map(|c| c.rect)).ok().flatten();
    let w0 = width(cx);
    if let Some(rect) = seam {
        let from = point(px(rect.right()), px(rect.y + rect.h / 2.));
        let to = point(from.x + px(40.), from.y);
        drag(cx, from, to, |_| {}).await;
    }
    let w1 = width(cx);
    focus(ws, Region::Explorer, cx);
    key(cx, "cmd-z");
    let w2 = width(cx);
    key(cx, "cmd-shift-z");
    r.check(
        "the explorer's seam dragged: ⌘Z puts its width back, ⇧⌘Z widens it again",
        seam.is_some() && (w1 - w0 - 40.).abs() < 1. && w2 == w0 && width(cx) == w1 && next_undo(ws, cx).0.as_deref() == Some("Resize panel"),
        format!("{w0} → {w1} → {w2} → {} {:?}", width(cx), next_undo(ws, cx)),
    );
}

/// A tab closed with ⌘W, an editor group closed (after its "Sure?").
async fn tabs(ws: &Entity<Workspace>, conn: &str, r: &mut Report, cx: &mut AsyncWindowContext) {
    let sql = "SELECT 'reopened' AS tab";
    let state = |cx: &mut AsyncWindowContext| -> (usize, Option<(String, Option<String>)>, bool) {
        cx.update(|_, cx| {
            let w = ws.read(cx);
            let found = w.tabs.iter().map(|t| t.read(cx)).find(|t| t.editor.read(cx).text() == sql).map(|t| (t.editor.read(cx).text().to_string(), t.connection().map(|c| c.profile.id.clone())));
            (w.tabs.len(), found, w.split.is_some())
        })
        .unwrap_or_default()
    };
    let n0 = state(cx).0;
    // New tabs take the demo's connection (the explorer's clicks above may have picked another).
    let _ = cx.update(|_, cx| ws.update(cx, |w, _| w.current_conn = Some(conn.to_string())));
    key(cx, "cmd-t");
    let _ = cx.update(|_, cx| {
        let editor = ws.read(cx).active_tab().unwrap().read(cx).editor.clone();
        editor.update(cx, |e, cx| e.set_text(sql, cx));
    });
    key(cx, "cmd-w");
    let closed = state(cx);
    focus(ws, Region::Explorer, cx);
    key(cx, "cmd-z");
    let reopened = state(cx);
    r.check(
        "a tab closed with ⌘W: ⌘Z on the explorer reopens it with its script, on its connection",
        closed.0 == n0 && closed.1.is_none() && reopened.0 == n0 + 1 && reopened.1 == Some((sql.to_string(), Some(conn.to_string()))),
        format!("closed {closed:?} reopened {reopened:?}"),
    );
    focus(ws, Region::Explorer, cx);
    key(cx, "cmd-shift-z");
    let reclosed = state(cx);
    key(cx, "cmd-z");
    r.check("⇧⌘Z closes it again, ⌘Z reopens it again", reclosed.0 == n0 && reclosed.1.is_none() && state(cx).1.is_some(), format!("{reclosed:?} {:?}", state(cx)));

    // To the side (⌘\), then that group closed: its × asks, again it closes; ⌘Z brings the group
    // back with its tab.
    let _ = cx.update(|window, cx| {
        let ix = ws.read(cx).tabs.iter().position(|t| t.read(cx).editor.read(cx).text() == sql).unwrap_or(0);
        ws.update(cx, |w, cx| w.activate(ix, window, cx));
    });
    key(cx, "cmd-\\");
    pause(cx, 400).await;
    let split = state(cx).2;
    for _ in 0..2 {
        let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::split::CloseGroup), cx));
        draw(cx);
    }
    pause(cx, 400).await;
    let folded = state(cx);
    let label = next_undo(ws, cx).0;
    // (Its tab in front has no result: the grid is not on screen.)
    focus(ws, Region::Explorer, cx);
    key(cx, "cmd-z");
    pause(cx, 400).await;
    let back = cx
        .update(|_, cx| {
            let w = ws.read(cx);
            w.split.as_ref().map(|s| s.right.iter().filter_map(|id| w.tabs.iter().find(|t| t.entity_id() == *id)).map(|t| t.read(cx).editor.read(cx).text().to_string()).collect::<Vec<_>>())
        })
        .ok()
        .flatten();
    r.check(
        "an editor group closed (after its \"Sure?\"): ⌘Z on the explorer opens the split again, its tab in it",
        split && !folded.2 && label.as_deref() == Some("Close editor group") && back == Some(vec![sql.to_string()]),
        format!("split {split} folded {folded:?} label {label:?} back {back:?}"),
    );
}

/// In the SQL editor and in a text field the keys are the text's; Edit ▸ Undo follows the focus.
async fn text(ws: &Entity<Workspace>, tree: &Entity<crate::tree::SchemaTree>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let steps = next_undo(ws, cx);
    let editor = cx.update(|_, cx| ws.read(cx).active_tab().unwrap().read(cx).editor.clone()).unwrap();
    let _ = cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            e.set_text("SELECT 1", cx);
            e.set_cursor(8, cx);
        });
        window.focus(&editor.focus_handle(cx), cx);
    });
    typ(cx, "23");
    let typed = cx.update(|_, cx| editor.read(cx).text().to_string()).unwrap();
    key(cx, "ctrl-z");
    let ctrl = (cx.update(|_, cx| editor.read(cx).text().to_string()).unwrap(), next_undo(ws, cx));
    key(cx, "cmd-z");
    let cmd = (cx.update(|_, cx| editor.read(cx).text().to_string()).unwrap(), next_undo(ws, cx));
    r.check(
        "in the SQL editor ⌘Z undoes its typing and ⌃Z does nothing; the app's steps are untouched",
        typed == "SELECT 123" && ctrl == ("SELECT 123".to_string(), steps.clone()) && cmd == ("SELECT 1".to_string(), steps.clone()),
        format!("typed {typed:?} ctrl {ctrl:?} cmd {cmd:?} steps {steps:?}"),
    );
    // Edit ▸ Undo in the editor: the text's too.
    typ(cx, "4");
    let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::editor::Undo), cx));
    let menu = (cx.update(|_, cx| editor.read(cx).text().to_string()).unwrap(), next_undo(ws, cx));
    r.check("Edit ▸ Undo in the editor is its text's", menu == ("SELECT 1".to_string(), steps.clone()), format!("{menu:?}"));

    // The explorer's filter: a text field.
    let field = cx.update(|_, cx| tree.read(cx).demo_filter()).unwrap();
    let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.demo_focus_filter(window, cx)));
    draw(cx);
    typ(cx, "ord");
    let typed = cx.update(|_, cx| field.read(cx).text().to_string()).unwrap();
    key(cx, "ctrl-z");
    let ctrl = (cx.update(|_, cx| field.read(cx).text().to_string()).unwrap(), next_undo(ws, cx));
    key(cx, "cmd-z");
    let cmd = (cx.update(|_, cx| field.read(cx).text().to_string()).unwrap(), next_undo(ws, cx));
    typ(cx, "x");
    let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::editor::Undo), cx));
    let menu = (cx.update(|_, cx| field.read(cx).text().to_string()).unwrap(), next_undo(ws, cx));
    r.check(
        "in a text field (the explorer's filter) ⌘Z and Edit ▸ Undo undo its typing, ⌃Z does nothing; the app's steps are untouched",
        typed == "ord" && ctrl == ("ord".to_string(), steps.clone()) && cmd == (String::new(), steps.clone()) && menu == (String::new(), steps.clone()),
        format!("typed {typed:?} ctrl {ctrl:?} cmd {cmd:?} menu {menu:?}"),
    );
    key(cx, "escape");

    // Edit ▸ Undo / Redo outside text: the app's.
    focus(ws, Region::Explorer, cx);
    let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::editor::Undo), cx));
    let undone = next_undo(ws, cx);
    let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::editor::Redo), cx));
    r.check(
        "Edit ▸ Undo on the explorer is the app's undo, Edit ▸ Redo its redo",
        undone.1 == steps.0 && next_undo(ws, cx) == steps,
        format!("steps {steps:?} after undo {undone:?} after redo {:?}", next_undo(ws, cx)),
    );
}
