//! `split-check` (`--demo`): two editor groups side by side, driven with real key and mouse
//! events through the window where a person would use them — ⌘\ opens a copy of the tab to the
//! side (the split growing in, then half and half, each strip over its group), a press in a group
//! gives it the focus and its Run button runs its own tab, the keyboard focus landing in the
//! other group moves the focus there, ⌃Tab and ⌘1 stay within the focused group, the seam
//! resizes the groups (a double-click evens them), a tab dragged onto the other strip moves
//! there, ⌥ held while a table opens from the explorer sends it to the other group, the session
//! keeps the groups across a relaunch, a group's × folds it away with its tabs joining the
//! other (the left one's as well as the right one's), and closing a group's last tab closes the
//! split. Also: no Open to the Side button on the strip; each strip scrolls its tabs sideways on
//! its own (the wheel, the tab in front kept in view); the connection picker of a narrow group
//! gives way instead of running over the History button; each group has its own cell viewer
//! (⇧Space in its grid, inside the group, resized by its edge, kept by the group that stays);
//! a tab dragged over a group's editor previews where it lands and dropped there moves into that
//! group or opens a group of its own at either side. PASS/FAIL lines like `dock-check`.
//!
//! Captures: `split` shows two groups with results in both, the right one focused;
//! `split-opening` catches ⌘\ on its way (`DATAMAN_SPLIT_FREEZE=0.5`). With `DATAMAN_SHOT_DIR`,
//! the check also captures `split-scrolled`, `split-narrow`, `split-viewers`, `split-drop-side`
//! and `split-drop-whole` there.

use std::sync::Arc;
use std::time::Duration;

use dataman_core::groups::Pane;
use dataman_core::session::SessionStore;
use gpui::{
    AsyncWindowContext, Bounds, Entity, Focusable, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels, Point,
    ScrollDelta, ScrollWheelEvent, TouchPhase, point,
};

use crate::app::Workspace;
use crate::split::DropZone;
use crate::restore::Sessions;
use crate::tab::QueryTab;
use crate::theme::{Pt, metrics, px};

pub fn is_stage(s: &str) -> bool {
    matches!(s, "split-check" | "split" | "split-opening")
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

fn key(cx: &mut AsyncWindowContext, k: &str) {
    draw(cx);
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(Keystroke::parse(k).unwrap(), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

/// `DATAMAN_SHOT_DIR`: a capture of the window as it is, `split-<name>.png`.
fn snap(name: &str, cx: &mut AsyncWindowContext) {
    let Ok(dir) = std::env::var("DATAMAN_SHOT_DIR") else { return };
    let path = format!("{dir}/split-{name}.png");
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        crate::bench::capture(window, &path, cx);
    });
}

fn at(x: f32, y: f32) -> Point<Pixels> {
    point(px(x), px(y))
}

fn mouse_move(cx: &mut AsyncWindowContext, p: Point<Pixels>, pressed: bool) {
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: pressed.then_some(MouseButton::Left), modifiers: Modifiers::default() }));
}

fn down(cx: &mut AsyncWindowContext, p: Point<Pixels>, click_count: usize) {
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: p, modifiers: Modifiers::default(), click_count, first_mouse: false }));
}

fn up(cx: &mut AsyncWindowContext, p: Point<Pixels>, click_count: usize) {
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: p, modifiers: Modifiers::default(), click_count }));
}

fn click(cx: &mut AsyncWindowContext, p: Point<Pixels>) {
    mouse_move(cx, p, false);
    down(cx, p, 1);
    up(cx, p, 1);
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

async fn wait(cx: &mut AsyncWindowContext, mut done: impl FnMut(&mut AsyncWindowContext) -> bool) -> bool {
    for _ in 0..200 {
        if done(cx) {
            return true;
        }
        pause(cx, 50).await;
    }
    false
}

/// `tab`'s run has ended.
fn finished(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|_, cx| tab.read(cx).run_phase().is_some_and(|p| p.is_finished())).unwrap_or(true)
}

