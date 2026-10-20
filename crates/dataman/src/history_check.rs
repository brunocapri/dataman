use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels, Point, point, px};

use crate::app::Workspace;
use dataman_core::layout::NavView;
use crate::history::HistoryPanel;

pub fn is_stage(s: &str) -> bool {
    s == "history-check" || s == "folders-check"
}

pub(crate) fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

pub(crate) fn input(cx: &mut AsyncWindowContext, e: PlatformInput) {
    let _ = cx.update(|window, cx| {
        window.dispatch_event(e, cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

/// A move first: GPUI's hit-test for the down/up pair follows the latest hovered element.
pub(crate) fn click(cx: &mut AsyncWindowContext, p: Point<Pixels>, button: MouseButton, modifiers: Modifiers) {
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: None, modifiers }));
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button, position: p, modifiers, click_count: 1, first_mouse: false }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button, position: p, modifiers, click_count: 1 }));
}

pub(crate) fn key(cx: &mut AsyncWindowContext, k: &str) {
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(crate::keymap::keystroke(k), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

pub(crate) fn typ(cx: &mut AsyncWindowContext, s: &str) {
    for c in s.chars() {
        key(cx, &c.to_string());
    }
}

pub(crate) async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

/// Retried briefly: the list measures items as it repaints after a change, not
/// necessarily in the very next frame.
pub(crate) async fn row_point(history: &Entity<HistoryPanel>, item_ix: usize, cx: &mut AsyncWindowContext) -> Option<Point<Pixels>> {
    for _ in 0..20 {
        draw(cx);
        if let Some(p) = cx.update(|_, cx| history.read(cx).demo_item_point(item_ix)).ok().flatten() {
            return Some(p);
        }
        pause(cx, 20).await;
    }
    None
}

pub(crate) fn menu_at(history: &Entity<HistoryPanel>, label: &str, sub: bool, cx: &mut AsyncWindowContext) -> Option<Point<Pixels>> {
    draw(cx);
    cx.update(|_, cx| history.read(cx).demo_menu_point(label, sub)).ok().flatten()
}

pub(crate) fn menu_click(history: &Entity<HistoryPanel>, label: &str, cx: &mut AsyncWindowContext) -> bool {
    match menu_at(history, label, false, cx) {
        Some(p) => {
            click(cx, p, MouseButton::Left, Modifiers::default());
            true
        }
        None => false,
    }
}

fn rail_click(ws: &Entity<Workspace>, v: NavView, cx: &mut AsyncWindowContext) {
    draw(cx);
    if let Some(b) = cx.update(|_, cx| ws.read(cx).rail_bounds.borrow().get(&v).copied()).ok().flatten() {
        click(cx, b.center(), MouseButton::Left, Modifiers::default());
    }
    draw(cx);
}

pub async fn run(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    if std::env::var("DATAMAN_SHOT_STAGE").is_ok_and(|s| s.trim_end_matches("-light") == "folders-check") {
        return folders(ws, cx).await;
    }
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };

    let history = cx.update(|_, cx| ws.read(cx).history.clone()).unwrap();
    draw(cx);
    rail_click(ws, NavView::History, cx);
    for _ in 0..100 {
        if cx.update(|_, cx| history.read(cx).demo_entry_count() > 0).unwrap_or(false) {
            break;
        }
        pause(cx, 50).await;
    }
    report("the default query already left a history entry", cx.update(|_, cx| history.read(cx).demo_entry_count() > 0).unwrap_or(false), String::new());
    pause(cx, 400).await;
    let _ = cx.update(|_, cx| history.update(cx, |h: &mut HistoryPanel, cx| h.demo_favorite_entry(0, cx)));
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });

    let Some(dd) = cx.update(|_, cx| history.read(cx).demo_dropdown_bounds()).unwrap() else {
        println!("FAIL the status dropdown has no bounds yet");
        return false;
    };
    let dd_click = point(dd.origin.x + px(10.), dd.origin.y + dd.size.height * 0.5);
    click(cx, dd_click, MouseButton::Left, Modifiers::default());
    pause(cx, 30).await;
    let opened = cx.update(|_, cx| history.read(cx).demo_dropdown_open()).unwrap_or(false);
    report("clicking the status dropdown opens it", opened, String::new());
    click(cx, dd_click, MouseButton::Left, Modifiers::default());
    pause(cx, 30).await;

    let _ = cx.update(|window, cx| history.update(cx, |h, cx| h.demo_search("select", window, cx)));
    rail_click(ws, NavView::Favorites, cx);
    pause(cx, 30).await;
    let (fav_view, fav_search) = cx.update(|_, cx| (history.read(cx).demo_showing_favorites(), history.read(cx).demo_filter_text(cx))).unwrap();
    rail_click(ws, NavView::History, cx);
    pause(cx, 30).await;
    let (hist_view, hist_search) = cx.update(|_, cx| (!history.read(cx).demo_showing_favorites(), history.read(cx).demo_filter_text(cx))).unwrap();
    report(
        "the rail switches between History and Favorites, each with its own search",
        fav_view && fav_search.is_empty() && hist_view && hist_search == "select",
        format!("favorites {fav_view} {fav_search:?}, history {hist_view} {hist_search:?}"),
    );
    let _ = cx.update(|window, cx| history.update(cx, |h, cx| h.demo_search("", window, cx)));
    rail_click(ws, NavView::Favorites, cx);
    pause(cx, 30).await;
    let (showing, dropdown_untouched, count) =
        cx.update(|_, cx| (history.read(cx).demo_showing_favorites(), !history.read(cx).demo_dropdown_open(), history.read(cx).demo_favorites().len())).unwrap();
    report(
        "the favorites listed, the status dropdown untouched",
        showing && dropdown_untouched && count == 1,
        format!("showing={showing} dropdown_untouched={dropdown_untouched} count={count}"),
    );

    let Some(row) = row_point(&history, 0, cx).await else {
        println!("FAIL the favorite row has no bounds");
        return false;
    };
    if crate::keymap::LINUX {
        click(cx, row, MouseButton::Right, Modifiers::default());
    } else {
        click(cx, row, MouseButton::Left, Modifiers { control: true, ..Default::default() });
    }
    pause(cx, 30).await;
    let menu_open = cx.update(|_, cx| history.read(cx).demo_fav_menu_open()).unwrap_or(false);
    report(if crate::keymap::LINUX { "a right click opens the favorite's context menu" } else { "⌃-click opens the favorite's context menu" }, menu_open, String::new());
    menu_click(&history, "Rename", cx);
    pause(cx, 30).await;
    let renaming = cx.update(|_, cx| history.read(cx).demo_renaming_index()).unwrap();
    report("Rename starts inline editing", renaming == Some(0), format!("{renaming:?}"));
    let focused = cx.update(|window, cx| history.read(cx).demo_renaming_focused(window, cx)).unwrap();
    report("the rename field holds the keyboard focus", focused, String::new());
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

    if let Some(row) = row_point(&history, 0, cx).await {
        click(cx, row, MouseButton::Left, Modifiers::default());
        pause(cx, 30).await;
        key(cx, "alt-enter");
        pause(cx, 50).await;
        draw(cx);
        let title = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.read(cx).title(cx).to_string())).unwrap();
        report("a renamed favorite opens in a tab titled with its name", title.as_deref() == Some("Renamed fav"), format!("{title:?}"));
    }

    let Some(row) = row_point(&history, 0, cx).await else {
        println!("FAIL the favorite row has no bounds");
        return false;
    };
    click(cx, row, MouseButton::Right, Modifiers::default());
    pause(cx, 30).await;
    menu_click(&history, "Rename", cx);
    pause(cx, 30).await;
    key(cx, "cmd-a");
    typ(cx, "whoops");
    key(cx, "escape");
    let (name, side_open, renaming) =
        cx.update(|_, cx| (history.read(cx).demo_favorites()[0].1.clone(), ws.read(cx).explorer_open && ws.read(cx).view == NavView::Favorites, history.read(cx).demo_renaming_index())).unwrap();
    report("esc cancels the rename, keeps the old name, and the panel stays open", name == "Renamed fav" && side_open && renaming.is_none(), format!("{name:?} side_open={side_open} renaming={renaming:?}"));

    let Some(row) = row_point(&history, 0, cx).await else {
        println!("FAIL the favorite row has no bounds");
        return false;
    };
    click(cx, row, MouseButton::Right, Modifiers::default());
    pause(cx, 30).await;
    menu_click(&history, "Rename", cx);
    pause(cx, 30).await;
    key(cx, "cmd-a");
    key(cx, "backspace");
    key(cx, "enter");
    let name = cx.update(|_, cx| history.read(cx).demo_favorites()[0].1.clone()).unwrap();
    report("an empty name reverts to the old one", name == "Renamed fav", name);

    let Some(row) = row_point(&history, 0, cx).await else {
        println!("FAIL the favorite row has no bounds");
        return false;
    };
    click(cx, row, MouseButton::Right, Modifiers::default());
    pause(cx, 30).await;
    let menu_open = cx.update(|_, cx| history.read(cx).demo_fav_menu_open()).unwrap_or(false);
    report("right-click opens the favorite's context menu", menu_open, String::new());
    menu_click(&history, "Remove", cx);
    pause(cx, 30).await;
    let count = cx.update(|_, cx| history.read(cx).demo_favorites().len()).unwrap();
    report("Remove deletes the favorite", count == 0, format!("{count}"));

    // The menu opens 26 pt under the button; OK is its second row (4 pt padding, 26 pt rows).
    rail_click(ws, NavView::History, cx);
    pause(cx, 30).await;
    draw(cx);
    click(cx, dd_click, MouseButton::Left, Modifiers::default());
    pause(cx, 30).await;
    draw(cx);
    click(cx, point(dd.origin.x + px(20.), dd.origin.y + px(26. + 4. + 26. + 13.)), MouseButton::Left, Modifiers::default());
    pause(cx, 30).await;
    let still = !cx.update(|_, cx| history.read(cx).demo_showing_favorites()).unwrap();
    let closed = !cx.update(|_, cx| history.read(cx).demo_dropdown_open()).unwrap_or(true);
    report("a status filter keeps the History view, its menu shut", still && closed, format!("history={still} closed={closed}"));

    let count = |cx: &mut AsyncWindowContext| cx.update(|_, cx| history.read(cx).demo_entry_count()).unwrap_or(0);
    let n0 = count(cx);
    let _ = cx.update(|_, cx| history.update(cx, |h, cx| h.demo_open_entry_menu(0, cx)));
    draw(cx);
    let labels = cx.update(|_, cx| history.read(cx).demo_menu_labels()).unwrap_or_default();
    let want = |fav: &str| ["Run", "Open in new tab", "—", fav, "—", "Remove from history"].map(String::from).to_vec();
    report(
        "an entry's menu: Run, Open in new tab, Add to favorites, Remove from history",
        labels == want("Add to favorites") || labels == want("Remove from favorites"),
        format!("{labels:?}"),
    );
    menu_click(&history, "Remove from history", cx);
    pause(cx, 30).await;
    let removed = count(cx);
    let _ = cx.update(|window, cx| window.focus(&gpui::Focusable::focus_handle(&history, cx), cx));
    key(cx, "cmd-z");
    pause(cx, 30).await;
    report("Remove from history takes the entry out; ⌘Z puts it back", n0 > 0 && removed == n0 - 1 && count(cx) == n0, format!("{n0} → {removed} → {}", count(cx)));
    let trash = cx.update(|_, cx| history.read(cx).demo_clear_bounds()).ok().flatten();
    if let Some(b) = trash {
        click(cx, b.center(), MouseButton::Left, Modifiers::default());
        pause(cx, 30).await;
        draw(cx);
    }
    let (asking, kept) = (cx.update(|_, cx| history.read(cx).demo_clear_asking()).unwrap_or(false), count(cx));
    if let Some(b) = cx.update(|_, cx| history.read(cx).demo_clear_bounds()).ok().flatten() {
        click(cx, b.center(), MouseButton::Left, Modifiers::default());
        pause(cx, 30).await;
    }
    let cleared = count(cx);
    let _ = cx.update(|window, cx| window.focus(&gpui::Focusable::focus_handle(&history, cx), cx));
    key(cx, "cmd-z");
    pause(cx, 30).await;
    report(
        "the trash asks \"Clear all?\" first, then clears the history; ⌘Z brings it back",
        trash.is_some() && asking && kept == n0 && cleared == 0 && count(cx) == n0,
        format!("asking {asking}, {kept} → {cleared} → {}", count(cx)),
    );
    rail_click(ws, NavView::Favorites, cx);
    pause(cx, 30).await;
    draw(cx);

    let dir = std::env::temp_dir().join(format!("dataman-fav-import-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let files = [("top customers.sql", "SELECT * FROM customer ORDER BY id LIMIT 10;"), ("orders.sql", "SELECT count(*) FROM orders;"), ("again.SQL", "SELECT * FROM customer ORDER BY id LIMIT 10;")];
    let paths: Vec<std::path::PathBuf> = files.iter().map(|(n, sql)| {
        let p = dir.join(n);
        let _ = std::fs::write(&p, sql);
        p
    }).collect();
    let before = cx.update(|_, cx| history.read(cx).demo_favorites().len()).unwrap();
    if let Some(fb) = cx.update(|_, cx| history.read(cx).demo_new_folder_bounds()).unwrap() {
        let at = point(fb.origin.x + fb.size.width / 2., fb.origin.y + fb.size.height / 2.);
        click(cx, at, MouseButton::Right, Modifiers::default());
        pause(cx, 30).await;
        draw(cx);
        crate::sql_picker::set_test_paths(paths.clone());
        menu_click(&history, "Import favorites…", cx);
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

fn favorite(ws: &Entity<Workspace>, id: &str, cx: &mut AsyncWindowContext) -> Option<dataman_core::Favorite> {
    cx.update(|_, cx| ws.read(cx).store.read(cx).core.favorites.list().into_iter().find(|f| f.id == id)).ok().flatten()
}

fn linked(tab: &Entity<crate::tab::QueryTab>, cx: &mut AsyncWindowContext) -> (Option<String>, String) {
    cx.update(|_, cx| (tab.read(cx).favorite.as_ref().map(|(id, _)| id.clone()), tab.read(cx).title(cx).to_string())).unwrap()
}

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
    report(&mut ok, "\"← added to favorites\" points at the rail's lit Favorites icon", shown.as_ref().is_some_and(|(id, l)| Some(id) == id_a.as_ref() && *l == "added to favorites"), format!("{shown:?}"));

    let b = tab_with(ws, "SELECT 42 AS answer;\nSELECT 1;", cx);
    key(cx, "cmd-s");
    pause(cx, 30).await;
    draw(cx);
    let (id_b, title_b) = linked(&b, cx);
    let fav_b = id_b.as_deref().and_then(|id| favorite(ws, id, cx));
    report(&mut ok, "a name already taken gets a number", fav_b.as_ref().is_some_and(|f| f.name == "SELECT 42 AS answer 2") && title_b == "SELECT 42 AS answer 2" && id_b != id_a, format!("{fav_b:?} {title_b:?}"));

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
                    (ws.read(cx).explorer_open && ws.read(cx).view == NavView::Favorites, h.demo_showing_favorites(), h.demo_selected_favorite(), h.demo_has_focus(window), ws.read(cx).saved_flash.is_none())
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
        "⌘S in a favorite's tab saves its script into it, keeping its name (\"saved to favorites\")",
        after.as_ref().is_some_and(|f| f.sql == "SELECT 43 AS answer;" && f.name == "SELECT 42 AS answer 2") && still == id_b && title == "SELECT 42 AS answer 2" && shown.is_some_and(|(_, l)| l == "saved to favorites"),
        format!("{after:?} {title:?}"),
    );

    if let Some(id) = id_b.clone() {
        let ix = cx.update(|_, cx| history.read(cx).demo_favorites().iter().position(|f| f.0 == id)).unwrap();
        if let Some(ix) = ix {
            let _ = cx.update(|window, cx| history.update(cx, |h, cx| h.demo_rename(ix, "Answers", window, cx)));
            draw(cx);
            let (_, title) = linked(&b, cx);
            report(&mut ok, "a favorite renamed in the panel renames its tab", title == "Answers", title);
        }
    }

    let dir = std::env::temp_dir().join(format!("dataman-save-as-{}", std::process::id()));
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

async fn palette_checks(ws: &Entity<Workspace>, history: &Entity<HistoryPanel>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let words = ["orders", "customers", "revenue", "churn", "latency"];
    let ids = cx
        .update(|_, cx| {
            let core = ws.read(cx).store.read(cx).core.clone();
            (0..500).filter_map(|i| core.favorites.add(dataman_core::Favorite::new(format!("Report {i:03} {}", words[i % 5]), format!("SELECT {i} AS n, 'marker_{i:03}' AS tag FROM orders"))).ok().map(|f| f.id)).collect::<Vec<_>>()
        })
        .unwrap();
    report(&mut ok, "500 favorites saved", ids.len() == 500, format!("{}", ids.len()));
    let _ = cx.update(|_, cx| {
        ws.update(cx, |w, cx| {
            w.set_nav(NavView::Explorer, cx);
            w.set_explorer_open(false, cx);
        })
    });
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

    let _ = cx.update(|_, cx| palette.update(cx, |p, cx| p.set_query("marker_123", cx)));
    let (label, ..) = state(cx);
    report(&mut ok, "a piece of a favorite's SQL finds it", label == "Report 123 churn", label);
    let _ = cx.update(|_, cx| palette.update(cx, |p, cx| p.set_query("report 497 rev", cx)));
    draw(cx);
    let (label, ..) = state(cx);
    report(&mut ok, "its name finds it (fuzzy), among the other kinds", label == "Report 497 revenue", label);
    key(cx, "enter");
    draw(cx);
    let (closed, opening) = cx.update(|_, cx| (ws.read(cx).palette.is_none(), ws.read(cx).demo_explorer_sliding() || cx.reduce_motion())).unwrap();
    pause(cx, 400).await;
    for _ in 0..3 {
        draw(cx);
        pause(cx, 20).await;
    }
    let (side, favs, sel, focus) = cx
        .update(|window, cx| {
            let h = history.read(cx);
            (ws.read(cx).explorer_open && ws.read(cx).view == NavView::Favorites, h.demo_showing_favorites(), h.demo_selected_favorite(), h.demo_has_focus(window))
        })
        .unwrap();
    let want = ids.get(497).cloned();
    report(
        &mut ok,
        "↩ slides the favorites in, the one picked selected, scrolled into view (near the bottom) and focused",
        closed && opening && side && favs && focus && sel.as_ref().is_some_and(|(id, ix, inside)| Some(id) == want.as_ref() && *ix > 400 && *inside),
        format!("closed {closed} opening {opening} side {side} favorites {favs} focus {focus} {sel:?}"),
    );
    key(cx, "down");
    draw(cx);
    let next = cx.update(|_, cx| history.read(cx).demo_selected_favorite()).unwrap();
    report(&mut ok, "↓ in the list moves on from it", next.as_ref().is_some_and(|(id, _, inside)| Some(id) == ids.get(498) && *inside), format!("{next:?}"));

    let conn_id = cx.update(|_, cx| ws.read(cx).active_tab().and_then(|t| t.read(cx).connection().map(|c| c.profile.id.clone()))).unwrap();
    if let Some(conn_id) = conn_id {
        let fav = cx
            .update(|_, cx| {
                let core = ws.read(cx).store.read(cx).core.clone();
                let mut f = dataman_core::Favorite::new("Offline favorite", "SELECT 'offline' AS state");
                f.connection_id = Some(conn_id.clone());
                core.favorites.add(f).ok()
            })
            .unwrap();
        let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.store.update(cx, |s, cx| s.disconnect(&conn_id, cx))));
        pause(cx, 200).await;
        let offline = cx.update(|_, cx| ws.read(cx).store.read(cx).connection(&conn_id).is_none()).unwrap();
        if let Some(fav) = fav {
            let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_favorite(fav, false, window, cx)));
            let mut got = (false, String::new());
            for _ in 0..200 {
                pause(cx, 50).await;
                got = cx
                    .update(|_, cx| {
                        let t = ws.read(cx).active_tab().cloned();
                        let on = t.as_ref().and_then(|t| t.read(cx).connection().map(|c| c.profile.id.clone())) == Some(conn_id.clone());
                        (on, t.map(|t| t.read(cx).editor.read(cx).text().to_string()).unwrap_or_default())
                    })
                    .unwrap();
                if got.0 {
                    break;
                }
            }
            report(
                &mut ok,
                "a favorite on a disconnected connection opens in a tab that connects",
                offline && got.0 && got.1.contains("'offline'"),
                format!("offline before {offline}, connected {}, text {:?}", got.0, got.1),
            );
        }
    }
    ok
}

