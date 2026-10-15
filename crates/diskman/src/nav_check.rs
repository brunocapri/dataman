//! `fk-nav-check` (`--demo`): back / forward (`nav`) driven with real events through the window —
//! ⌘-click a reference and back to the very cell, ⌘← ⌘→, the mouse's side buttons, the title
//! bar's arrows, a new jump dropping what was ahead, a closed tab skipped, ⌘← in the editor
//! staying the line's start. PASS/FAIL lines like `fk-check`.

use std::time::Duration;

use gpui::{
    AsyncWindowContext, Entity, Focusable, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, NavigationDirection, PlatformInput, Pixels,
    Point, ScrollDelta, ScrollWheelEvent, TouchPhase, point, px,
};

use crate::app::{Region, Workspace};
use crate::grid::ResultGrid;
use crate::tab::QueryTab;
use crate::theme::metrics;

fn input(cx: &mut AsyncWindowContext, e: PlatformInput) {
    let _ = cx.update(|window, cx| {
        window.dispatch_event(e, cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn key(cx: &mut AsyncWindowContext, k: &str) {
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(Keystroke::parse(k).unwrap(), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn click(cx: &mut AsyncWindowContext, p: Point<Pixels>, button: MouseButton, modifiers: Modifiers) {
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button, position: p, modifiers, click_count: 1, first_mouse: false }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button, position: p, modifiers, click_count: 1 }));
}

fn side_button(cx: &mut AsyncWindowContext, p: Point<Pixels>, d: NavigationDirection) {
    click(cx, p, MouseButton::Navigate(d), Modifiers::default());
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn active(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Entity<QueryTab> {
    cx.update(|_, cx| ws.read(cx).active_tab().unwrap().clone()).unwrap()
}

fn region(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Region> {
    cx.update(|window, cx| ws.read(cx).focused_region(window, cx)).ok().flatten()
}

fn can(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> (bool, bool) {
    cx.update(|_, cx| (ws.read(cx).can_go(false), ws.read(cx).can_go(true))).unwrap()
}

fn spot(grid: &Entity<ResultGrid>, cx: &mut AsyncWindowContext) -> String {
    cx.update(|_, cx| format!("{:?}", grid.read(cx).spot())).unwrap()
}

/// The title bar's ‹ and › (chrome.rs: the explorer toggle 14 pt after the lights, 26 wide, then
/// 4 pt and two 20 pt arrows), on the bar's middle line.
fn arrow(forward: bool) -> Point<Pixels> {
    let lights = metrics::lights_right_at(crate::theme::zoom());
    point(px(lights + if forward { 74. } else { 54. }), px(metrics::TITLE_H / 2.))
}

pub async fn run(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let src = active(ws, cx);
    let grid = cx.update(|_, cx| src.read(cx).grid.clone()).unwrap();
    // The references of the result.
    let mut cols = None;
    for _ in 0..100 {
        cols = cx
            .update(|_, cx| {
                let g = grid.read(cx);
                let n = g.result().and_then(|r| r.columns()).map_or(0, |c| c.len());
                Some(((0..n).find(|&c| g.is_fk_column(c))?, (0..n).find(|&c| !g.is_fk_column(c))?))
            })
            .ok()
            .flatten();
        if cols.is_some() {
            break;
        }
        pause(cx, 100).await;
    }
    let Some((fk, plain)) = cols else {
        println!("FAIL no reference column was resolved");
        return false;
    };
    let at = |cx: &mut AsyncWindowContext, r: usize, c: usize| cx.update(|_, cx| grid.read(cx).demo_cell_point(r, c)).unwrap();
    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));

    // Scrolled down, a plain cell selected: the place left.
    let mid = at(cx, 2, plain);
    input(
        cx,
        PlatformInput::ScrollWheel(ScrollWheelEvent {
            position: mid,
            delta: ScrollDelta::Pixels(point(px(0.), px(-220.))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        }),
    );
    let p = at(cx, 14, plain);
    click(cx, p, MouseButton::Left, Modifiers::default());
    pause(cx, 50).await;
    report("nothing behind at the start", can(ws, cx) == (false, false), format!("{:?}", can(ws, cx)));

    // ⌘-click a reference: a new tab; the place left is the reference's cell, scrolled.
    let p = at(cx, 14, fk);
    click(cx, p, MouseButton::Left, Modifiers { platform: true, ..Default::default() });
    pause(cx, 400).await;
    let before = spot(&grid, cx);
    let fk_cell = format!("head: CellPos {{ row: 14, col: {fk} }}");
    report("the place left: the reference's cell, scrolled", before.contains(&fk_cell) && !before.contains("scroll: (0.0, 0.0)"), before.clone());
    let dst = active(ws, cx);
    report("⌘-click opens the referenced row in a new tab", dst != src, String::new());
    report("one step behind, none ahead", can(ws, cx) == (true, false), format!("{:?}", can(ws, cx)));

    // ⌘← (the focus on the new tab's grid or editor, not a text field's ⌘←).
    let g2 = cx.update(|_, cx| dst.read(cx).grid.clone()).unwrap();
    let _ = cx.update(|window, cx| window.focus(&g2.focus_handle(cx), cx));
    // The source grid drifting while away (it does not on its own): the step puts it back.
    let _ = cx.update(|_, cx| grid.update(cx, |g, cx| g.select_cell(0, plain, cx)));
    key(cx, "cmd-left");
    report("⌘← goes back to the source tab", active(ws, cx) == src, String::new());
    report("… with the grid focused", region(ws, cx) == Some(Region::Grid), format!("{:?}", region(ws, cx)));
    let after = spot(&grid, cx);
    report("… on the same cell and scroll", after == before, format!("{before} → {after}"));
    report("one step ahead", can(ws, cx) == (false, true), format!("{:?}", can(ws, cx)));

    key(cx, "cmd-right");
    report("⌘→ goes forward to the reference", active(ws, cx) == dst, String::new());

    // The mouse's side buttons, anywhere in the window.
    side_button(cx, mid, NavigationDirection::Back);
    report("the back button goes back", active(ws, cx) == src, String::new());
    side_button(cx, point(px(600.), px(metrics::TITLE_H / 2.)), NavigationDirection::Forward);
    report("the forward button goes forward", active(ws, cx) == dst, String::new());

    // The title bar's arrows.
    click(cx, arrow(false), MouseButton::Left, Modifiers::default());
    report("the ‹ arrow goes back", active(ws, cx) == src, String::new());
    click(cx, arrow(false), MouseButton::Left, Modifiers::default());
    report("the dimmed ‹ does nothing", active(ws, cx) == src, String::new());
    click(cx, arrow(true), MouseButton::Left, Modifiers::default());
    report("the › arrow goes forward", active(ws, cx) == dst, String::new());
    click(cx, arrow(false), MouseButton::Left, Modifiers::default());

    // A new jump after going back drops what was ahead.
    key(cx, "cmd-t");
    pause(cx, 50).await;
    let third = active(ws, cx);
    report("⌘T after going back: nothing ahead", third != src && third != dst && can(ws, cx) == (true, false), format!("{:?}", can(ws, cx)));

    // ⌘← in the editor is the line's start.
    key(cx, "s");
    key(cx, "e");
    key(cx, "l");
    let editor = cx.update(|_, cx| third.read(cx).editor.clone()).unwrap();
    key(cx, "cmd-left");
    let cur = cx.update(|_, cx| editor.read(cx).selection_ends().1).unwrap();
    report("⌘← in the editor: the line's start, no step", active(ws, cx) == third && cur == 0, format!("cursor {cur}"));
    // … while the side button still steps from there.
    side_button(cx, point(px(600.), px(300.)), NavigationDirection::Back);
    report("the back button from the editor", active(ws, cx) == src, String::new());
    side_button(cx, point(px(600.), px(300.)), NavigationDirection::Forward);
    report("… and forward to the editor", active(ws, cx) == third && region(ws, cx) == Some(Region::Editor), format!("{:?}", region(ws, cx)));

    // A closed tab is skipped: close the source, back from the third lands nowhere.
    let ix = cx.update(|_, cx| ws.read(cx).tabs.iter().position(|t| *t == src)).unwrap().unwrap();
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.close_tab(ix, window, cx)));
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.activate_tab(&third, window, cx)));
    // third → (activate) recorded the tab before; back skips the closed source.
    let before_back = active(ws, cx);
    side_button(cx, point(px(600.), px(300.)), NavigationDirection::Back);
    let now = active(ws, cx);
    report("a closed tab is never gone back to", now != src && cx.update(|_, cx| ws.read(cx).tabs.contains(&now)).unwrap(), format!("from {:?} to {:?}", before_back.entity_id(), now.entity_id()));

    println!("{}", if ok { "ALL PASS" } else { "SOME CHECKS FAILED" });
    ok
}
