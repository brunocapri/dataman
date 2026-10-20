use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Focusable, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels, Point, ScrollDelta, ScrollWheelEvent, TouchPhase, point, px};

use crate::app::Workspace;
use crate::grid::ResultGrid;

pub const SQL: &str = "SELECT * FROM orders ORDER BY id LIMIT 500;";

fn input(cx: &mut AsyncWindowContext, e: PlatformInput) {
    let _ = cx.update(|window, cx| {
        window.dispatch_event(e, cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn key(cx: &mut AsyncWindowContext, k: &str) {
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(crate::keymap::keystroke(k), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn down(cx: &mut AsyncWindowContext, p: Point<Pixels>, button: MouseButton, modifiers: Modifiers, click_count: usize) {
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button, position: p, modifiers, click_count, first_mouse: false }));
}

fn up(cx: &mut AsyncWindowContext, p: Point<Pixels>, button: MouseButton, modifiers: Modifiers, click_count: usize) {
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button, position: p, modifiers, click_count }));
}

fn click(cx: &mut AsyncWindowContext, p: Point<Pixels>, modifiers: Modifiers) {
    down(cx, p, MouseButton::Left, modifiers, 1);
    up(cx, p, MouseButton::Left, modifiers, 1);
}

fn right_click(cx: &mut AsyncWindowContext, p: Point<Pixels>) {
    down(cx, p, MouseButton::Right, Modifiers::default(), 1);
    up(cx, p, MouseButton::Right, Modifiers::default(), 1);
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn grid_of(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Entity<ResultGrid> {
    cx.update(|_, cx| ws.read(cx).active_tab().unwrap().read(cx).grid.clone()).unwrap()
}

async fn columns(grid: &Entity<ResultGrid>, cx: &mut AsyncWindowContext) -> Option<(usize, usize)> {
    for _ in 0..100 {
        let found = cx.update(|_, cx| {
            let g = grid.read(cx);
            let n = g.result().and_then(|r| r.columns()).map_or(0, |c| c.len());
            let fk = (0..n).find(|&c| g.is_fk_column(c))?;
            let plain = (0..n).find(|&c| !g.is_fk_column(c))?;
            Some((fk, plain))
        });
        if let Ok(Some(f)) = found {
            return Some(f);
        }
        pause(cx, 100).await;
    }
    None
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, cx: &mut AsyncWindowContext) -> bool {
    let grid = grid_of(ws, cx);
    let Some((fk, plain)) = columns(&grid, cx).await else {
        println!("FAIL no reference column was resolved");
        return false;
    };
    let _ = cx.update(|window, cx| {
        window.focus(&grid.focus_handle(cx), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
    let at = |cx: &mut AsyncWindowContext, r: usize, c: usize| cx.update(|_, cx| grid.read(cx).demo_cell_point(r, c)).unwrap();
    match stage {
        "fk-menu-context" => {
            let p = at(cx, 3, fk);
            right_click(cx, p);
            true
        }
        "fk-pop" => {
            let p = at(cx, 3, plain);
            click(cx, p, Modifiers::default());
            true
        }
        "fk-land" => {
            let p = at(cx, 3, fk);
            click(cx, p, Modifiers::secondary_key());
            true
        }
        "fk-pop-check" => pop_check(ws, &grid, fk, plain, cx).await,
        _ => check(ws, &grid, fk, plain, cx).await,
    }
}

fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

async fn pop_check(ws: &Entity<Workspace>, grid: &Entity<ResultGrid>, fk: usize, plain: usize, cx: &mut AsyncWindowContext) -> bool {
    use crate::motion::PopStyle::{Find, Select};
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let none = Modifiers::default();
    let pop = |cx: &mut AsyncWindowContext| cx.update(|_, cx| grid.read(cx).demo_pop()).unwrap();
    let at = |cx: &mut AsyncWindowContext, r: usize, c: usize| cx.update(|_, cx| grid.read(cx).demo_cell_point(r, c)).unwrap();
    let over = crate::motion::PopStyle::Find.duration().as_millis() as u64 + 50;

    let p = at(cx, 2, plain);
    click(cx, p, none);
    report("a click pops the selected value (Select)", pop(cx) == Some(((2, plain), Select)), format!("{:?}", pop(cx)));
    let q = at(cx, 3, plain);
    click(cx, q, none);
    report("a click mid-pop restarts it on the new value", pop(cx) == Some(((3, plain), Select)), format!("{:?}", pop(cx)));
    pause(cx, over).await;
    draw(cx);
    report("the pop ends by itself", pop(cx).is_none(), format!("{:?}", pop(cx)));
    click(cx, q, none);
    report("a click on the selected value does not pop again", pop(cx).is_none(), format!("{:?}", pop(cx)));
    let p = at(cx, 2, plain);
    click(cx, p, none);
    let shift = Modifiers { shift: true, ..Default::default() };
    let r5 = at(cx, 5, plain);
    click(cx, r5, shift);
    report("⇧-click extends without a pop", pop(cx).is_none(), format!("{:?}", pop(cx)));
    let (a, b) = (at(cx, 1, plain), at(cx, 4, plain));
    down(cx, a, MouseButton::Left, none, 1);
    let started = pop(cx) == Some(((1, plain), Select));
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: b, pressed_button: Some(MouseButton::Left), modifiers: none }));
    up(cx, b, MouseButton::Left, none, 1);
    report("a drag into a range drops the press's pop", started && pop(cx).is_none(), format!("{started} {:?}", pop(cx)));
    let p = at(cx, 2, plain);
    click(cx, p, none);
    key(cx, "down");
    report("arrow keys move without a pop", pop(cx).is_none(), format!("{:?}", pop(cx)));
    let p4 = at(cx, 4, plain);
    down(cx, p4, MouseButton::Left, none, 1);
    up(cx, p4, MouseButton::Left, none, 1);
    down(cx, p4, MouseButton::Left, none, 2);
    up(cx, p4, MouseButton::Left, none, 2);
    report("a double click does not pop (it edits)", pop(cx).is_none(), format!("{:?}", pop(cx)));
    pause(cx, 400).await;
    if cx.update(|_, cx| grid.read(cx).demo_editing()).unwrap() {
        key(cx, "escape");
        pause(cx, 100).await;
    }
    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
    let f2 = at(cx, 6, fk);
    click(cx, f2, none);
    let menu = cx.update(|_, cx| grid.read(cx).demo_menu()).unwrap();
    report("a click on a reference pops without opening a menu", pop(cx) == Some(((6, fk), Select)) && menu.is_none(), format!("{:?} {menu:?}", pop(cx)));

    pause(cx, over).await;
    let _ = cx.update(|_, cx| cx.set_reduce_motion(true));
    let p = at(cx, 7, plain);
    click(cx, p, none);
    report("reduce motion: a click selects without a pop", pop(cx).is_none(), format!("{:?}", pop(cx)));
    let shut = cx
        .update(|_, cx| {
            ws.update(cx, |w, cx| w.set_explorer_open(false, cx));
            let now = ws.read(cx).explorer_now();
            ws.update(cx, |w, cx| w.set_explorer_open(true, cx));
            now
        })
        .unwrap();
    report("reduce motion: ⌘B shuts the explorer at once", shut == 0., format!("width {shut}"));
    let _ = cx.update(|_, cx| crate::motion::apply_reduce_motion(cx));
    let sliding = cx
        .update(|_, cx| {
            ws.update(cx, |w, cx| w.set_explorer_open(false, cx));
            let now = ws.read(cx).explorer_now();
            ws.update(cx, |w, cx| w.set_explorer_open(true, cx));
            now
        })
        .unwrap();
    report("with motion: ⌘B slides (still wide right after)", sliding > 100., format!("width {sliding}"));
    pause(cx, 400).await;
    draw(cx);

    let p = at(cx, 2, fk);
    let cmd = Modifiers::secondary_key();
    click(cx, p, cmd);
    let mut landed = None;
    for _ in 0..60 {
        pause(cx, 50).await;
        draw(cx);
        landed = cx
            .update(|_, cx| {
                let t = ws.read(cx).active_tab()?.read(cx);
                let g = t.grid.read(cx);
                let ((row, col), style) = g.demo_pop()?;
                let name = g.result()?.columns()?.get(col)?.name.clone();
                Some((row, name, style))
            })
            .unwrap();
        if landed.is_some() {
            break;
        }
    }
    report("FK landing: the first row's key pops (Find)", landed == Some((0, "id".to_string(), Find)), format!("{landed:?}"));

    let tab = cx.update(|_, cx| ws.read(cx).active_tab().unwrap().clone()).unwrap();
    let editor = cx.update(|_, cx| tab.read(cx).editor.clone()).unwrap();
    let sql = "SELECT nope_col FROM orders";
    let _ = cx.update(|_, cx| {
        editor.update(cx, |e, cx| e.set_text(sql, cx));
        tab.update(cx, |t, cx| t.run(vec![sql.to_string()], cx));
    });
    let mut jumped = false;
    for _ in 0..40 {
        pause(cx, 100).await;
        jumped = cx.update(|_, cx| editor.update(cx, |e, cx| e.jump_to_error(cx))).unwrap();
        if jumped {
            break;
        }
    }
    draw(cx);
    let target = cx.update(|_, cx| editor.read(cx).demo_pop()).unwrap();
    let want = sql.find("nope_col").map(|i| i..i + "nope_col".len());
    report("⌘' pops the error token (Find)", jumped && target == want, format!("{jumped} {target:?} vs {want:?}"));

    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    let first = cx
        .update(|_, cx| {
            tree.update(cx, |t, cx| {
                t.set_filter("ord", cx);
                t.pop_first_match(cx);
            });
            tree.read(cx).demo_pop()
        })
        .unwrap();
    report("the filter's first match pops", first.as_ref().is_some_and(|id| id.ends_with("/r:orders")), format!("{first:?}"));
    pause(cx, over).await;
    draw(cx);
    let again = cx
        .update(|_, cx| {
            tree.update(cx, |t, cx| t.pop_first_match(cx));
            tree.read(cx).demo_pop()
        })
        .unwrap();
    report("the same hit does not pop twice", again.is_none(), format!("{again:?}"));
    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.set_filter("", cx)));
    println!("{}", if ok { "ALL PASS" } else { "SOME CHECKS FAILED" });
    ok
}