fn tabs(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Vec<Entity<QueryTab>> {
    cx.update(|_, cx| ws.read(cx).tabs.clone()).unwrap_or_default()
}

fn active(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Entity<QueryTab>> {
    cx.update(|_, cx| ws.read(cx).active_tab().cloned()).ok().flatten()
}

fn focused(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Pane> {
    cx.update(|_, cx| ws.read(cx).split.as_ref().map(|s| s.focus)).ok().flatten()
}

/// The right group's tabs.
fn right(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Vec<Entity<QueryTab>> {
    cx.update(|_, cx| {
        let w = ws.read(cx);
        w.group(Pane::Right).into_iter().map(|i| w.tabs[i].clone()).collect()
    })
    .unwrap_or_default()
}

/// The groups as GPUI laid them out (left, right), in points: x and width.
fn panes(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<((f32, f32), (f32, f32))> {
    draw(cx);
    cx.update(|_, cx| {
        let b = ws.read(cx).pane_bounds.borrow().clone();
        // The left group, the hairline, the right group (the grabber after them).
        let (l, r) = (b.first()?, b.get(2)?);
        Some(((l.origin.x.pt(), l.size.width.pt()), (r.origin.x.pt(), r.size.width.pt())))
    })
    .ok()
    .flatten()
}

fn center(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> crate::dock::Rect {
    cx.update(|_, cx| ws.read(cx).frame_now.center).unwrap()
}

/// The centre of the `i`th tab of a group's strip, as last laid out.
fn tab_mid(ws: &Entity<Workspace>, pane: Pane, i: usize, cx: &mut AsyncWindowContext) -> Option<Point<Pixels>> {
    draw(cx);
    let b = cx
        .update(|_, cx| {
            let w = ws.read(cx);
            let b = if pane == Pane::Left { w.tab_bounds.borrow() } else { w.tab_bounds_right.borrow() };
            b.get(i).copied()
        })
        .ok()
        .flatten()?;
    Some(point(b.left() + b.size.width / 2., b.top() + b.size.height / 2.))
}

fn near(a: f32, b: f32, tol: f32) -> bool {
    (a - b).abs() <= tol
}

fn middle(b: Bounds<Pixels>) -> Point<Pixels> {
    point(b.left() + b.size.width / 2., b.top() + b.size.height / 2.)
}

/// A group's strip as last laid out: its tabs (scrolling), +, the room after them and, with two
/// groups, the group's ×.
fn parts(ws: &Entity<Workspace>, pane: Pane, cx: &mut AsyncWindowContext) -> Vec<Bounds<Pixels>> {
    draw(cx);
    cx.update(|_, cx| ws.read(cx).strip_parts.borrow()[pane.index()].clone()).unwrap_or_default()
}

/// A group's strip: how far its tabs are scrolled (≤ 0), how far they can be, and where they
/// show.
fn strip_scroll(ws: &Entity<Workspace>, pane: Pane, cx: &mut AsyncWindowContext) -> (f32, f32, Bounds<Pixels>) {
    cx.update(|_, cx| {
        let h = &ws.read(cx).strip_scroll[pane.index()];
        (h.offset().x.pt(), h.max_offset().x.pt(), h.bounds())
    })
    .unwrap()
}

fn wheel(cx: &mut AsyncWindowContext, p: Point<Pixels>, dy: f32) {
    mouse_move(cx, p, false);
    input(cx, PlatformInput::ScrollWheel(ScrollWheelEvent { position: p, delta: ScrollDelta::Pixels(point(px(0.), px(dy))), modifiers: Modifiers::default(), touch_phase: TouchPhase::Moved }));
}

/// A press on `from`, the pointer along `path` with the button held (a frame drawn at each
/// step); the release is the caller's.
fn drag_along(cx: &mut AsyncWindowContext, from: Point<Pixels>, path: &[Point<Pixels>]) {
    mouse_move(cx, from, false);
    down(cx, from, 1);
    for &p in path {
        mouse_move(cx, p, true);
        draw(cx);
    }
}

fn drop_now(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<(Pane, DropZone)> {
    cx.update(|_, cx| ws.read(cx).editor_drop).ok().flatten()
}

fn viewer_open(ws: &Entity<Workspace>, pane: Pane, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|_, cx| ws.read(cx).viewer_open(pane)).unwrap_or(false)
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        if stage == "split-check" {
            println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        }
        ok &= pass;
    };
    // The demo's tab ran its query.
    let first = tabs(ws, cx)[0].clone();
    wait(cx, |cx| finished(&first, cx)).await;
    let reduced = cx.update(|_, cx| cx.reduce_motion()).unwrap();
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.focus_editor(window, cx)));
    let one = parts(ws, Pane::Left, cx);
    report("one group: no Open to the Side button at the strip's end (tabs, +, room)", one.len() == 3, format!("{} parts", one.len()));

    // ⌘\: a copy of the tab in a second group, growing in from the right.
    key(cx, "cmd-\\");
    let all = tabs(ws, cx);
    let copy = all.last().cloned().unwrap();
    let same = cx
        .update(|_, cx| {
            let (a, b) = (first.read(cx), copy.read(cx));
            a.editor.read(cx).text() == b.editor.read(cx).text()
                && a.connection().map(|c| c.profile.id.clone()) == b.connection().map(|c| c.profile.id.clone())
                && a.database() == b.database()
                && !b.has_run
        })
        .unwrap();
    report(
        "⌘\\ opens a copy to the side: same script, connection and database, a session of its own",
        all.len() == 2 && copy != first && same && right(ws, cx) == [copy.clone()],
        format!("{} tabs, right {:?}", all.len(), right(ws, cx).len()),
    );
    report("the copy is in front with the focus; the tab stays as it was", active(ws, cx).as_ref() == Some(&copy) && focused(ws, cx) == Some(Pane::Right) && cx.update(|_, cx| first.read(cx).has_run).unwrap(), format!("{:?}", focused(ws, cx)));
    if stage == "split-opening" {
        return ok;
    }
    let moving = cx.update(|_, cx| ws.read(cx).split_move.is_some()).unwrap();
    pause(cx, 60).await;
    let mid = panes(ws, cx);
    let c = center(ws, cx);
    report(
        "the new group grows in from the right while the first narrows",
        reduced || (moving && mid.is_some_and(|((_, lw), (rx, rw))| rw > 1. && rw < c.w / 2. - 10. && lw > c.w / 2. && near(rx + rw, c.right(), 1.5))),
        format!("{mid:?} in {c:?}"),
    );
    pause(cx, 400).await;
    let open = panes(ws, cx);
    let half = (c.w - crate::split::SEAM) / 2.;
    report(
        "settled: half and half around the seam",
        open.is_some_and(|((lx, lw), (rx, rw))| near(lx, c.x, 1.) && near(lw, half, 1.5) && near(rw, half, 1.5) && near(rx, lx + lw + crate::split::SEAM, 1.5)),
        format!("{open:?} in {c:?}"),
    );
    let (l0, r0) = (tab_mid(ws, Pane::Left, 0, cx), tab_mid(ws, Pane::Right, 0, cx));
    let seam_x = open.map_or(0., |((lx, lw), _)| lx + lw);
    report(
        "each group's tabs sit in a strip over it",
        l0.zip(r0).is_some_and(|(l, r)| l.x.pt() < seam_x && r.x.pt() > seam_x),
        format!("{l0:?} {r0:?} seam {seam_x}"),
    );

    // A press in the left group gives it the focus; the right group's Run runs its own tab.
    let editor_y = c.y + metrics::TOOLBAR_H + 60.;
    click(cx, at(c.x + 120., editor_y));
    report("a press in the left group gives it the focus", focused(ws, cx) == Some(Pane::Left) && active(ws, cx).as_ref() == Some(&first), format!("{:?}", focused(ws, cx)));
    let (rx, _) = open.map(|(_, r)| r).unwrap_or_default();
    let _ = cx.update(|_, cx| copy.update(cx, |t, cx| t.editor.update(cx, |e, cx| e.set_text("SELECT 'right' AS side, g FROM generate_series(1, 3) g;", cx))));
    click(cx, at(rx + 30., c.y + metrics::TOOLBAR_H / 2.));
    let ran = wait(cx, |cx| finished(&copy, cx)).await;
    let rows = cx.update(|_, cx| copy.read(cx).result().map(|r| r.rows())).ok().flatten();
    let left_rows = cx.update(|_, cx| first.read(cx).result().map(|r| r.rows())).ok().flatten();
    report(
        "the right group's Run runs the right group's tab (and gives it the focus)",
        ran && rows == Some(3) && focused(ws, cx) == Some(Pane::Right) && left_rows.is_some_and(|n| n != 3),
        format!("rows {rows:?} left {left_rows:?} focus {:?}", focused(ws, cx)),
    );

    // Each group has its own cell viewer: ⇧Space in the right group's grid opens the right
    // one, inside the group, about that grid's cell; the left group's stays shut.
    pause(cx, 300).await;
    let result = cx.update(|_, cx| ws.read(cx).result_bounds.get()).ok().flatten();
    if let Some(r) = result {
        click(cx, point(r.left() + px(60.), r.top() + px(metrics::RESULT_BAR_H + 50.)));
    }
    let grid_focused = cx.update(|window, cx| copy.read(cx).grid.focus_handle(cx).contains_focused(window, cx)).unwrap_or(false);
    key(cx, "shift-space");
    pause(cx, 320).await;
    draw(cx);
    let right_pane = panes(ws, cx).map(|(_, r)| r);
    // Its width, leaving the group its least room for the editor and results.
    let vw = cx.update(|_, cx| ws.read(cx).viewers[1].w).unwrap().min(right_pane.map_or(0., |(_, rw)| rw - crate::cell_viewer::MAIN_MIN));
    let result = cx.update(|_, cx| ws.read(cx).result_bounds.get()).ok().flatten();
    report(
        "⇧Space in the right group's grid opens the right group's cell viewer, not the left's",
        grid_focused && viewer_open(ws, Pane::Right, cx) && !viewer_open(ws, Pane::Left, cx) && cx.update(|_, cx| ws.read(cx).side.is_none()).unwrap(),
        format!("grid focused {grid_focused}, right {} left {}", viewer_open(ws, Pane::Right, cx), viewer_open(ws, Pane::Left, cx)),
    );
    report(
        "the viewer sits inside its group, at its right: the results end where it starts",
        right_pane.zip(result).is_some_and(|((rx, rw), r)| near(r.right().pt(), rx + rw - vw, 2.)),
        format!("{right_pane:?} {result:?} viewer {vw}"),
    );
    // Its left edge resizes it, with a real drag (narrower: wider it would take the group's
    // least room).
    if let Some((rx, rw)) = right_pane {
        let edge = at(rx + rw - vw, c.y + c.h / 2.);
        drag_along(cx, edge, &[at(edge.x.pt() + 20., edge.y.pt()), at(edge.x.pt() + 60., edge.y.pt())]);
        up(cx, at(edge.x.pt() + 60., edge.y.pt()), 1);
    }
    pause(cx, 50).await;
    let vw2 = cx.update(|_, cx| ws.read(cx).viewers[1].w).unwrap();
    let result2 = cx.update(|_, cx| ws.read(cx).result_bounds.get()).ok().flatten();
    report(
        "dragging the viewer's edge resizes it (the results take the room it gives)",
        near(vw2, vw - 60., 1.5) && result.zip(result2).is_some_and(|(a, b)| near(b.right().pt(), a.right().pt() + 60., 2.)),
        format!("{vw} → {vw2}"),
    );
    // The left group's own: opened from its grid, the right one stays as it is.
    let l = panes(ws, cx).map(|(l, _)| l);
    if let Some((lx, _)) = l {
        click(cx, at(lx + 60., c.y + c.h - 120.));
    }
    key(cx, "shift-space");
    pause(cx, 320).await;
    snap("viewers", cx);
    report(
        "the left group opens its own viewer; both are open, each in its group",
        viewer_open(ws, Pane::Left, cx) && viewer_open(ws, Pane::Right, cx) && focused(ws, cx) == Some(Pane::Left),
        format!("left {} right {} focus {:?}", viewer_open(ws, Pane::Left, cx), viewer_open(ws, Pane::Right, cx), focused(ws, cx)),
    );
    key(cx, "shift-space");
    report("⇧Space again shuts the left one only", !viewer_open(ws, Pane::Left, cx) && viewer_open(ws, Pane::Right, cx), String::new());
    // The right one shut again for what follows.
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.toggle_viewer(Pane::Right, cx)));
    pause(cx, 320).await;

    // The keyboard focus landing in the other group moves the focus there.
    let _ = cx.update(|window, cx| {
        let f = first.read(cx).editor.focus_handle(cx);
        window.focus(&f, cx);
    });
    draw(cx);
    report("the keys landing in the other group give it the focus", focused(ws, cx) == Some(Pane::Left) && active(ws, cx).as_ref() == Some(&first), format!("{:?}", focused(ws, cx)));

    // ⌘T opens in the focused group; ⌃Tab and ⌘1 stay within it.
    key(cx, "cmd-t");
    let third = active(ws, cx).unwrap();
    let left_group = cx.update(|_, cx| ws.read(cx).group(Pane::Left).len()).unwrap();
    report("⌘T opens a tab in the focused group", left_group == 2 && right(ws, cx) == [copy.clone()] && focused(ws, cx) == Some(Pane::Left), format!("left {left_group}"));
    key(cx, "ctrl-tab");
    let after_tab = active(ws, cx);
    key(cx, "cmd-1");
    let after_one = active(ws, cx);
    report(
        "⌃Tab and ⌘1 stay within the focused group",
        after_tab.as_ref() == Some(&first) && after_one.as_ref() == Some(&first) && focused(ws, cx) == Some(Pane::Left),
        format!("{:?} {:?}", after_tab.map(|t| t.entity_id()), after_one.map(|t| t.entity_id())),
    );

    // Each strip scrolls its tabs on its own: six more tabs in the left group overflow its
    // strip; the tab in front (the last) is scrolled into view, the wheel scrolls them back, ⌘1
    // brings the first into view; the right strip does not move.
    for _ in 0..6 {
        key(cx, "cmd-t");
    }
    pause(cx, 50).await;
    draw(cx);
    let (x0, max0, view) = strip_scroll(ws, Pane::Left, cx);
    let last = tab_mid(ws, Pane::Left, 7, cx);
    snap("scrolled", cx);
    report(
        "eight tabs overflow the left strip: it scrolls, the tab in front (the last) in view",
        max0 > 50. && x0 < -1. && last.is_some_and(|p| p.x.pt() < view.right().pt() && p.x.pt() > view.left().pt()),
        format!("offset {x0} of {max0}, last {last:?} in {view:?}"),
    );
    let right0 = strip_scroll(ws, Pane::Right, cx).0;
    wheel(cx, middle(view), 120.);
    let x1 = strip_scroll(ws, Pane::Left, cx).0;
    report("the wheel (a vertical turn) scrolls the strip sideways", x1 > x0 + 50., format!("{x0} → {x1}"));
    report("the other group's strip stays where it was", strip_scroll(ws, Pane::Right, cx).0 == right0, String::new());
    wheel(cx, middle(view), -400.);
    key(cx, "cmd-1");
    draw(cx);
    let (x2, _, _) = strip_scroll(ws, Pane::Left, cx);
    report("⌘1: the first tab comes to the front, scrolled into view", near(x2, 0., 0.5) && active(ws, cx).as_ref() == Some(&first), format!("{x2}"));
    // Back as it was: the six closed (the last one in front each time).
    key(cx, "cmd-9");
    for _ in 0..6 {
        key(cx, "cmd-w");
    }
    let left_n = cx.update(|_, cx| ws.read(cx).group(Pane::Left).len()).unwrap();
    report("the extra tabs closed", left_n == 2 && active(ws, cx).as_ref() == Some(&third), format!("left {left_n}"));
    key(cx, "cmd-1");

    // The seam resizes the groups; a double-click evens them.
    let y = c.y + c.h / 2.;
    mouse_move(cx, at(seam_x, y), false);
    down(cx, at(seam_x, y), 1);
    for x in [seam_x - 60., seam_x - 140., seam_x - 200.] {
        mouse_move(cx, at(x, y), true);
    }
    up(cx, at(seam_x - 200., y), 1);
    let resized = panes(ws, cx);
    report(
        "dragging the seam resizes the groups",
        resized.is_some_and(|((lx, lw), (_, rw))| near(lx + lw, seam_x - 200., 2.) && rw > half + 150.),
        format!("{resized:?}"),
    );
    // Narrower still (the least a group keeps): the connection picker gives way, ending before
    // the History button.
    let x = resized.map_or(seam_x, |((lx, lw), _)| lx + lw);
    drag_along(cx, at(x, y), &[at(x - 40., y), at(c.x, y)]);
    up(cx, at(c.x, y), 1);
    let narrow = panes(ws, cx);
    snap("narrow", cx);
    let picker = cx.update(|_, cx| ws.read(cx).picker_bounds.get()).ok().flatten();
    let tb = cx.update(|_, cx| ws.read(cx).toolbar_parts.borrow()[0].clone()).unwrap_or_default();
    let history = tb.iter().rev().find(|b| b.size.width > px(10.)).copied();
    report(
        "in a narrow group the connection picker shrinks, never over the History button",
        picker.zip(history).is_some_and(|(p, h)| p.right() <= h.left() && p.size.width > px(40.)),
        format!("picker {picker:?} history {history:?} in {narrow:?}"),
    );
    let resized = narrow;
    let fp = cx.update(|_, cx| ws.read(cx).session_fingerprint(cx)).unwrap();
    let x = resized.map_or(seam_x, |((lx, lw), _)| lx + lw);
    mouse_move(cx, at(x, y), false);
    down(cx, at(x, y), 1);
    up(cx, at(x, y), 1);
    down(cx, at(x, y), 2);
    up(cx, at(x, y), 2);
    let even = panes(ws, cx);
    report("a double-click on the seam evens them", even.is_some_and(|((_, lw), (_, rw))| near(lw, rw, 1.5)), format!("{even:?}"));
    report("the session notices", cx.update(|_, cx| ws.read(cx).session_fingerprint(cx)).unwrap() != fp, String::new());

    // A tab dragged onto the other strip moves there (the session and results with it).
    let from = tab_mid(ws, Pane::Left, 1, cx);
    let to = tab_mid(ws, Pane::Right, 0, cx);
    if let (Some(p), Some(q)) = (from, to) {
        mouse_move(cx, p, false);
        down(cx, p, 1);
        for s in [point(p.x + px(8.), p.y), point(q.x - px(40.), q.y), point(q.x + px(30.), q.y)] {
            mouse_move(cx, s, true);
            draw(cx);
        }
        up(cx, point(q.x + px(30.), q.y), 1);
    }
    pause(cx, 100).await;
    report(
        "a tab dragged onto the other strip moves into that group, after the tab it passed",
        right(ws, cx) == [copy.clone(), third.clone()] && active(ws, cx).as_ref() == Some(&third) && focused(ws, cx) == Some(Pane::Right) && tabs(ws, cx).len() == 3,
        format!("right {:?}", right(ws, cx).iter().map(|t| t.entity_id()).collect::<Vec<_>>()),
    );

    // ⌥ held while a table opens from the explorer: it opens in the other group.
    let conn = cx.update(|_, cx| first.read(cx).connection().map(|c| (c.profile.id.clone(), first.read(cx).database().to_string()))).ok().flatten();
    if let Some((id, db)) = conn {
        let alt = Modifiers { alt: true, ..Default::default() };
        input(cx, PlatformInput::ModifiersChanged(gpui::ModifiersChangedEvent { modifiers: alt, capslock: Default::default() }));
        let p = at(c.x + 40., editor_y);
        input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: p, modifiers: alt, click_count: 1, first_mouse: false }));
        input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: p, modifiers: alt, click_count: 1 }));
        // The press landed in the left group: the table goes to the right one.
        let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
        let _ = cx.update(|_, cx| tree.update(cx, |_, cx| cx.emit(crate::tree::TreeEvent::OpenRelation { conn: id, db, schema: "public".into(), name: "orders".into() })));
        input(cx, PlatformInput::ModifiersChanged(gpui::ModifiersChangedEvent { modifiers: Modifiers::default(), capslock: Default::default() }));
        draw(cx);
        let opened = active(ws, cx);
        report(
            "⌥ held while a table opens from the explorer: it opens in the other group",
            opened.as_ref().is_some_and(|t| right(ws, cx).last() == Some(t)) && focused(ws, cx) == Some(Pane::Right) && tabs(ws, cx).len() == 4,
            format!("{} tabs, right {}", tabs(ws, cx).len(), right(ws, cx).len()),
        );
        // Without ⌥ it opens in the focused group, as before.
        let _ = cx.update(|_, cx| ws.update(cx, |w, _| w.alt_press = false));
        if let Some(t) = opened {
            let ix = tabs(ws, cx).iter().position(|x| *x == t).unwrap_or(0);
            let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.close_tab(ix, window, cx)));
        }
    }

    if stage == "split" {
        // The left group's tab runs too, the right group keeps the focus: the capture.
        let _ = cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                if let Some(ix) = w.tabs.iter().position(|t| *t == copy) {
                    w.activate(ix, window, cx);
                }
            })
        });
        pause(cx, 300).await;
        return ok;
    }

    // The session keeps the groups: saved, the workspace rebuilt as at launch.
    let dir = std::env::temp_dir().join(format!("dataman-split-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(SessionStore::new(&dir));
    let _ = cx.update(|_, cx| cx.set_global(Sessions(Some(store.clone()))));
    let _ = cx.update(|_, cx| ws.update(cx, |w, _| w.split.as_mut().map(|s| s.ratio = 0.4)));
    let texts = |ws: &Entity<Workspace>, pane: Pane, cx: &mut AsyncWindowContext| -> Vec<String> {
        cx.update(|_, cx| {
            let w = ws.read(cx);
            w.group(pane).into_iter().map(|i| w.tabs[i].read(cx).editor.read(cx).text().to_string()).collect()
        })
        .unwrap()
    };
    let before = (texts(ws, Pane::Left, cx), texts(ws, Pane::Right, cx), active(ws, cx).map(|t| cx.update(|_, cx| t.read(cx).editor.read(cx).text().to_string()).unwrap()));
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.save_session(true, cx)));
    let core = cx.update(|_, cx| ws.read(cx).store.read(cx).core.clone()).unwrap();
    let _ = cx.update(|_, cx| {
        ws.update(cx, |w, _| {
            w.session_off = true;
            w.session_task = None;
        })
    });
    let ws2 = cx.update(|window, cx| window.replace_root(cx, |window, cx| Workspace::new(core, window, cx))).unwrap();
    pause(cx, 100).await;
    draw(cx);
    let after = (texts(&ws2, Pane::Left, cx), texts(&ws2, Pane::Right, cx), active(&ws2, cx).map(|t| cx.update(|_, cx| t.read(cx).editor.read(cx).text().to_string()).unwrap()));
    let ratio = cx.update(|_, cx| ws2.read(cx).split.as_ref().map(|s| s.ratio)).ok().flatten();
    report(
        "relaunched: the same tabs in the same groups, the focused one in front, the width kept",
        after == before && ratio == Some(0.4) && focused(&ws2, cx) == Some(Pane::Right),
        format!("{after:?} vs {before:?}, ratio {ratio:?}"),
    );
    let _ = cx.update(|_, cx| cx.set_global(Sessions(None)));
    let _ = std::fs::remove_dir_all(&dir);
    let ws = &ws2;
    pause(cx, 300).await;

    // The right group's ×: its tabs join the left group, none is closed, the left keeps the
    // focus.
    let n = tabs(ws, cx).len();
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.focus_pane(Pane::Left, window, cx)));
    let left_front = active(ws, cx);
    let open = panes(ws, cx);
    // The × is the strip's last part (22 pt), at its end.
    let strip_end = parts(ws, Pane::Right, cx).last().copied();
    if let Some(b) = strip_end {
        click(cx, middle(b));
    }
    let folding = cx.update(|_, cx| ws.read(cx).split.is_none() && (ws.read(cx).split_move.is_some() || cx.reduce_motion())).unwrap();
    report(
        "a group's × closes the split; its tabs join the other group, none closed",
        folding && tabs(ws, cx).len() == n && active(ws, cx) == left_front,
        format!("{} of {n} tabs, split {:?}", tabs(ws, cx).len(), cx.update(|_, cx| ws.read(cx).split.is_some()).unwrap()),
    );
    pause(cx, 60).await;
    draw(cx);
    let mid = cx.update(|_, cx| ws.read(cx).split_now).ok().flatten();
    report("it folds away to the right", reduced || mid.is_some_and(|f| f.open > 0. && f.open < 1. && f.leaving == Pane::Right), format!("{mid:?}"));
    pause(cx, 400).await;
    draw(cx);
    let one = cx.update(|_, cx| ws.read(cx).split_now.is_none()).unwrap();
    let full = cx.update(|_, cx| ws.read(cx).result_bounds.get().map(|b| b.size.width.pt())).ok().flatten();
    let c = center(ws, cx);
    report("then one group has the whole surface", one && full.is_none_or(|w| near(w, c.w, 1.5)), format!("{full:?} vs {}", c.w));
    let _ = open;

    // The left group's ×: the LEFT group closes (whichever group has the focus), its tabs join
    // the right one, whose tab stays in front, with its cell viewer.
    let _ = cx.update(|_, cx| ws.update(cx, |w, _| w.focus_visible = false));
    key(cx, "cmd-\\");
    pause(cx, 300).await;
    let n = tabs(ws, cx).len();
    let copy = active(ws, cx).unwrap();
    report("⌘\\ again: two groups", right(ws, cx) == [copy.clone()], format!("{}", right(ws, cx).len()));
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.toggle_viewer(Pane::Right, cx)));
    pause(cx, 300).await;
    let seam = panes(ws, cx).map_or(0., |((lx, lw), _)| lx + lw);
    let left_x = parts(ws, Pane::Left, cx).last().copied();
    if let Some(b) = left_x {
        click(cx, middle(b));
    }
    let leaving = cx.update(|_, cx| ws.read(cx).split_move.as_ref().map(|m| m.leaving)).ok().flatten();
    report(
        "the left group's × (left of the seam) closes the left group; the right group's tab stays in front",
        left_x.is_some_and(|b| b.right().pt() <= seam)
            && cx.update(|_, cx| ws.read(cx).split.is_none()).unwrap()
            && tabs(ws, cx).len() == n
            && active(ws, cx).as_ref() == Some(&copy)
            && (reduced || leaving == Some(Pane::Left)),
        format!("× {left_x:?} seam {seam}, leaving {leaving:?}, active is the copy: {}", active(ws, cx).as_ref() == Some(&copy)),
    );
    report("the group that stays keeps its cell viewer", viewer_open(ws, Pane::Left, cx) && !viewer_open(ws, Pane::Right, cx), String::new());
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.toggle_viewer(Pane::Left, cx)));
    pause(cx, 400).await;
    draw(cx);
    // The focused group's × closes it too (the right one, focused).
    key(cx, "cmd-\\");
    pause(cx, 300).await;
    let copy2 = active(ws, cx).unwrap();
    let left_front = cx.update(|_, cx| ws.read(cx).front_of(Pane::Left).cloned()).ok().flatten();
    if let Some(b) = parts(ws, Pane::Right, cx).last().copied() {
        click(cx, middle(b));
    }
    report(
        "the focused right group's × closes the right group; the left group's tab is in front",
        cx.update(|_, cx| ws.read(cx).split.is_none()).unwrap() && tabs(ws, cx).len() == n + 1 && active(ws, cx) == left_front && tabs(ws, cx).contains(&copy2),
        format!("{} tabs", tabs(ws, cx).len()),
    );
    pause(cx, 400).await;
    if let Some(ix) = tabs(ws, cx).iter().position(|t| *t == copy2) {
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.close_tab(ix, window, cx)));
    }
    if let Some(ix) = tabs(ws, cx).iter().position(|t| *t == copy) {
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.activate(ix, window, cx)));
    }
    draw(cx);

    // Closing a group's last tab closes the split.
    key(cx, "cmd-\\");
    pause(cx, 300).await;
    let n = tabs(ws, cx).len();
    let copy3 = active(ws, cx).unwrap();
    report("⌘\\ once more: two groups", right(ws, cx) == [copy3.clone()], format!("{}", right(ws, cx).len()));
    key(cx, "cmd-w");
    report(
        "closing a group's last tab closes the split; the other group's tab is in front",
        cx.update(|_, cx| ws.read(cx).split.is_none()).unwrap() && tabs(ws, cx).len() == n - 1 && !tabs(ws, cx).contains(&copy3) && active(ws, cx).as_ref() == Some(&copy),
        format!("{} tabs", tabs(ws, cx).len()),
    );
    pause(cx, 400).await;
    ok &= drop_checks(ws, reduced, cx).await;
    ok
}

