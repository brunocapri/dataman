//! `history-check` (`--demo`): a favorite's context menu (right-click, ⌃-click) with Rename
//! (inline, ↩ confirms, esc cancels, an empty name reverts) and Remove, the status dropdown
//! (All / OK / Error / Cancelled) and the separate favorites toggle beside it; ⌘S (a new
//! favorite, then saving into it, the toolbar's feedback and its click), ⇧⌘S (Save As…, the
//! panel's answer injected), and ⌘K among ~500 favorites (scrolling, finding one by name and by
//! SQL, ↩ showing it in the favorites). Driven with real mouse and key events through the
//! window. PASS/FAIL lines like `find-check`.

use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels, Point, point, px};

use crate::app::{Side, Workspace};
use crate::history::HistoryPanel;

pub fn is_stage(s: &str) -> bool {
    s == "history-check"
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

/// A move first: GPUI's hit-test for the down/up pair follows the latest hovered element.
fn click(cx: &mut AsyncWindowContext, p: Point<Pixels>, button: MouseButton, modifiers: Modifiers) {
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: None, modifiers }));
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button, position: p, modifiers, click_count: 1, first_mouse: false }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button, position: p, modifiers, click_count: 1 }));
}

fn key(cx: &mut AsyncWindowContext, k: &str) {
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(Keystroke::parse(k).unwrap(), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn typ(cx: &mut AsyncWindowContext, s: &str) {
    for c in s.chars() {
        key(cx, &c.to_string());
    }
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

/// A point inside the favorite row at `item_ix` (row 0, once the dropdown shows only
/// favorites): drawn fresh first, retried briefly (the list measures items as it repaints
/// after a change, not necessarily in the very next frame).
async fn row_point(history: &Entity<HistoryPanel>, item_ix: usize, cx: &mut AsyncWindowContext) -> Option<Point<Pixels>> {
    for _ in 0..20 {
        draw(cx);
        if let Some(p) = cx.update(|_, cx| history.read(cx).demo_item_point(item_ix)).ok().flatten() {
            return Some(p);
        }
        pause(cx, 20).await;
    }
    None
}

/// The favorite's context menu opens right under the click: Rename is its first row, Remove
/// its second (`ui::menu_item` is 26 pt, the popover has 4 pt of padding).
fn rename_at(p: Point<Pixels>) -> Point<Pixels> {
    point(p.x + px(20.), p.y + px(4. + 13.))
}
fn remove_at(p: Point<Pixels>) -> Point<Pixels> {
    point(p.x + px(20.), p.y + px(4. + 26. + 13.))
}

pub async fn run(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };

    let history = cx.update(|_, cx| ws.read(cx).history.clone()).unwrap();
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.toggle_history_for_demo(cx)));
    for _ in 0..100 {
        if cx.update(|_, cx| history.read(cx).demo_entry_count() > 0).unwrap_or(false) {
            break;
        }
        pause(cx, 50).await;
    }
    report("the default query already left a history entry", cx.update(|_, cx| history.read(cx).demo_entry_count() > 0).unwrap_or(false), String::new());
    // The panel slides in (180-240 ms): let it land before anything is clicked in it, or its
    // bounds (read a moment before a click reaches it) would be stale mid-animation.
    pause(cx, 400).await;
    // Favoriting the entry is setup, not what this stage checks (the star toggle shipped
    // earlier): done directly, through the window for a repaint.
    let _ = cx.update(|_, cx| history.update(cx, |h: &mut HistoryPanel, cx| h.demo_favorite_entry(0, cx)));
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });

    // The status dropdown: opens, lists only All/OK/Error/Cancelled (never Favorites).
    let Some(dd) = cx.update(|_, cx| history.read(cx).demo_dropdown_bounds()).unwrap() else {
        println!("FAIL the status dropdown has no bounds yet");
        return false;
    };
    let dd_click = point(dd.origin.x + px(10.), dd.origin.y + dd.size.height * 0.5);
    click(cx, dd_click, MouseButton::Left, Modifiers::default());
    pause(cx, 30).await;
    let opened = cx.update(|_, cx| history.read(cx).demo_dropdown_open()).unwrap_or(false);
    report("clicking the status dropdown opens it", opened, String::new());
    // A second click on the same button shuts it (its own toggle).
    click(cx, dd_click, MouseButton::Left, Modifiers::default());
    pause(cx, 30).await;

    // Favorites is a separate toggle beside it, never a row of that dropdown.
    let Some(fb) = cx.update(|_, cx| history.read(cx).demo_favorites_toggle_bounds()).unwrap() else {
        println!("FAIL the favorites toggle has no bounds yet");
        return false;
    };
    let fb_click = point(fb.origin.x + fb.size.width * 0.5, fb.origin.y + fb.size.height * 0.5);
    click(cx, fb_click, MouseButton::Left, Modifiers::default());
    pause(cx, 30).await;
    let (showing, dropdown_untouched, count) =
        cx.update(|_, cx| (history.read(cx).demo_showing_favorites(), !history.read(cx).demo_dropdown_open(), history.read(cx).demo_favorites().len())).unwrap();
    report(
        "the favorites toggle filters to it, beside (not inside) the status dropdown",
        showing && dropdown_untouched && count == 1,
        format!("showing={showing} dropdown_untouched={dropdown_untouched} count={count}"),
    );

    // ⌃-click the favorite: its context menu opens; Rename starts inline editing; ↩ commits.
    let Some(row) = row_point(&history, 0, cx).await else {
        println!("FAIL the favorite row has no bounds");
        return false;
    };
    click(cx, row, MouseButton::Left, Modifiers { control: true, ..Default::default() });
    pause(cx, 30).await;
    let menu_open = cx.update(|_, cx| history.read(cx).demo_fav_menu_open()).unwrap_or(false);
    report("⌃-click opens the favorite's context menu", menu_open, String::new());
    click(cx, rename_at(row), MouseButton::Left, Modifiers::default());
    pause(cx, 30).await;
    let renaming = cx.update(|_, cx| history.read(cx).demo_renaming_index()).unwrap();
    report("Rename starts inline editing", renaming == Some(0), format!("{renaming:?}"));
    // The field itself holds the keyboard focus (not the row, not the panel): otherwise
    // typing lands nowhere, which `typ`'s keystrokes alone would not catch.
    let focused = cx.update(|window, cx| history.read(cx).demo_renaming_focused(window, cx)).unwrap();
    report("the rename field holds the keyboard focus", focused, String::new());
    // Clicking into the field (what a person does before typing) keeps the focus there: the
    // row under it must not take the click back to the panel.
    draw(cx);
    match cx.update(|_, cx| history.read(cx).demo_rename_field_point(cx)).unwrap() {
        Some(p) => {
            click(cx, p, MouseButton::Left, Modifiers::default());
            pause(cx, 30).await;
            let focused = cx.update(|window, cx| history.read(cx).demo_renaming_focused(window, cx)).unwrap();
            let renaming = cx.update(|_, cx| history.read(cx).demo_renaming_index()).unwrap();
            report("a click inside the rename field keeps it focused", focused && renaming == Some(0), format!("focused={focused} renaming={renaming:?}"));
        }
        None => report("a click inside the rename field keeps it focused", false, "the field has no bounds".into()),
    }
    key(cx, "cmd-a");
    typ(cx, "Renamed fav");
    key(cx, "enter");
    let name = cx.update(|_, cx| history.read(cx).demo_favorites()[0].1.clone()).unwrap();
    report("↩ commits the new name", name == "Renamed fav", name);

    // Opening a renamed favorite names the new tab after it.
    if let Some(row) = row_point(&history, 0, cx).await {
        click(cx, row, MouseButton::Left, Modifiers::default());
        pause(cx, 30).await;
        key(cx, "alt-enter");
        pause(cx, 50).await;
        draw(cx);
        let title = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.read(cx).title(cx).to_string())).unwrap();
        report("a renamed favorite opens in a tab titled with its name", title.as_deref() == Some("Renamed fav"), format!("{title:?}"));
    }

    // esc cancels: a half-typed rename never reaches the store, and the panel stays open.
    let Some(row) = row_point(&history, 0, cx).await else {
        println!("FAIL the favorite row has no bounds");
        return false;
    };
    click(cx, row, MouseButton::Right, Modifiers::default());
    pause(cx, 30).await;
    click(cx, rename_at(row), MouseButton::Left, Modifiers::default());
    pause(cx, 30).await;
    key(cx, "cmd-a");
    typ(cx, "whoops");
    key(cx, "escape");
    let (name, side_open, renaming) =
        cx.update(|_, cx| (history.read(cx).demo_favorites()[0].1.clone(), matches!(ws.read(cx).side, Some(Side::History)), history.read(cx).demo_renaming_index())).unwrap();
    report("esc cancels the rename, keeps the old name, and the panel stays open", name == "Renamed fav" && side_open && renaming.is_none(), format!("{name:?} side_open={side_open} renaming={renaming:?}"));

    // An empty name reverts to the old one.
    let Some(row) = row_point(&history, 0, cx).await else {
        println!("FAIL the favorite row has no bounds");
        return false;
    };
    click(cx, row, MouseButton::Right, Modifiers::default());
    pause(cx, 30).await;
    click(cx, rename_at(row), MouseButton::Left, Modifiers::default());
    pause(cx, 30).await;
    key(cx, "cmd-a");
    key(cx, "backspace");
    key(cx, "enter");
    let name = cx.update(|_, cx| history.read(cx).demo_favorites()[0].1.clone()).unwrap();
    report("an empty name reverts to the old one", name == "Renamed fav", name);

    // Right-click: Remove.
    let Some(row) = row_point(&history, 0, cx).await else {
        println!("FAIL the favorite row has no bounds");
        return false;
    };
    click(cx, row, MouseButton::Right, Modifiers::default());
    pause(cx, 30).await;
    let menu_open = cx.update(|_, cx| history.read(cx).demo_fav_menu_open()).unwrap_or(false);
    report("right-click opens the favorite's context menu", menu_open, String::new());
    click(cx, remove_at(row), MouseButton::Left, Modifiers::default());
    pause(cx, 30).await;
    let count = cx.update(|_, cx| history.read(cx).demo_favorites().len()).unwrap();
    report("Remove deletes the favorite", count == 0, format!("{count}"));

    // Picking a status filter while favorites are on switches favorites off. The menu opens
    // 26 pt under the button; OK is its second row (4 pt padding, 26 pt rows).
    let fav_on = cx.update(|_, cx| history.read(cx).demo_showing_favorites()).unwrap();
    click(cx, dd_click, MouseButton::Left, Modifiers::default());
    pause(cx, 30).await;
    draw(cx);
    click(cx, point(dd.origin.x + px(20.), dd.origin.y + px(26. + 4. + 26. + 13.)), MouseButton::Left, Modifiers::default());
    pause(cx, 30).await;
    let fav_off = !cx.update(|_, cx| history.read(cx).demo_showing_favorites()).unwrap();
    let closed = !cx.update(|_, cx| history.read(cx).demo_dropdown_open()).unwrap_or(true);
    report("a status filter turns favorites off", fav_on && fav_off && closed, format!("before={fav_on} off={fav_off} closed={closed}"));

    // Import Favorites… (the star toggle's right-click menu, its only row): several .sql files
    // at once (the picker's paths injected), each a favorite named after its file; one with
    // SQL already saved is skipped.
    let dir = std::env::temp_dir().join(format!("diskman-fav-import-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let files = [("top customers.sql", "SELECT * FROM customer ORDER BY id LIMIT 10;"), ("orders.sql", "SELECT count(*) FROM orders;"), ("again.SQL", "SELECT * FROM customer ORDER BY id LIMIT 10;")];
    let paths: Vec<std::path::PathBuf> = files.iter().map(|(n, sql)| {
        let p = dir.join(n);
        let _ = std::fs::write(&p, sql);
        p
    }).collect();
    let before = cx.update(|_, cx| history.read(cx).demo_favorites().len()).unwrap();
    if let Some(fb) = cx.update(|_, cx| history.read(cx).demo_favorites_toggle_bounds()).unwrap() {
        let at = point(fb.origin.x + fb.size.width / 2., fb.origin.y + fb.size.height / 2.);
        click(cx, at, MouseButton::Right, Modifiers::default());
        pause(cx, 30).await;
        draw(cx);
        crate::sql_picker::set_test_paths(paths.clone());
        click(cx, rename_at(at), MouseButton::Left, Modifiers::default());
        pause(cx, 200).await;
        draw(cx);
    }
    let favs = cx.update(|_, cx| history.read(cx).demo_favorites()).unwrap();
    let names: Vec<&str> = favs.iter().map(|f| f.1.as_str()).collect();
    report(
        "Import Favorites… adds each .sql file, named after it, and skips one already saved",
        favs.len() == before + 2 && names.contains(&"top customers") && names.contains(&"orders"),
        format!("{before} → {names:?}"),
    );
    let _ = std::fs::remove_dir_all(&dir);

    ok &= save_checks(ws, &history, cx).await;
    ok &= palette_checks(ws, &history, cx).await;
    println!("{}", if ok { "ALL PASS" } else { "SOME CHECKS FAILED" });
    ok
}

fn report(ok: &mut bool, name: &str, pass: bool, detail: String) {
    println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
    *ok &= pass;
}

/// A new tab holding `sql`, the editor focused.
fn tab_with(ws: &Entity<Workspace>, sql: &str, cx: &mut AsyncWindowContext) -> Entity<crate::tab::QueryTab> {
    let tab = cx.update(|window, cx| ws.update(cx, |w, cx| w.new_tab(window, cx))).unwrap();
    let _ = cx.update(|window, cx| {
        let editor = tab.read(cx).editor.clone();
        editor.update(cx, |e, cx| e.set_text(sql, cx));
        window.focus(&gpui::Focusable::focus_handle(&editor, cx), cx);
    });
    draw(cx);
    tab
}

fn favorite(ws: &Entity<Workspace>, id: &str, cx: &mut AsyncWindowContext) -> Option<diskman_core::Favorite> {
    cx.update(|_, cx| ws.read(cx).store.read(cx).core.favorites.list().into_iter().find(|f| f.id == id)).ok().flatten()
}

fn linked(tab: &Entity<crate::tab::QueryTab>, cx: &mut AsyncWindowContext) -> (Option<String>, String) {
    cx.update(|_, cx| (tab.read(cx).favorite.as_ref().map(|(id, _)| id.clone()), tab.read(cx).title(cx).to_string())).unwrap()
}

/// ⌘S in a tab without a favorite saves its script as a new one (named after its first line,
/// made unique) and links the tab; "Saved as favorite →" shows by the History button and its
/// click shows the favorite; ⌘S in a linked tab saves into the favorite ("Saved"); ⇧⌘S writes
/// the script to the file the save panel answers (injected).
async fn save_checks(ws: &Entity<Workspace>, history: &Entity<HistoryPanel>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let flash = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).saved_flash.as_ref().map(|f| (f.id.clone(), f.label))).unwrap();

    let a = tab_with(ws, "-- the answer\nSELECT 42 AS answer;", cx);
    key(cx, "cmd-s");
    pause(cx, 30).await;
    draw(cx);
    let (id_a, title_a) = linked(&a, cx);
    let fav_a = id_a.as_deref().and_then(|id| favorite(ws, id, cx));
    let shown = flash(cx);
    report(
        &mut ok,
        "⌘S in a tab without a favorite saves its script as one, named after its first line; the tab takes its name",
        fav_a.as_ref().is_some_and(|f| f.name == "SELECT 42 AS answer" && f.sql == "-- the answer\nSELECT 42 AS answer;") && title_a == "SELECT 42 AS answer",
        format!("{fav_a:?} title {title_a:?}"),
    );
    report(&mut ok, "\"Saved as favorite\" shows by the History button", shown.as_ref().is_some_and(|(id, l)| Some(id) == id_a.as_ref() && *l == "Saved as favorite"), format!("{shown:?}"));

    // Another tab, the same first line: the name is made unique.
    let b = tab_with(ws, "SELECT 42 AS answer;\nSELECT 1;", cx);
    key(cx, "cmd-s");
    pause(cx, 30).await;
    draw(cx);
    let (id_b, title_b) = linked(&b, cx);
    let fav_b = id_b.as_deref().and_then(|id| favorite(ws, id, cx));
    report(&mut ok, "a name already taken gets a number", fav_b.as_ref().is_some_and(|f| f.name == "SELECT 42 AS answer 2") && title_b == "SELECT 42 AS answer 2" && id_b != id_a, format!("{fav_b:?} {title_b:?}"));

    // Its click: the favorites, that favorite selected and focused.
    let at = cx.update(|_, cx| ws.read(cx).saved_bounds.get()).unwrap();
    match at {
        Some(b) => {
            click(cx, point(b.left() + b.size.width / 2., b.top() + b.size.height / 2.), MouseButton::Left, Modifiers::default());
            pause(cx, 400).await;
            draw(cx);
            draw(cx);
            let (side, favs, sel, focus, gone) = cx
                .update(|window, cx| {
                    let h = history.read(cx);
                    (matches!(ws.read(cx).side, Some(Side::History)), h.demo_showing_favorites(), h.demo_selected_favorite(), h.demo_has_focus(window), ws.read(cx).saved_flash.is_none())
                })
                .unwrap();
            report(
                &mut ok,
                "a click on it shows the favorite in the favorites, selected and focused",
                side && favs && sel.as_ref().is_some_and(|(id, _, inside)| Some(id) == id_b.as_ref() && *inside) && focus && gone,
                format!("side {side} favorites {favs} {sel:?} focus {focus} gone {gone}"),
            );
        }
        None => report(&mut ok, "a click on it shows the favorite in the favorites, selected and focused", false, "no bounds".into()),
    }

    // ⌘S in a linked tab saves into its favorite: the name stays.
    let _ = cx.update(|window, cx| {
        let editor = b.read(cx).editor.clone();
        editor.update(cx, |e, cx| e.set_text("SELECT 43 AS answer;", cx));
        window.focus(&gpui::Focusable::focus_handle(&editor, cx), cx);
    });
    draw(cx);
    key(cx, "cmd-s");
    pause(cx, 30).await;
    let after = id_b.as_deref().and_then(|id| favorite(ws, id, cx));
    let (still, title) = linked(&b, cx);
    let shown = flash(cx);
    report(
        &mut ok,
        "⌘S in a favorite's tab saves its script into it, keeping its name (\"Saved\")",
        after.as_ref().is_some_and(|f| f.sql == "SELECT 43 AS answer;" && f.name == "SELECT 42 AS answer 2") && still == id_b && title == "SELECT 42 AS answer 2" && shown.is_some_and(|(_, l)| l == "Saved"),
        format!("{after:?} {title:?}"),
    );

    // Renamed in the panel: the tab follows.
    if let Some(id) = id_b.clone() {
        let ix = cx.update(|_, cx| history.read(cx).demo_favorites().iter().position(|f| f.0 == id)).unwrap();
        if let Some(ix) = ix {
            let _ = cx.update(|window, cx| history.update(cx, |h, cx| h.demo_rename(ix, "Answers", window, cx)));
            draw(cx);
            let (_, title) = linked(&b, cx);
            report(&mut ok, "a favorite renamed in the panel renames its tab", title == "Answers", title);
        }
    }

    // ⇧⌘S: Save As… (the panel's answer injected).
    let dir = std::env::temp_dir().join(format!("diskman-save-as-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("report.sql");
    let c = tab_with(ws, "SELECT 'saved' AS file;\n", cx);
    crate::sql_picker::set_test_save_path(path.clone());
    key(cx, "cmd-shift-s");
    pause(cx, 200).await;
    draw(cx);
    let written = std::fs::read_to_string(&path).ok();
    let (_, title) = linked(&c, cx);
    report(&mut ok, "⇧⌘S writes the script to the .sql file picked, and names the tab after it", written.as_deref() == Some("SELECT 'saved' AS file;\n") && title == "report.sql", format!("{written:?} {title:?}"));
    let _ = std::fs::remove_dir_all(&dir);
    ok
}

/// ⌘K finds favorites among ~500 by name and by SQL; the Favorites scope lists them and
/// scrolls with the keys and the wheel; ↩ slides the favorites in with the one picked
/// selected, scrolled into view and focused, even near the bottom of the list.
async fn palette_checks(ws: &Entity<Workspace>, history: &Entity<HistoryPanel>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let words = ["orders", "customers", "revenue", "churn", "latency"];
    let ids = cx
        .update(|_, cx| {
            let core = ws.read(cx).store.read(cx).core.clone();
            (0..500).filter_map(|i| core.favorites.add(diskman_core::Favorite::new(format!("Report {i:03} {}", words[i % 5]), format!("SELECT {i} AS n, 'marker_{i:03}' AS tag FROM orders"))).ok().map(|f| f.id)).collect::<Vec<_>>()
        })
        .unwrap();
    report(&mut ok, "500 favorites saved", ids.len() == 500, format!("{}", ids.len()));
    // The history shut, so the pick slides it in.
    if cx.update(|_, cx| matches!(ws.read(cx).side, Some(Side::History))).unwrap() {
        let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.toggle_history_for_demo(cx)));
    }
    pause(cx, 400).await;
    draw(cx);
    key(cx, "cmd-k");
    pause(cx, 30).await;
    let Some(palette) = cx.update(|_, cx| ws.read(cx).palette.clone()).unwrap() else {
        report(&mut ok, "⌘K opens the palette", false, String::new());
        return ok;
    };
    let state = |cx: &mut AsyncWindowContext| cx.update(|_, cx| palette.read(cx).demo_state()).unwrap();
    let _ = cx.update(|_, cx| palette.update(cx, |p, cx| p.set_query("*", cx)));
    draw(cx);
    for _ in 0..40 {
        key(cx, "down");
    }
    draw(cx);
    let (_, sel, count, offset, inside, list) = state(cx);
    report(&mut ok, "the Favorites scope lists them and ↓ scrolls the selection along", sel == 40 && count == 80 && offset > 100. && inside, format!("sel {sel} of {count}, offset {offset}, inside {inside}"));
    let at = point(list.left() + list.size.width / 2., list.top() + list.size.height / 2.);
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: at, pressed_button: None, modifiers: Modifiers::default() }));
    input(cx, PlatformInput::ScrollWheel(gpui::ScrollWheelEvent { position: at, delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-300.))), modifiers: Modifiers::default(), touch_phase: gpui::TouchPhase::Moved }));
    draw(cx);
    let (_, _, _, wheeled, _, _) = state(cx);
    report(&mut ok, "the wheel scrolls the list", wheeled > offset + 100., format!("{offset} → {wheeled}"));

    // By name (fuzzy) and by a piece of the SQL.
    let _ = cx.update(|_, cx| palette.update(cx, |p, cx| p.set_query("marker_123", cx)));
    let (label, ..) = state(cx);
    report(&mut ok, "a piece of a favorite's SQL finds it", label == "Report 123 churn", label);
    let _ = cx.update(|_, cx| palette.update(cx, |p, cx| p.set_query("report 497 rev", cx)));
    draw(cx);
    let (label, ..) = state(cx);
    report(&mut ok, "its name finds it (fuzzy), among the other kinds", label == "Report 497 revenue", label);
    key(cx, "enter");
    draw(cx);
    let (closed, opening) = cx.update(|_, cx| (ws.read(cx).palette.is_none(), ws.read(cx).demo_side_sliding() || cx.reduce_motion())).unwrap();
    pause(cx, 400).await;
    for _ in 0..3 {
        draw(cx);
        pause(cx, 20).await;
    }
    let (side, favs, sel, focus) = cx
        .update(|window, cx| {
            let h = history.read(cx);
            (matches!(ws.read(cx).side, Some(Side::History)), h.demo_showing_favorites(), h.demo_selected_favorite(), h.demo_has_focus(window))
        })
        .unwrap();
    let want = ids.get(497).cloned();
    report(
        &mut ok,
        "↩ slides the favorites in, the one picked selected, scrolled into view (near the bottom) and focused",
        closed && opening && side && favs && focus && sel.as_ref().is_some_and(|(id, ix, inside)| Some(id) == want.as_ref() && *ix > 400 && *inside),
        format!("closed {closed} opening {opening} side {side} favorites {favs} focus {focus} {sel:?}"),
    );
    // ↓ moves on from it.
    key(cx, "down");
    draw(cx);
    let next = cx.update(|_, cx| history.read(cx).demo_selected_favorite()).unwrap();
    report(&mut ok, "↓ in the list moves on from it", next.as_ref().is_some_and(|(id, _, inside)| Some(id) == ids.get(498) && *inside), format!("{next:?}"));
    ok
}
