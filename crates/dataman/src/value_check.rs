use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Focusable, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels, Point, point, px};

use crate::app::Workspace;
use crate::grid::ResultGrid;

pub const SQL: &str = "SELECT p.id, p.status, p.address, ARRAY[p.id, p.id * 2, NULL] AS ids, ARRAY['a b', 'c\"d'] AS tags,\n       jsonb_build_object('n', p.id, 'tags', jsonb_build_array('x', 'y'), 'agent', jsonb_build_object('id', 7, 'ok', true)) AS meta,\n       '{\"x\": 1}'::text AS text_json, NULL::jsonb AS nothing, '42'::jsonb AS scalar\nFROM   orders p\nORDER  BY p.id\nLIMIT  50;";

const ADDRESS: usize = 2;
const IDS: usize = 3;
const META: usize = 5;

pub fn is_stage(s: &str) -> bool {
    s == "value-check"
}

fn input(cx: &mut AsyncWindowContext, e: PlatformInput) {
    let _ = cx.update(|window, cx| {
        window.dispatch_event(e, cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn key(cx: &mut AsyncWindowContext, k: &str) {
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(Keystroke::parse(k).unwrap(), cx);
    });
    draw(cx);
}

fn move_to(cx: &mut AsyncWindowContext, p: Point<Pixels>) {
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: None, modifiers: Modifiers::default() }));
    // The eye shows on the frame after the move.
    draw(cx);
}

fn press(cx: &mut AsyncWindowContext, p: Point<Pixels>, clicks: usize) {
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: p, modifiers: Modifiers::default(), click_count: clicks, first_mouse: false }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: p, modifiers: Modifiers::default(), click_count: clicks }));
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn snap(name: &str, cx: &mut AsyncWindowContext) {
    let Ok(dir) = std::env::var("DATAMAN_SHOT_DIR") else { return };
    let path = format!("{dir}/value-{name}.png");
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
    });
}

fn eyes(grid: &Entity<ResultGrid>, cx: &mut AsyncWindowContext) -> Vec<((usize, usize), Point<Pixels>)> {
    cx.update(|_, cx| grid.read(cx).demo_eyes()).unwrap_or_default()
}

fn eye_on(grid: &Entity<ResultGrid>, cell: (usize, usize), cx: &mut AsyncWindowContext) -> Option<Point<Pixels>> {
    eyes(grid, cx).into_iter().find(|(c, _)| *c == cell).map(|(_, p)| p)
}

