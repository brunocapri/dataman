//! `history-check` (`--demo`): a favorite's context menu (right-click, ⌃-click) with Rename
//! (inline, ↩ confirms, esc cancels, an empty name reverts) and Remove, the status dropdown
//! (All / OK / Error / Cancelled) and the separate favorites toggle beside it. Driven with
//! real mouse and key events through the window. PASS/FAIL lines like `find-check`.

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

    println!("{}", if ok { "ALL PASS" } else { "SOME CHECKS FAILED" });
    ok
}