pub(crate) async fn drag(cx: &mut AsyncWindowContext, from: Point<Pixels>, to: Point<Pixels>, before_up: impl FnOnce(&mut AsyncWindowContext)) {
    let none = Modifiers::default();
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: from, pressed_button: None, modifiers: none }));
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: from, modifiers: none, click_count: 1, first_mouse: false }));
    for i in 1..=8 {
        let k = i as f32 / 8.;
        let p = point(from.x + (to.x - from.x) * k, from.y + (to.y - from.y) * k);
        input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: Some(MouseButton::Left), modifiers: none }));
        pause(cx, 10).await;
    }
    draw(cx);
    before_up(cx);
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: to, modifiers: none, click_count: 1 }));
    pause(cx, 30).await;
    draw(cx);
}

fn double_click(cx: &mut AsyncWindowContext, p: Point<Pixels>) {
    let none = Modifiers::default();
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: None, modifiers: none }));
    for n in 1..=2 {
        input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: p, modifiers: none, click_count: n, first_mouse: false }));
        input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: p, modifiers: none, click_count: n }));
    }
}

pub(crate) async fn named_point(history: &Entity<HistoryPanel>, name: &str, cx: &mut AsyncWindowContext) -> Option<Point<Pixels>> {
    let ix = cx.update(|_, cx| history.read(cx).demo_item_named(name)).ok().flatten()?;
    row_point(history, ix, cx).await
}