async fn check(ws: &Entity<Workspace>, grid: &Entity<ResultGrid>, fk: usize, plain: usize, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let none = Modifiers::default();
    let menu = |cx: &mut AsyncWindowContext| cx.update(|_, cx| grid.read(cx).demo_menu()).unwrap();
    let sel = |cx: &mut AsyncWindowContext| cx.update(|_, cx| grid.read(cx).demo_selection()).unwrap();
    let tabs = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap();
    let at = |cx: &mut AsyncWindowContext, r: usize, c: usize| cx.update(|_, cx| grid.read(cx).demo_cell_point(r, c)).unwrap();
    let src = cx.update(|_, cx| ws.read(cx).active_tab().unwrap().clone()).unwrap();
    let src_rs = cx.update(|_, cx| src.read(cx).result().cloned()).unwrap();

    let p = at(cx, 2, fk);
    click(cx, p, none);
    report("a plain click on a reference opens nothing", menu(cx).is_none(), format!("{:?}", menu(cx)));
    report("the click selects the cell", sel(cx) == Some(((2, fk), (2, fk))), format!("{:?}", sel(cx)));

    let q = at(cx, 2, plain);
    click(cx, q, none);
    report("a plain value opens nothing", menu(cx).is_none(), format!("{:?}", menu(cx)));

    let (a, b) = (at(cx, 2, fk), at(cx, 5, fk));
    down(cx, a, MouseButton::Left, none, 1);
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: point(b.x, (a.y + b.y) / 2.), pressed_button: Some(MouseButton::Left), modifiers: none }));
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: b, pressed_button: Some(MouseButton::Left), modifiers: none }));
    up(cx, b, MouseButton::Left, none, 1);
    report("a drag selects a range, no menu", menu(cx).is_none() && sel(cx) == Some(((2, fk), (5, fk))), format!("{:?} {:?}", menu(cx), sel(cx)));

    click(cx, p, none);
    let shift = Modifiers { shift: true, ..Default::default() };
    let c0 = at(cx, 0, fk);
    click(cx, c0, shift);
    report("shift-click extends, no menu", menu(cx).is_none() && sel(cx) == Some(((2, fk), (0, fk))), format!("{:?} {:?}", menu(cx), sel(cx)));

    right_click(cx, p);
    let p_now = at(cx, 2, fk);
    let p_now = point(p_now.x - px(30.), p_now.y);
    input(cx, PlatformInput::ScrollWheel(ScrollWheelEvent { position: p_now, delta: ScrollDelta::Pixels(point(px(0.), px(-22.))), modifiers: none, touch_phase: TouchPhase::Moved }));
    report("scrolling closes the right-click menu", menu(cx).is_none(), format!("{:?}", menu(cx)));
    input(cx, PlatformInput::ScrollWheel(ScrollWheelEvent { position: p_now, delta: ScrollDelta::Pixels(point(px(0.), px(22.))), modifiers: none, touch_phase: TouchPhase::Moved }));
    let p = at(cx, 2, fk);
    right_click(cx, p);
    key(cx, "a");
    report("typing closes the right-click menu", menu(cx).is_none(), format!("{:?}", menu(cx)));
    right_click(cx, p);
    key(cx, "escape");
    let grid_focused = cx.update(|window, cx| grid.focus_handle(cx).is_focused(window)).unwrap();
    report("esc closes it, the grid has the keys", menu(cx).is_none() && grid_focused, format!("{:?}", menu(cx)));

    let p3 = at(cx, 3, fk);
    down(cx, p3, MouseButton::Left, none, 1);
    up(cx, p3, MouseButton::Left, none, 1);
    down(cx, p3, MouseButton::Left, none, 2);
    up(cx, p3, MouseButton::Left, none, 2);
    report("a double click does not open a menu (it edits the cell)", menu(cx).is_none(), format!("{:?}", menu(cx)));
    pause(cx, 600).await;
    let editing = cx.update(|_, cx| grid.read(cx).demo_editing()).unwrap();
    println!("info  double click opened the cell editor: {editing}");
    if editing {
        key(cx, "escape");
        pause(cx, 100).await;
    }
    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));

    let p = at(cx, 2, fk);
    right_click(cx, p);
    let m = menu(cx);
    report(
        "right click on a reference: Go to, —, Copy value, Copy with headers, —, Delete row",
        m.as_ref().is_some_and(|(_, l, _, _)| {
            l.len() == 6
                && l[0].starts_with("Go to")
                && l[1..] == ["—", "Copy value", "Copy with headers", "—", "Delete row"]
        }),
        format!("{m:?}"),
    );
    key(cx, "escape");
    let q = at(cx, 2, plain);
    right_click(cx, q);
    let m = menu(cx);
    report(
        "right click on a plain value: the copy items, Delete row",
        m.as_ref().is_some_and(|(_, l, _, _)| l == &["Copy value", "Copy with headers", "—", "Delete row"]),
        format!("{m:?}"),
    );
    let saved = cx.update(|_, cx| cx.read_from_clipboard()).unwrap();
    let f = cx.update(|window, cx| grid.read(cx).menu_focus_handle().is_focused(window)).unwrap();
    report("the right-click menu takes the keys (opened on the press)", f, String::new());
    key(cx, "down");
    key(cx, "enter");
    let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text())).unwrap();
    let want = cx.update(|_, cx| grid.read(cx).result().and_then(|r| r.snapshot().text(2, plain).map(|v| v.into_owned()))).unwrap();
    report("Copy value copies the cell", copied.is_some() && copied == want, format!("{copied:?} vs {want:?}"));
    let q = at(cx, 2, plain);
    right_click(cx, q);
    key(cx, "down");
    key(cx, "down");
    key(cx, "enter");
    let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text())).unwrap();
    let name = cx.update(|_, cx| grid.read(cx).result().and_then(|r| r.columns().and_then(|c| c.get(plain).map(|c| c.name.clone())))).unwrap();
    let with = match (&name, &want) {
        (Some(n), Some(v)) => Some(format!("{n}\n{v}")),
        _ => None,
    };
    report(
        "Copy with headers copies the column's name over the value",
        copied.is_some() && copied.as_deref().map(|c| c.trim_end_matches('\n')) == with.as_deref(),
        format!("{copied:?} vs {with:?}"),
    );
    if let Some(s) = saved {
        let _ = cx.update(|_, cx| cx.write_to_clipboard(s));
    }
    let ctrl = Modifiers { control: true, ..Default::default() };
    let p = at(cx, 2, fk);
    // ⌃-click is the right click on macOS; on Linux Ctrl-click is the ⌘-click.
    if crate::keymap::LINUX {
        right_click(cx, p);
    } else {
        click(cx, p, ctrl);
    }
    let m = menu(cx);
    report(if crate::keymap::LINUX { "a right click opens the menu" } else { "⌃-click opens the right-click menu" }, m.as_ref().is_some_and(|(_, l, _, _)| l.len() == 6), format!("{m:?}"));
    key(cx, "escape");

    let n0 = tabs(cx);
    let _ = cx.update(|window, cx| {
        grid.update(cx, |g, cx| g.select_cell(4, fk, cx));
        window.focus(&grid.focus_handle(cx), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
    key(cx, "alt-down");
    let m = menu(cx);
    report("⌥↓ opens the menu with the first item highlighted", m.as_ref().is_some_and(|(c, _, hi, _)| *c == (4, fk) && *hi == Some(0)), format!("{m:?}"));
    let value = cx.update(|_, cx| grid.read(cx).result().and_then(|r| r.snapshot().text(4, fk).map(|v| v.into_owned()))).unwrap().unwrap_or_default();
    key(cx, "enter");
    pause(cx, 1500).await;
    let (n1, title, rows, sql, same_db) = cx
        .update(|_, cx| {
            let w = ws.read(cx);
            let t = w.active_tab().unwrap().read(cx);
            let s = src.read(cx);
            (
                w.tabs.len(),
                t.title(cx).to_string(),
                t.result().map_or(0, |r| r.rows()),
                t.editor.read(cx).text().to_string(),
                t.database() == s.database() && t.connection().map(|c| c.profile.id.clone()) == s.connection().map(|c| c.profile.id.clone()),
            )
        })
        .unwrap();
    report("↩ opens a new tab", n1 == n0 + 1, format!("{n0} → {n1}"));
    report("on the same connection and database", same_db, String::new());
    report(&format!("titled like the explorer's tabs ({title})"), title.ends_with(&format!(" · id = {value}")), title.clone());
    report(&format!("runs `{sql}` at once: one row"), rows == 1 && sql.contains(&format!("WHERE id = {value};")), format!("{rows} rows"));
    let src_same = cx.update(|_, cx| match (src.read(cx).result(), &src_rs) {
        (Some(a), Some(b)) => std::sync::Arc::ptr_eq(a, b),
        _ => false,
    });
    report("the source tab keeps its result", src_same.unwrap_or(false), String::new());

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.activate_tab(&src, window, cx)));
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
    let p5 = at(cx, 5, fk);
    let cmd = Modifiers::secondary_key();
    click(cx, p5, cmd);
    pause(cx, 300).await;
    let n2 = tabs(cx);
    report("⌘-click opens the row without a menu", n2 == n1 + 1 && menu(cx).is_none(), format!("{n1} → {n2}"));

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.activate_tab(&src, window, cx)));
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
    let p = at(cx, 2, fk);
    right_click(cx, p);
    if let Some((_, _, _, (x, y))) = menu(cx) {
        let item = point(px(x + 60.), px(y + crate::tree_menu::PAD + crate::tree_menu::ITEM_H / 2.));
        click(cx, item, none);
    }
    pause(cx, 300).await;
    let n3 = tabs(cx);
    report("a click on \"Go to …\" opens the row", n3 == n2 + 1, format!("{n2} → {n3}"));
    println!("{}", if ok { "ALL PASS" } else { "SOME CHECKS FAILED" });
    ok
}
