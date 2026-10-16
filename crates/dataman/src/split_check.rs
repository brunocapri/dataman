//! `split-check` (`--demo`): two editor groups side by side, driven with real key and mouse
//! events through the window where a person would use them — ⌘\ opens a copy of the tab to the
//! side (the split growing in, then half and half, each strip over its group), a press in a group
//! gives it the focus and its Run button runs its own tab, the keyboard focus landing in the
//! other group moves the focus there, ⌃Tab and ⌘1 stay within the focused group, the seam
//! resizes the groups (a double-click evens them), a tab dragged onto the other strip moves
//! there, ⌥ held while a table opens from the explorer sends it to the other group, the session
//! keeps the groups across a relaunch, a group's × folds it away with its tabs joining the
//! other, and closing a group's last tab closes the split. PASS/FAIL lines like `dock-check`.
//!
//! Captures: `split` shows two groups with results in both, the right one focused;
//! `split-opening` catches ⌘\ on its way (`DATAMAN_SPLIT_FREEZE=0.5`).

use std::sync::Arc;
use std::time::Duration;

use dataman_core::groups::Pane;
use dataman_core::session::SessionStore;
use gpui::{AsyncWindowContext, Entity, Focusable, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels, Point, point};

use crate::app::Workspace;
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
    let strip_end = cx.update(|_, cx| ws.read(cx).tab_bounds_right.borrow().last().copied()).ok().flatten();
    if let Some(b) = strip_end {
        // The × is the strip's last child (22 pt), at its end.
        click(cx, point(b.left() + b.size.width / 2., b.top() + b.size.height / 2.));
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

    // Closing a group's last tab closes the split.
    let _ = cx.update(|_, cx| ws.update(cx, |w, _| w.focus_visible = false));
    key(cx, "cmd-\\");
    let n = tabs(ws, cx).len();
    let copy = active(ws, cx).unwrap();
    report("⌘\\ again: two groups", right(ws, cx) == [copy.clone()], format!("{}", right(ws, cx).len()));
    key(cx, "cmd-w");
    report(
        "closing a group's last tab closes the split; the other group's tab is in front",
        cx.update(|_, cx| ws.read(cx).split.is_none()).unwrap() && tabs(ws, cx).len() == n - 1 && !tabs(ws, cx).contains(&copy) && active(ws, cx) == left_front,
        format!("{} tabs", tabs(ws, cx).len()),
    );
    ok
}
