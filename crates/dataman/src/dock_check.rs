//! `dock-check` (`--demo`): panels moved between the docks with real mouse events through the
//! window — the explorer dragged by its grip to the bottom band (the preview is the area it then
//! takes, and the laid-out panel is exactly there once it has settled), the history dropped on
//! the explorer's left half, a drop over the middle of the work surface changing nothing, ⌘B
//! still shutting the explorer where it now is, the session noticing, and "Reset panel layout"
//! bringing every panel home. PASS/FAIL lines like `fk-check`.
//!
//! Captures: `dock-drag` holds the explorer mid-drag over the bottom band (the preview),
//! `dock-moved` shows it settled at the bottom with the history beside it, `dock-moving` the
//! same move caught on its way (`DATAMAN_DOCK_FREEZE=0.5`).

use std::time::Duration;

use dataman_core::layout::{Dock, PanelId, PanelLayout, Place};
use gpui::{AsyncWindowContext, Entity, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels, Point, point};

use crate::app::Workspace;
use crate::dock::Rect;
use crate::theme::{Pt, px};

pub fn is_stage(s: &str) -> bool {
    matches!(s, "dock-check" | "dock-drag" | "dock-moved" | "dock-moving")
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

/// A command as the palette or the menu sends it (the focus in the editor, as after the palette).
fn command(ws: &Entity<Workspace>, a: Box<dyn gpui::Action>, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        ws.update(cx, |w, cx| w.focus_editor(window, cx));
        window.dispatch_action(a, cx);
    });
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn at(x: f32, y: f32) -> Point<Pixels> {
    point(px(x), px(y))
}

/// The grip on a panel's top edge (`dock::GRIP`: centred, 10 pt high).
fn grip(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2., r.y + 5.)
}

fn panel(ws: &Entity<Workspace>, p: PanelId, cx: &mut AsyncWindowContext) -> Option<Rect> {
    cx.update(|_, cx| ws.read(cx).frame_now.panel(p)).ok().flatten()
}

fn layout(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> PanelLayout {
    cx.update(|_, cx| ws.read(cx).layout.clone()).unwrap()
}

/// Presses the grip at `from`, moves through `path` with the button held (a few steps, as a
/// hand would), and lets go at the last point unless `hold`.
async fn drag(cx: &mut AsyncWindowContext, from: (f32, f32), path: &[(f32, f32)], hold: bool) {
    let none = Modifiers::default();
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: at(from.0, from.1), modifiers: none, click_count: 1, first_mouse: false }));
    // Past GPUI's drag threshold first: the drag starts.
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: at(from.0 + 6., from.1 + 6.), pressed_button: Some(MouseButton::Left), modifiers: none }));
    for &(x, y) in path {
        input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: at(x, y), pressed_button: Some(MouseButton::Left), modifiers: none }));
        pause(cx, 30).await;
    }
    if !hold {
        let (x, y) = *path.last().unwrap_or(&from);
        input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: at(x, y), modifiers: none, click_count: 1 }));
    }
}

fn near(a: Rect, b: Rect, tol: f32) -> bool {
    (a.x - b.x).abs() <= tol && (a.y - b.y).abs() <= tol && (a.w - b.w).abs() <= tol && (a.h - b.h).abs() <= tol
}

/// The slots of dock `d` as GPUI laid them out, in points.
fn laid_out(ws: &Entity<Workspace>, d: Dock, cx: &mut AsyncWindowContext) -> Vec<Rect> {
    let i = Dock::ALL.iter().position(|x| *x == d).unwrap();
    cx.update(|_, cx| {
        let n = ws.read(cx).frame_now.dock(d).slots.len();
        ws.read(cx).slot_bounds.borrow()[i]
            .iter()
            .take(n)
            .map(|b| Rect { x: b.origin.x.pt(), y: b.origin.y.pt(), w: b.size.width.pt(), h: b.size.height.pt() })
            .collect()
    })
    .unwrap()
}