fn sheet(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<(String, bool, Vec<String>)> {
    cx.update(|_, cx| ws.read(cx).value_view.as_ref().map(|v| v.read(cx).demo_state())).ok().flatten()
}

pub async fn run(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let grid = cx.update(|_, cx| ws.read(cx).active_tab().unwrap().read(cx).grid.clone()).unwrap();
    for _ in 0..100 {
        if cx.update(|_, cx| grid.read(cx).result().is_some_and(|r| r.rows() >= 10)).unwrap_or(false) {
            break;
        }
        pause(cx, 100).await;
    }
    draw(cx);
    let at = |cx: &mut AsyncWindowContext, r: usize, c: usize| cx.update(|_, cx| grid.read(cx).demo_cell_point(r, c)).unwrap();

    let mut hovered = Vec::new();
    for c in 0..9 {
        if c == 5 {
            let _ = cx.update(|_, cx| grid.update(cx, |g, cx| g.set_scroll_x(f32::MAX, cx)));
            draw(cx);
        }
        let p = at(cx, 1, c);
        move_to(cx, p);
        hovered.push(eyes(&grid, cx).iter().any(|(cell, _)| *cell == (1, c)));
    }
    let _ = cx.update(|_, cx| grid.update(cx, |g, cx| g.set_scroll_x(0., cx)));
    draw(cx);
    report(
        "the hovered cell shows an eye on JSON objects and arrays, text holding one too (not plain text, NULL or a scalar)",
        hovered == [false, false, true, true, true, true, true, false, false],
        format!("{hovered:?}"),
    );
    let p = at(cx, 3, ADDRESS);
    move_to(cx, p);
    snap("hover", cx);
    move_to(cx, point(px(4.), px(4.)));
    report("the pointer gone, so is the eye", eyes(&grid, cx).is_empty(), format!("{:?}", eyes(&grid, cx)));

    let p = at(cx, 2, IDS);
    press(cx, p, 1);
    let p = at(cx, 6, 0);
    move_to(cx, p);
    report("the selected array cell keeps its eye", eye_on(&grid, (2, IDS), cx).is_some(), format!("{:?}", eyes(&grid, cx)));

    let p = at(cx, 1, ADDRESS);
    move_to(cx, p);
    let eye = eye_on(&grid, (1, ADDRESS), cx);
    if let Some(e) = eye {
        move_to(cx, e);
        press(cx, e, 1);
    }
    draw(cx);
    let s = sheet(ws, cx);
    let want = cx.update(|_, cx| grid.read(cx).value_at(1, ADDRESS).map(|c| c.value)).ok().flatten().unwrap_or_default();
    report(
        "a click on the eye opens the sheet with the cell's value, as a tree",
        eye.is_some() && s.as_ref().is_some_and(|(col, tree, lines)| col == "address" && *tree && lines.iter().any(|l| l.starts_with("zip: ")) && lines.iter().all(|l| want.contains(l.split(": ").nth(1).unwrap_or("")))),
        format!("eye {eye:?} {s:?} want {want}"),
    );
    let (sel, editing) = cx.update(|_, cx| (grid.read(cx).active_cell(), grid.read(cx).demo_editing())).unwrap();
    report("…and selects the cell, without an edit", sel == Some((1, ADDRESS)) && !editing, format!("{sel:?} editing {editing}"));
    snap("tree", cx);
    key(cx, "escape");
    let back = cx.update(|window, cx| grid.read(cx).focus_handle(cx).is_focused(window)).unwrap_or(false);
    report("esc closes it, the grid has the keys again", sheet(ws, cx).is_none() && back, format!("focused {back}"));

    let p = at(cx, 4, META);
    press(cx, p, 1);
    key(cx, "shift-space");
    let full = sheet(ws, cx).map_or(0, |s| s.2.len());
    let view = cx.update(|_, cx| ws.read(cx).value_view.clone()).ok().flatten();
    if let Some(v) = &view {
        let _ = cx.update(|_, cx| v.update(cx, |v, cx| v.demo_all(false, cx)));
    }
    draw(cx);
    snap("collapsed", cx);
    let collapsed = sheet(ws, cx).map_or(0, |s| s.2.len());
    if let Some(v) = &view {
        let _ = cx.update(|window, cx| v.update(cx, |v, cx| v.demo_text(true, window, cx)));
    }
    draw(cx);
    snap("text", cx);
    let text = sheet(ws, cx);
    report(
        "⇧Space on nested JSON: the tree, Collapse all down to the root's members, Text formatted",
        full == 7 && collapsed == 3 && text.as_ref().is_some_and(|(_, tree, lines)| !tree && lines.first().map(String::as_str) == Some("{") && lines.iter().any(|l| l.starts_with("    \"id\": 7"))),
        format!("full {full} collapsed {collapsed} {text:?}"),
    );
    key(cx, "escape");

    let p = at(cx, 2, IDS);
    press(cx, p, 1);
    key(cx, "shift-space");
    snap("list", cx);
    let s = sheet(ws, cx);
    report(
        "⇧Space on an array cell lists its items",
        s.as_ref().is_some_and(|(col, tree, lines)| col == "ids" && *tree && lines.len() == 3 && lines[2] == "2: NULL"),
        format!("{s:?}"),
    );
    key(cx, "shift-space");
    report("⇧Space again closes it", sheet(ws, cx).is_none(), String::new());

    let p = at(cx, 2, 1);
    press(cx, p, 1);
    key(cx, "shift-space");
    report("⇧Space on a text cell opens nothing", sheet(ws, cx).is_none(), String::new());

    let p = at(cx, 5, ADDRESS);
    move_to(cx, p);
    if let Some(e) = eye_on(&grid, (5, ADDRESS), cx) {
        move_to(cx, e);
        press(cx, e, 1);
        press(cx, e, 2);
    }
    let editing = cx.update(|_, cx| grid.read(cx).demo_editing()).unwrap();
    report("a double click on the eye opens the sheet, not the cell editor", sheet(ws, cx).is_some() && !editing, format!("editing {editing}"));
    press(cx, point(px(30.), px(300.)), 1);
    report("a click outside the sheet closes it", sheet(ws, cx).is_none(), String::new());

    println!("{}", if ok { "ALL PASS" } else { "SOME CHECKS FAILED" });
    ok
}