/// A tab dragged over the editor: the preview of where it lands, and the drop.
async fn drop_checks(ws: &Entity<Workspace>, reduced: bool, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let c = center(ws, cx);
    let y = c.y + c.h / 2.;
    let n = tabs(ws, cx).len();
    // One group: the first tab dragged over the editor's right third, its middle, its left third.
    let Some(from) = tab_mid(ws, Pane::Left, 0, cx) else {
        report("a tab to drag", false, String::new());
        return ok;
    };
    let dragged = tabs(ws, cx)[0].clone();
    let right_third = at(c.x + c.w * 0.85, y);
    drag_along(cx, from, &[point(from.x + px(10.), from.y + px(10.)), at(c.x + c.w * 0.6, y), right_third]);
    let side = drop_now(ws, cx);
    pause(cx, 250).await;
    snap("drop-side", cx);
    report("over the only group's right third: its right half is previewed", side == Some((Pane::Left, DropZone::Side(Pane::Right))), format!("{side:?}"));
    mouse_move(cx, at(c.x + c.w * 0.5, y), true);
    draw(cx);
    let whole = drop_now(ws, cx);
    let glide = cx.update(|_, cx| ws.read(cx).editor_drop_glide.is_some()).unwrap();
    report("its middle: the whole group, the preview gliding there", whole == Some((Pane::Left, DropZone::Whole)) && (reduced || glide), format!("{whole:?} glide {glide}"));
    mouse_move(cx, at(c.x + c.w * 0.1, y), true);
    draw(cx);
    report("its left third: its left half", drop_now(ws, cx) == Some((Pane::Left, DropZone::Side(Pane::Left))), format!("{:?}", drop_now(ws, cx)));
    // Dropped on the right third: a group of its own at the right, growing in.
    mouse_move(cx, right_third, true);
    draw(cx);
    up(cx, right_third, 1);
    let moving = cx.update(|_, cx| ws.read(cx).split_move.as_ref().map(|m| m.leaving)).ok().flatten();
    report(
        "dropped there: the tab moves into a new right group, which grows in like Open to the Side",
        right(ws, cx) == [dragged.clone()] && active(ws, cx).as_ref() == Some(&dragged) && focused(ws, cx) == Some(Pane::Right) && tabs(ws, cx).len() == n && (reduced || moving == Some(Pane::Right)),
        format!("right {} moving {moving:?}", right(ws, cx).len()),
    );
    report("no preview once dropped", drop_now(ws, cx).is_none(), String::new());
    pause(cx, 400).await;
    // Two groups: a left tab dragged over the right group's editor lights all of it; dropped,
    // it moves into the right group.
    let open = panes(ws, cx);
    let Some(((lx, lw), (rx, rw))) = open else {
        report("two groups laid out", false, String::new());
        return ok;
    };
    let mover = cx.update(|_, cx| ws.read(cx).group(Pane::Left).first().map(|&i| ws.read(cx).tabs[i].clone())).ok().flatten().unwrap();
    let from = tab_mid(ws, Pane::Left, 0, cx).unwrap();
    let over = at(rx + rw * 0.3, y);
    drag_along(cx, from, &[point(from.x + px(10.), from.y + px(10.)), at(lx + lw / 2., y), over]);
    let own = cx.update(|_, cx| ws.read(cx).editor_drop).ok().flatten();
    snap("drop-whole", cx);
    report("over the other group's editor: all of it is previewed", own == Some((Pane::Right, DropZone::Whole)), format!("{own:?}"));
    up(cx, over, 1);
    pause(cx, 100).await;
    report(
        "dropped there: it moves into that group, in front, with the focus",
        right(ws, cx).last() == Some(&mover) && right(ws, cx).len() == 2 && active(ws, cx).as_ref() == Some(&mover) && focused(ws, cx) == Some(Pane::Right),
        format!("right {}", right(ws, cx).len()),
    );
    // Over its own group's editor (beside another one): nothing.
    let from = tab_mid(ws, Pane::Right, 0, cx).unwrap();
    drag_along(cx, from, &[point(from.x + px(10.), from.y + px(10.)), at(rx + rw / 2., y)]);
    report("over its own group beside another: no preview", drop_now(ws, cx).is_none(), format!("{:?}", drop_now(ws, cx)));
    up(cx, at(rx + rw / 2., y), 1);
    // The right group's × gives one group again; a tab dropped on the left third opens a group
    // of its own at the left, the others staying on the right.
    if let Some(b) = parts(ws, Pane::Right, cx).last().copied() {
        click(cx, middle(b));
    }
    pause(cx, 400).await;
    let n = tabs(ws, cx).len();
    let front = active(ws, cx).unwrap();
    let last = tabs(ws, cx).last().cloned().unwrap();
    let from = tab_mid(ws, Pane::Left, n - 1, cx).unwrap();
    let left_third = at(c.x + c.w * 0.1, y);
    drag_along(cx, from, &[point(from.x + px(10.), from.y + px(10.)), at(c.x + c.w * 0.5, y), left_third]);
    up(cx, left_third, 1);
    let moving = cx.update(|_, cx| ws.read(cx).split_move.as_ref().map(|m| m.leaving)).ok().flatten();
    let left_group = cx.update(|_, cx| ws.read(cx).group(Pane::Left).len()).unwrap();
    let right_front = cx.update(|_, cx| ws.read(cx).front_of(Pane::Right).cloned()).ok().flatten();
    report(
        "dropped on the only group's left third: a group of its own at the left, growing in; the others at the right",
        left_group == 1
            && tabs(ws, cx).first() == Some(&last)
            && active(ws, cx).as_ref() == Some(&last)
            && focused(ws, cx) == Some(Pane::Left)
            && right(ws, cx).len() == n - 1
            && (front == last || right_front.as_ref() == Some(&front))
            && (reduced || moving == Some(Pane::Left)),
        format!("left {left_group}, right {}, moving {moving:?}", right(ws, cx).len()),
    );
    pause(cx, 400).await;
    let open = panes(ws, cx);
    report("then half and half", open.is_some_and(|((_, lw), (_, rw))| near(lw, rw, 1.5)), format!("{open:?}"));
    ok
}