/// The bottom band of the work surface, a little left of its middle.
fn bottom_band(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> (f32, f32) {
    let c = cx.update(|_, cx| ws.read(cx).frame_now.center).unwrap();
    (c.x + c.w * 0.4, c.bottom() - 30.)
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    // The history open on the right, the explorer on the left: the default arrangement.
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.toggle_history_for_demo(cx)));
    pause(cx, 400).await;
    draw(cx);
    let (Some(explorer), Some(history)) = (panel(ws, PanelId::Explorer, cx), panel(ws, PanelId::Side, cx)) else {
        report("both panels on screen", false, "explorer or history missing".into());
        return false;
    };
    report("the default arrangement", layout(ws, cx).is_default() && explorer.x == 0. && history.right() > explorer.right(), format!("{explorer:?} {history:?}"));
    let fp0 = cx.update(|_, cx| ws.read(cx).session_fingerprint(cx)).unwrap();

    // The explorer by its grip to the bottom band: the preview, then the drop.
    let band = bottom_band(ws, cx);
    let start = grip(explorer);
    let hold = stage == "dock-drag";
    drag(cx, start, &[(start.0 + 120., start.1 + 200.), (band.0 - 60., band.1 - 20.), band], true).await;
    draw(cx);
    let (target, preview, shown) = cx
        .update(|_, cx| ws.read(cx).panel_drag.as_ref().map(|g| (g.target, g.preview, g.shown)))
        .unwrap()
        .unwrap_or((None, None, None));
    let want = Place { dock: Dock::Bottom, index: 0 };
    report("the bottom band offers the bottom dock", target == Some(want), format!("{target:?}"));
    let expected = cx.update(|window, cx| ws.read(cx).rest_frame(&ws.read(cx).layout.moved(PanelId::Explorer, want), window).panel(PanelId::Explorer)).unwrap();
    report("the preview is the area it will take", preview.is_some() && preview == expected, format!("{preview:?} vs {expected:?}"));
    report("the preview is drawn", shown.is_some(), format!("{shown:?}"));
    if hold {
        return ok;
    }
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: at(band.0, band.1), modifiers: Modifiers::default(), click_count: 1 }));
    let moving = cx.update(|_, cx| !ws.read(cx).dock_moves.is_empty() || cx.reduce_motion()).unwrap();
    report("dropped, it settles into place", layout(ws, cx).bottom == [PanelId::Explorer] && moving, format!("{:?} moving {moving}", layout(ws, cx)));
    if stage == "dock-moving" {
        return ok;
    }
    // Half-way it is neither here nor there; once settled, exactly the preview.
    pause(cx, 60).await;
    draw(cx);
    let mid = panel(ws, PanelId::Explorer, cx);
    let left_mid = cx.update(|_, cx| ws.read(cx).frame_now.dock(Dock::Left).rect.w).unwrap();
    report("mid-move the left dock folds as the bottom opens", cx.update(|_, cx| cx.reduce_motion()).unwrap() || (left_mid > 0. && left_mid < explorer.w && mid.is_some_and(|m| m.h < expected.unwrap().h)), format!("left {left_mid} {mid:?}"));
    pause(cx, 400).await;
    draw(cx);
    let landed = panel(ws, PanelId::Explorer, cx);
    report("landed on the preview", landed.zip(expected).is_some_and(|(a, b)| near(a, b, 0.01)), format!("{landed:?} vs {expected:?}"));
    let slots = laid_out(ws, Dock::Bottom, cx);
    report("laid out exactly there", slots.first().zip(expected).is_some_and(|(a, b)| near(*a, b, 1.5)), format!("{slots:?} vs {expected:?}"));
    report("the session notices", cx.update(|_, cx| ws.read(cx).session_fingerprint(cx)).unwrap() != fp0, String::new());
    if stage == "dock-moved" {
        // The history beside it, on its left: dropped on the explorer's left half.
        let history = panel(ws, PanelId::Side, cx).unwrap();
        let e = landed.unwrap();
        drag(cx, grip(history), &[(history.x - 200., history.y + 300.), (e.x + e.w * 0.2, e.y + e.h / 2.)], false).await;
        pause(cx, 400).await;
        draw(cx);
        return ok;
    }

    // The history onto the explorer's left half: they share the bottom dock, history first.
    let history = panel(ws, PanelId::Side, cx).unwrap();
    let e = landed.unwrap();
    drag(cx, grip(history), &[(history.x - 200., history.y + 300.), (e.x + e.w * 0.2, e.y + e.h / 2.)], false).await;
    pause(cx, 400).await;
    draw(cx);
    let (h2, e2) = (panel(ws, PanelId::Side, cx), panel(ws, PanelId::Explorer, cx));
    report(
        "dropped on a panel's left half: beside it, first",
        layout(ws, cx).bottom == [PanelId::Side, PanelId::Explorer] && h2.zip(e2).is_some_and(|(h, e)| (h.right() - e.x).abs() < 0.01 && h.y == e.y),
        format!("{:?} {h2:?} {e2:?}", layout(ws, cx)),
    );
    let slots = laid_out(ws, Dock::Bottom, cx);
    report("both laid out where the frame says", slots.len() == 2 && near(slots[0], h2.unwrap(), 1.5) && near(slots[1], e2.unwrap(), 1.5), format!("{slots:?}"));

    // Let go over the middle of the work surface: nothing moves.
    let before = layout(ws, cx);
    let c = cx.update(|_, cx| ws.read(cx).frame_now.center).unwrap();
    let e = e2.unwrap();
    drag(cx, grip(e), &[(c.x + c.w / 2., c.y + c.h / 2.)], true).await;
    let none = cx.update(|_, cx| ws.read(cx).panel_drag.as_ref().map(|g| (g.target, g.preview))).unwrap();
    report("over the middle: no target, no preview", none == Some((None, None)), format!("{none:?}"));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: at(c.x + c.w / 2., c.y + c.h / 2.), modifiers: Modifiers::default(), click_count: 1 }));
    draw(cx);
    report("let go there: unchanged", layout(ws, cx) == before && cx.update(|_, cx| ws.read(cx).panel_drag.is_none()).unwrap(), format!("{:?}", layout(ws, cx)));

    // ⌘B shuts the explorer where it is, and opens it there again.
    command(ws, Box::new(crate::app::ToggleExplorer), cx);
    pause(cx, 400).await;
    draw(cx);
    let shut = panel(ws, PanelId::Explorer, cx);
    let h3 = panel(ws, PanelId::Side, cx);
    report("⌘B shuts it in the bottom dock", shut.is_none() && h3.is_some_and(|h| h.w > e.w), format!("{shut:?} {h3:?}"));
    command(ws, Box::new(crate::app::ToggleExplorer), cx);
    pause(cx, 400).await;
    draw(cx);
    report("⌘B opens it there again", panel(ws, PanelId::Explorer, cx).is_some_and(|r| near(r, e, 0.01)), format!("{:?}", panel(ws, PanelId::Explorer, cx)));

    // The palette's commands: the side panel to the left dock, then everything home.
    command(ws, Box::new(crate::dock::MoveSidePanelLeft), cx);
    pause(cx, 400).await;
    draw(cx);
    let h4 = panel(ws, PanelId::Side, cx);
    report("Move side panel to the left dock", layout(ws, cx).left == [PanelId::Side] && h4.is_some_and(|h| h.x == 0.), format!("{:?} {h4:?}", layout(ws, cx)));
    command(ws, Box::new(crate::dock::ResetPanelLayout), cx);
    pause(cx, 400).await;
    draw(cx);
    let (e5, h5) = (panel(ws, PanelId::Explorer, cx), panel(ws, PanelId::Side, cx));
    report(
        "Reset panel layout: every panel home",
        layout(ws, cx).is_default() && e5.zip(Some(explorer)).is_some_and(|(a, b)| near(a, b, 0.01)) && h5.zip(Some(history)).is_some_and(|(a, b)| near(a, b, 0.01)),
        format!("{e5:?} {h5:?}"),
    );
    ok
}