pub(crate) fn rows(history: &Entity<HistoryPanel>, cx: &mut AsyncWindowContext) -> Vec<String> {
    cx.update(|_, cx| history.read(cx).demo_rows()).unwrap_or_default()
}

pub(crate) fn filed(history: &Entity<HistoryPanel>, name: &str, cx: &mut AsyncWindowContext) -> Option<Option<String>> {
    cx.update(|_, cx| history.read(cx).demo_filed().into_iter().find(|f| f.0 == name).map(|f| f.1)).ok().flatten()
}

fn snap(name: &str, cx: &mut AsyncWindowContext) {
    let Ok(dir) = std::env::var("DATAMAN_SHOT_DIR") else { return };
    let path = format!("{dir}/folders-{name}.png");
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
    });
}

async fn folders(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let history = cx.update(|_, cx| ws.read(cx).history.clone()).unwrap();
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.demo_favorites_view(cx)));
    for _ in 0..100 {
        if cx.update(|_, cx| history.read(cx).demo_entry_count() > 0).unwrap_or(false) {
            break;
        }
        pause(cx, 50).await;
    }
    pause(cx, 400).await;
    let on_favs = cx.update(|_, cx| history.read(cx).demo_showing_favorites()).unwrap();
    report(&mut ok, "the panel opens on the favorites", on_favs, String::new());

    let conn_id = cx.update(|_, cx| ws.read(cx).active_tab().and_then(|t| t.read(cx).connection().map(|c| c.profile.id.clone()))).unwrap();
    let run_sql = "SELECT 'folders-run' AS marker";
    let _ = cx.update(|_, cx| {
        let core = ws.read(cx).store.read(cx).core.clone();
        for (name, sql) in [("Orders by day", "SELECT date_trunc('day', created) AS day, count(*) FROM orders GROUP BY 1 ORDER BY 1"), ("Top customers", "SELECT customer, count(*) FROM orders GROUP BY 1 ORDER BY 2 DESC LIMIT 10"), ("Run marker", run_sql)] {
            let mut f = dataman_core::Favorite::new(name, sql);
            f.connection_id = conn_id.clone();
            let _ = core.favorites.add(f);
        }
    });
    rail_click(ws, NavView::Explorer, cx);
    rail_click(ws, NavView::Favorites, cx);
    let listed = rows(&history, cx);
    report(
        &mut ok,
        "the rail's Favorites lists favorites saved behind the panel at once",
        ["Orders by day", "Top customers", "Run marker"].iter().all(|n| listed.contains(&n.to_string())),
        format!("{listed:?}"),
    );
    draw(cx);

    let new_folder = |cx: &mut AsyncWindowContext| {
        draw(cx);
        match cx.update(|_, cx| history.read(cx).demo_new_folder_bounds()).unwrap() {
            Some(b) => {
                click(cx, b.center(), MouseButton::Left, Modifiers::default());
                true
            }
            None => false,
        }
    };
    let clicked = new_folder(cx);
    pause(cx, 30).await;
    let (naming, focused) = cx.update(|window, cx| (history.read(cx).demo_renaming_folder(), history.read(cx).demo_renaming_focused(window, cx))).unwrap();
    report(&mut ok, "\"+ folder\" makes a folder and names it in place", clicked && naming.as_deref() == Some("New folder") && focused, format!("clicked {clicked} naming {naming:?} focused {focused}"));
    typ(cx, "Reports");
    key(cx, "enter");
    let _ = new_folder(cx);
    pause(cx, 30).await;
    typ(cx, "Archive");
    key(cx, "enter");
    let r = rows(&history, cx);
    report(
        &mut ok,
        "typed over, the names stick; the folders come first, a new one last, then the top level (no counts)",
        r == ["▾ Reports", "▾ Archive", "Orders by day", "Top customers", "Run marker"],
        format!("{r:?}"),
    );

    let store_folders = |cx: &mut AsyncWindowContext| -> Vec<String> {
        cx.update(|_, cx| ws.read(cx).store.read(cx).core.favorites.folders().into_iter().map(|d| d.name).collect()).unwrap_or_default()
    };
    let (Some(from), Some(to)) = (named_point(&history, "Archive", cx).await, named_point(&history, "Reports", cx).await) else {
        report(&mut ok, "the folders have bounds", false, String::new());
        return ok;
    };
    let mut during = (None, None);
    drag(cx, from, point(to.x, to.y - px(8.)), |cx| {
        during = cx.update(|_, cx| (history.read(cx).demo_folder_drop(), history.read(cx).demo_drop_at())).unwrap_or_default();
        snap("reorder", cx);
    })
    .await;
    let (r, stored) = (rows(&history, cx), store_folders(cx));
    report(
        &mut ok,
        "a folder dragged over the top half of another: a line above that one, no folder tinted",
        during == (Some((0, vec![(0, false)])), None),
        format!("{during:?}"),
    );
    report(
        &mut ok,
        "dropped, the folder moves there, and the store keeps the order",
        r == ["▾ Archive", "▾ Reports", "Orders by day", "Top customers", "Run marker"] && stored == ["Archive", "Reports"],
        format!("{r:?} stored {stored:?}"),
    );
    let (Some(from), Some(to)) = (named_point(&history, "Archive", cx).await, named_point(&history, "Top customers", cx).await) else { return false };
    let mut during = None;
    drag(cx, from, to, |cx| during = cx.update(|_, cx| history.read(cx).demo_folder_drop()).ok().flatten()).await;
    let (r, stored) = (rows(&history, cx), store_folders(cx));
    report(
        &mut ok,
        "dragged over the top level, it goes after the last folder (the line under it)",
        during == Some((2, vec![(1, true)])) && r == ["▾ Reports", "▾ Archive", "Orders by day", "Top customers", "Run marker"] && stored == ["Reports", "Archive"],
        format!("{during:?} {r:?} stored {stored:?}"),
    );

    let (Some(from), Some(to)) = (named_point(&history, "Orders by day", cx).await, named_point(&history, "Reports", cx).await) else {
        report(&mut ok, "the rows have bounds", false, String::new());
        return ok;
    };
    let (mut target, mut line) = (None, None);
    drag(cx, from, to, |cx| {
        (target, line) = cx.update(|_, cx| (history.read(cx).demo_drop_at(), history.read(cx).demo_folder_drop())).unwrap_or_default();
        snap("drag", cx);
    })
    .await;
    report(&mut ok, "a favorite dragged over a folder targets it (no line between folders)", target.as_deref() == Some("Reports") && line.is_none(), format!("{target:?} {line:?}"));
    let r = rows(&history, cx);
    report(
        &mut ok,
        "dropped there, it is filed in it and listed under it",
        filed(&history, "Orders by day", cx) == Some(Some("Reports".into())) && r == ["▾ Reports", "  Orders by day", "▾ Archive", "Top customers", "Run marker"],
        format!("{r:?}"),
    );

    let Some(folder) = named_point(&history, "Reports", cx).await else { return false };
    click(cx, folder, MouseButton::Left, Modifiers::default());
    pause(cx, 450).await;
    let shut = rows(&history, cx);
    key(cx, "right");
    let opened = rows(&history, cx);
    key(cx, "left");
    pause(cx, 450).await;
    let shut_again = rows(&history, cx);
    report(
        &mut ok,
        "a click shuts a folder; → opens it, ← shuts it",
        shut.contains(&"▸ Reports".to_string()) && !shut.contains(&"  Orders by day".to_string()) && opened.contains(&"  Orders by day".to_string()) && shut_again == shut,
        format!("{shut:?} / {opened:?} / {shut_again:?}"),
    );

    click(cx, folder, MouseButton::Left, Modifiers::default());
    pause(cx, 60).await;
    let moving = cx.update(|_, cx| history.read(cx).demo_folding()).unwrap_or(false);
    snap("drain", cx);
    pause(cx, 700).await;
    let still = cx.update(|_, cx| history.read(cx).demo_folding()).unwrap_or(true);
    let shown = rows(&history, cx);
    report(
        &mut ok,
        "a click opening a folder drains its icon's fill and unfolds its favorites, then stops",
        moving && !still && shown.contains(&"  Orders by day".to_string()),
        format!("moving {moving}, still {still}, {shown:?}"),
    );
    click(cx, folder, MouseButton::Left, Modifiers::default());
    pause(cx, 60).await;
    let during = rows(&history, cx);
    let moving = cx.update(|_, cx| history.read(cx).demo_folding()).unwrap_or(false);
    snap("shut", cx);
    pause(cx, 400).await;
    let after = rows(&history, cx);
    report(
        &mut ok,
        "a click shutting a folder folds its favorites away, then they are gone",
        moving && during.contains(&"▸ Reports".to_string()) && during.contains(&"  Orders by day".to_string()) && after.contains(&"▸ Reports".to_string()) && !after.contains(&"  Orders by day".to_string()),
        format!("moving {moving}, {during:?} / {after:?}"),
    );

    let _ = cx.update(|window, cx| history.update(cx, |h, cx| h.demo_search("by day", window, cx)));
    let found = rows(&history, cx);
    let _ = cx.update(|window, cx| history.update(cx, |h, cx| h.demo_search("", window, cx)));
    let cleared = rows(&history, cx);
    report(
        &mut ok,
        "a search opens the folder holding a match; cleared, it is shut again",
        found == ["▾ Reports", "  Orders by day"] && cleared.contains(&"▸ Reports".to_string()),
        format!("{found:?} / {cleared:?}"),
    );

    let Some(folder) = named_point(&history, "Reports", cx).await else { return false };
    click(cx, folder, MouseButton::Left, Modifiers::default());
    pause(cx, 30).await;
    let Some(from) = named_point(&history, "Orders by day", cx).await else { return false };
    let list = cx.update(|_, cx| history.read(cx).demo_list_bounds()).unwrap();
    let below = point(list.center().x, list.bottom() - px(20.));
    let mut target = None;
    drag(cx, from, below, |cx| target = cx.update(|_, cx| history.read(cx).demo_drop_at()).ok().flatten()).await;
    report(
        &mut ok,
        "dragged out of its folder, below the rows, it is back at the top level",
        target.as_deref() == Some("top") && filed(&history, "Orders by day", cx) == Some(None),
        format!("target {target:?}, filed {:?}", filed(&history, "Orders by day", cx)),
    );

    let Some(p) = named_point(&history, "Top customers", cx).await else { return false };
    click(cx, p, MouseButton::Right, Modifiers::default());
    pause(cx, 30).await;
    let moved = match menu_at(&history, "Move to folder", false, cx) {
        Some(m) => {
            input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: m, pressed_button: None, modifiers: Modifiers::default() }));
            match menu_at(&history, "Archive", true, cx) {
                Some(s) => {
                    click(cx, s, MouseButton::Left, Modifiers::default());
                    pause(cx, 30).await;
                    true
                }
                None => false,
            }
        }
        None => false,
    };
    report(&mut ok, "Move to folder ▸ files a favorite in the folder picked", moved && filed(&history, "Top customers", cx) == Some(Some("Archive".into())), format!("submenu {moved}, {:?}", filed(&history, "Top customers", cx)));

    let Some(p) = named_point(&history, "Run marker", cx).await else { return false };
    click(cx, p, MouseButton::Right, Modifiers::default());
    pause(cx, 30).await;
    let clicked = menu_click(&history, "Run", cx);
    let mut ran = false;
    for _ in 0..100 {
        pause(cx, 50).await;
        if cx.update(|_, cx| history.read(cx).demo_has_entry(run_sql)).unwrap_or(false) {
            ran = true;
            break;
        }
    }
    let title = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.read(cx).title(cx).to_string())).unwrap();
    report(&mut ok, "Run (the favorite's menu) runs it in a tab of its own", clicked && ran && title.as_deref() == Some("Run marker"), format!("clicked {clicked} ran {ran} title {title:?}"));

    let before = rows(&history, cx);
    let Some(p) = named_point(&history, "Archive", cx).await else { return false };
    double_click(cx, p);
    pause(cx, 30).await;
    let naming = cx.update(|_, cx| history.read(cx).demo_renaming_folder()).unwrap();
    let after = rows(&history, cx);
    report(
        &mut ok,
        "a double-click on a folder renames nothing, it only opens or shuts it",
        naming.is_none() && before.contains(&"▾ Archive".to_string()) && after.contains(&"▸ Archive".to_string()),
        format!("naming {naming:?} {before:?} → {after:?}"),
    );

    click(cx, p, MouseButton::Right, Modifiers::default());
    pause(cx, 30).await;
    let clicked = menu_click(&history, "Rename", cx);
    pause(cx, 30).await;
    let (naming, focused) = cx.update(|window, cx| (history.read(cx).demo_renaming_folder(), history.read(cx).demo_renaming_focused(window, cx))).unwrap();
    key(cx, "cmd-a");
    typ(cx, "Old stuff");
    key(cx, "enter");
    let names: Vec<String> = cx.update(|_, cx| history.read(cx).demo_folders().into_iter().map(|d| d.1).collect()).unwrap();
    report(
        &mut ok,
        "Rename in a folder's menu names it in place, and it keeps its place",
        clicked && naming.as_deref() == Some("Archive") && focused && names == ["Reports", "Old stuff"],
        format!("clicked {clicked} naming {naming:?} focused {focused} {names:?}"),
    );

    let state = |cx: &mut AsyncWindowContext| -> (Vec<String>, usize, bool) {
        cx.update(|_, cx| {
            let h = history.read(cx);
            (h.demo_folders().into_iter().map(|d| d.1).collect(), h.demo_favorites().len(), h.demo_fav_menu_open())
        })
        .unwrap_or_default()
    };
    let sure = "Sure? (deletes 1 query)";
    let Some(p) = named_point(&history, "Old stuff", cx).await else { return false };
    click(cx, p, MouseButton::Right, Modifiers::default());
    pause(cx, 30).await;
    let clicked = menu_click(&history, "Delete folder", cx);
    pause(cx, 30).await;
    let asking = menu_at(&history, sure, false, cx).is_some() && menu_at(&history, "Delete folder", false, cx).is_none();
    snap("sure", cx);
    let (names, count, open) = state(cx);
    report(
        &mut ok,
        "Delete folder asks first: its row says \"Sure? (deletes 1 query)\", the menu stays, nothing is deleted",
        clicked && asking && open && names == ["Reports", "Old stuff"] && count == 3,
        format!("clicked {clicked} asking {asking} open {open} {names:?} count {count}"),
    );
    if let Some(r) = menu_at(&history, "Rename", false, cx) {
        input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: r, pressed_button: None, modifiers: Modifiers::default() }));
    }
    draw(cx);
    let left = menu_at(&history, "Delete folder", false, cx).is_some();
    let _ = menu_click(&history, "Delete folder", cx);
    pause(cx, 30).await;
    let again = menu_at(&history, sure, false, cx).is_some();
    key(cx, "escape");
    let esc = (menu_at(&history, "Delete folder", false, cx).is_some(), state(cx).2);
    let _ = menu_click(&history, "Delete folder", cx);
    pause(cx, 3300).await;
    draw(cx);
    let waited = menu_at(&history, "Delete folder", false, cx).is_some();
    report(
        &mut ok,
        "the \"Sure?\" gives way to Delete folder when the pointer leaves it, on esc (the menu stays) and after a few seconds",
        left && again && esc == (true, true) && waited && state(cx).0 == ["Reports", "Old stuff"],
        format!("left {left} again {again} esc {esc:?} waited {waited}"),
    );

    let _ = menu_click(&history, "Delete folder", cx);
    pause(cx, 30).await;
    let confirmed = menu_click(&history, sure, cx);
    pause(cx, 30).await;
    let (names, count, open) = state(cx);
    report(
        &mut ok,
        "\"Sure?\" clicked: the folder is deleted with the query in it, the others stay",
        confirmed && !open && names == ["Reports"] && count == 2 && filed(&history, "Top customers", cx).is_none(),
        format!("confirmed {confirmed} open {open} {names:?} count {count} {:?}", filed(&history, "Top customers", cx)),
    );

    if std::env::var_os("DATAMAN_SHOT_DIR").is_some() {
        let _ = cx.update(|_, cx| {
            let core = ws.read(cx).store.read(cx).core.clone();
            let reports = core.favorites.folders().into_iter().find(|d| d.name == "Reports").map(|d| d.id);
            let archive = core.favorites.add_folder("Archive").ok().map(|d| d.id);
            for (i, (name, to)) in [("Daily signups", &reports), ("Revenue by month", &reports), ("Slow queries", &archive)].into_iter().enumerate() {
                let mut f = dataman_core::Favorite::new(name, format!("SELECT {i} AS n, count(*) FROM orders WHERE customer % 3 = {i}"));
                f.connection_id = conn_id.clone();
                if let Ok(f) = core.favorites.add(f) {
                    let _ = core.favorites.move_to_folder(&f.id, to.as_deref());
                }
            }
            ws.update(cx, |w, cx| w.demo_favorites_view(cx));
        });
        if let (Some(p), Some(to)) = (named_point(&history, "Orders by day", cx).await, named_point(&history, "Reports", cx).await) {
            drag(cx, p, to, |_| {}).await;
        }
        if let Some(p) = named_point(&history, "Revenue by month", cx).await {
            click(cx, p, MouseButton::Left, Modifiers::default());
        }
        pause(cx, 50).await;
        snap("tree", cx);
        if let Some(p) = named_point(&history, "Run marker", cx).await {
            click(cx, p, MouseButton::Right, Modifiers::default());
            if let Some(m) = menu_at(&history, "Move to folder", false, cx) {
                input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: m, pressed_button: None, modifiers: Modifiers::default() }));
                draw(cx);
                snap("menu", cx);
            }
            key(cx, "escape");
        }
    }
    println!("{}", if ok { "ALL PASS" } else { "SOME CHECKS FAILED" });
    ok
}
