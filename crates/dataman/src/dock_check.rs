//! `dock-check` (`--demo`): panels moved between the docks with real mouse and key events
//! through the window — the explorer held over the work surface's bottom edge finding no dock
//! there, Esc letting go of that drag (and of a tab's) without doing anything else, the history
//! dropped beside the explorer as a column of its own (the preview is the area it then takes,
//! and the laid-out panel is exactly there once it has settled), the result zoom keeping it
//! there, then stacked under the explorer with the seam between them dragged and double-clicked
//! back to even, the history's header menu moving it home, ⌘B still shutting the explorer where
//! it now is, the session noticing, and "Reset panel layout" bringing every panel home. PASS/FAIL
//! lines like `fk-check`. Along the way: a cell viewer (⇧Space in the grid) folding the right dock
//! away and back without touching the arrangement or the session, the history's toggle (⌘Y) and
//! ⌘B showing it again meanwhile, and the title bar's "DATAMAN" staying over the explorer's column
//! (beside the history, either side, stacked with it; gone with the explorer in the right dock).
//!
//! Captures (`DATAMAN_SHOT`): `dock-drag` holds the history mid-drag over the explorer's right
//! edge (the preview), `dock-moved` shows it settled beside the explorer, `dock-moving` the same
//! move caught on its way (`DATAMAN_DOCK_FREEZE=0.5`), `dock-stacked` the two stacked at a moved
//! split, `dock-menu` the history's header menu. `DATAMAN_SHOT_DIR=dir` also captures
//! `dock-esc.png` (a drag let go of), `dock-stacked-hover.png` (the history's × on its header's
//! hover), `dock-viewer-folding.png` / `dock-viewer.png` (the right dock folding away for a cell
//! viewer, and gone), `dock-beside.png` / `dock-history-left.png` (the history in a column right or
//! left of the explorer: its own fill, "DATAMAN" over the explorer).

use std::time::Duration;

use dataman_core::groups::Pane;
use dataman_core::layout::{Column, Dock, EVEN, PanelId, PanelLayout, Place};
use gpui::{AsyncWindowContext, Entity, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels, Point, point};

use crate::app::{Side, Workspace};
use crate::dock::Rect;
use crate::theme::{Pt, metrics, px};

pub fn is_stage(s: &str) -> bool {
    matches!(s, "dock-check" | "dock-drag" | "dock-moved" | "dock-moving" | "dock-stacked" | "dock-menu")
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

/// `DATAMAN_SHOT_DIR`: a capture of the window as it is, `dock-<name>.png`.
fn snap(name: &str, cx: &mut AsyncWindowContext) {
    let Ok(dir) = std::env::var("DATAMAN_SHOT_DIR") else { return };
    let path = format!("{dir}/dock-{name}.png");
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
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

/// Lets a move (or a zoom) settle, then draws.
async fn settle(cx: &mut AsyncWindowContext) {
    pause(cx, 400).await;
    draw(cx);
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

fn panels(cols: &[Column]) -> Vec<Vec<PanelId>> {
    cols.iter().map(|c| c.panels.clone()).collect()
}

fn none() -> Modifiers {
    Modifiers::default()
}

fn down(cx: &mut AsyncWindowContext, (x, y): (f32, f32), button: MouseButton, clicks: usize) {
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button, position: at(x, y), modifiers: none(), click_count: clicks, first_mouse: false }));
}

fn up(cx: &mut AsyncWindowContext, (x, y): (f32, f32), button: MouseButton, clicks: usize) {
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button, position: at(x, y), modifiers: none(), click_count: clicks }));
}

fn move_to(cx: &mut AsyncWindowContext, (x, y): (f32, f32), held: bool) {
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: at(x, y), pressed_button: held.then_some(MouseButton::Left), modifiers: none() }));
}

fn press(cx: &mut AsyncWindowContext, k: &str) {
    let ks = Keystroke::parse(k).unwrap();
    input(cx, PlatformInput::KeyDown(KeyDownEvent { keystroke: ks.clone(), is_held: false, prefer_character_input: false }));
    input(cx, PlatformInput::KeyUp(KeyUpEvent { keystroke: ks }));
}

/// Presses at `from`, moves through `path` with the button held (a few steps, as a hand would),
/// and lets go at the last point unless `hold`.
async fn drag(cx: &mut AsyncWindowContext, from: (f32, f32), path: &[(f32, f32)], hold: bool) {
    down(cx, from, MouseButton::Left, 1);
    // Past GPUI's drag threshold first: the drag starts.
    move_to(cx, (from.0 + 6., from.1 + 6.), true);
    for &p in path {
        move_to(cx, p, true);
        pause(cx, 30).await;
    }
    if !hold {
        up(cx, *path.last().unwrap_or(&from), MouseButton::Left, 1);
    }
}

fn near(a: Rect, b: Rect, tol: f32) -> bool {
    (a.x - b.x).abs() <= tol && (a.y - b.y).abs() <= tol && (a.w - b.w).abs() <= tol && (a.h - b.h).abs() <= tol
}

/// The slots of dock `d`, column by column, as GPUI laid them out and as the frame says.
fn laid_out(ws: &Entity<Workspace>, d: Dock, cx: &mut AsyncWindowContext) -> (Vec<Rect>, Vec<Rect>) {
    let i = crate::dock::ix(d);
    cx.update(|_, cx| {
        let w = ws.read(cx);
        let bounds = w.slot_bounds.borrow();
        let mut laid = Vec::new();
        for (ci, col) in w.frame_now.dock(d).cols.iter().enumerate() {
            let b = bounds[i].get(ci).cloned().unwrap_or_default();
            laid.extend(b.iter().take(col.slots.len()).map(|b| Rect { x: b.origin.x.pt(), y: b.origin.y.pt(), w: b.size.width.pt(), h: b.size.height.pt() }));
        }
        (laid, w.frame_now.dock(d).slots().map(|(_, r)| *r).collect())
    })
    .unwrap()
}

fn laid_right(ws: &Entity<Workspace>, d: Dock, cx: &mut AsyncWindowContext) -> bool {
    let (laid, want) = laid_out(ws, d, cx);
    laid.len() == want.len() && laid.iter().zip(&want).all(|(a, b)| near(*a, *b, 1.5))
}

/// The title bar's "DATAMAN" as laid out.
fn logo(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Rect> {
    cx.update(|_, cx| ws.read(cx).logo_bounds.get()).ok().flatten().map(|b| Rect { x: b.origin.x.pt(), y: b.origin.y.pt(), w: b.size.width.pt(), h: b.size.height.pt() })
}

/// "DATAMAN" at the right edge of column `c` (its 14 pt inset), inside it.
fn logo_over(l: Option<Rect>, c: Rect) -> bool {
    l.is_some_and(|l| l.x >= c.x && (l.right() - (c.right() - 14.)).abs() < 1.5)
}

/// The explorer's column in the left dock.
fn explorer_col(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Rect> {
    cx.update(|_, cx| ws.read(cx).frame_now.dock(Dock::Left).cols.iter().find(|c| c.slots.iter().any(|(s, _)| s.panel == PanelId::Explorer && !s.ghost)).map(|c| c.rect)).ok().flatten()
}

fn viewer_open(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|_, cx| ws.read(cx).viewer_open(Pane::Left)).unwrap_or(false)
}

/// A click in the result grid (it takes the focus), then ⇧Space: the group's cell viewer opens
/// or shuts.
fn toggle_viewer(cx: &mut AsyncWindowContext, ws: &Entity<Workspace>) {
    if let Some(r) = cx.update(|_, cx| ws.read(cx).result_bounds.get()).ok().flatten() {
        let p = (r.left().pt() + 60., r.top().pt() + metrics::RESULT_BAR_H + 50.);
        down(cx, p, MouseButton::Left, 1);
        up(cx, p, MouseButton::Left, 1);
    }
    press(cx, "shift-space");
}

fn right_w(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> f32 {
    cx.update(|_, cx| ws.read(cx).frame_now.dock(Dock::Right).rect.w).unwrap()
}

fn zoom(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.toggle_result_zoom(window, cx)));
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let history_view = cx.update(|_, cx| ws.read(cx).history.clone()).unwrap();
    // The history open on the right, the explorer on the left: the default arrangement.
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.toggle_history_for_demo(cx)));
    settle(cx).await;
    let (Some(explorer), Some(history)) = (panel(ws, PanelId::Explorer, cx), panel(ws, PanelId::History, cx)) else {
        report("both panels on screen", false, "explorer or history missing".into());
        return false;
    };
    report("the default arrangement", layout(ws, cx).is_default() && explorer.x == 0. && history.right() > explorer.right(), format!("{explorer:?} {history:?}"));
    let fp0 = cx.update(|_, cx| ws.read(cx).session_fingerprint(cx)).unwrap();
    let full_stage = stage == "dock-check";
    let logo0 = logo(ws, cx);
    report("\"DATAMAN\" sits over the explorer, at its right edge", logo_over(logo0, explorer), format!("{logo0:?} {explorer:?}"));

    if full_stage {
        // Alone in its dock, the history folds away under the zoomed result, as before.
        zoom(ws, cx);
        settle(cx).await;
        let zoomed = panel(ws, PanelId::History, cx);
        zoom(ws, cx);
        settle(cx).await;
        report("alone in its dock, the history folds under the result zoom", zoomed.is_none() && panel(ws, PanelId::History, cx) == Some(history), format!("{zoomed:?}"));

        // No bottom dock: the explorer held over the work surface's bottom edge finds nothing.
        let c = cx.update(|_, cx| ws.read(cx).frame_now.center).unwrap();
        let start = grip(explorer);
        let bottom = (c.x + c.w * 0.4, c.bottom() - 20.);
        drag(cx, start, &[(start.0 + 120., start.1 + 200.), bottom], true).await;
        draw(cx);
        let found = cx.update(|_, cx| ws.read(cx).panel_drag.as_ref().map(|g| (g.target, g.preview))).unwrap();
        report("the bottom edge offers no dock (there is none)", found == Some((None, None)), format!("{found:?}"));
        // Esc lets go of it, mid-drag, with the history's search focused and holding text: the
        // drag ends, nothing drops, and the key does nothing else (the panel stays open, its
        // search keeps the focus and its text).
        let _ = cx.update(|window, cx| history_view.update(cx, |h, cx| h.demo_search("select", window, cx)));
        let rows = cx.update(|_, cx| history_view.read(cx).demo_rows()).unwrap();
        let searching = |cx: &mut AsyncWindowContext| cx.update(|window, cx| history_view.read(cx).filter_focus(cx).is_focused(window)).unwrap();
        let held = cx.update(|_, cx| cx.has_active_drag()).unwrap() && searching(cx);
        press(cx, "escape");
        let (dragging, no_preview, open, rows_kept) =
            cx.update(|_, cx| (cx.has_active_drag(), ws.read(cx).panel_drag.is_none(), matches!(ws.read(cx).side, Some(Side::History)), history_view.read(cx).demo_rows() == rows)).unwrap();
        let after = (dragging, no_preview, open, searching(cx), rows_kept);
        report("esc lets go of a panel's drag", held && !after.0 && after.1, format!("held {held} {after:?}"));
        report("…and does nothing else (the history stays, its search keeps the focus and its text)", after.2 && after.3 && after.4, format!("{after:?}"));
        snap("esc", cx);
        up(cx, bottom, MouseButton::Left, 1);
        move_to(cx, (bottom.0 + 40., bottom.1 - 40.), false);
        draw(cx);
        report("let go of after esc: nothing dropped", layout(ws, cx).is_default() && panel(ws, PanelId::Explorer, cx) == Some(explorer), format!("{:?}", layout(ws, cx)));

        // Esc lets go of a tab's drag too: the tabs stay in order.
        let result_tab = cx.update(|_, cx| ws.read(cx).active_tab().cloned()).unwrap();
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.new_tab(window, cx)));
        draw(cx);
        let tabs_before = cx.update(|_, cx| ws.read(cx).tabs.clone()).unwrap();
        let tb = cx.update(|_, cx| ws.read(cx).tab_bounds.borrow().clone()).unwrap();
        if tb.len() >= 2 {
            let mid = |b: gpui::Bounds<Pixels>| (b.left().pt() + b.size.width.pt() / 2., b.top().pt() + b.size.height.pt() / 2.);
            let (p0, p1) = (mid(tb[0]), mid(tb[1]));
            drag(cx, p0, &[(p1.0 + 10., p1.1)], true).await;
            draw(cx);
            let during = cx.update(|_, cx| (cx.has_active_drag(), ws.read(cx).tab_drop)).unwrap();
            press(cx, "escape");
            let gone = cx.update(|_, cx| !cx.has_active_drag()).unwrap();
            up(cx, (p1.0 + 10., p1.1), MouseButton::Left, 1);
            draw(cx);
            let kept = cx.update(|_, cx| ws.read(cx).tabs == tabs_before).unwrap();
            report("esc lets go of a tab's drag; the tabs stay in order", during.0 && during.1.is_some() && gone && kept, format!("{during:?} gone {gone} kept {kept}"));
        } else {
            report("esc lets go of a tab's drag", false, format!("{} tab bounds", tb.len()));
        }
        // Back to the tab with the result (the zoom below needs one).
        let _ = cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                if let Some(ix) = result_tab.as_ref().and_then(|t| w.tabs.iter().position(|x| x == t)) {
                    w.activate(ix, window, cx);
                }
            })
        });
        settle(cx).await;

        // A cell viewer (⇧Space in the grid) folds the right dock away, sliding; the
        // arrangement, the history's state and the session stay as they were.
        let reduced = cx.update(|_, cx| cx.reduce_motion()).unwrap();
        let fp1 = cx.update(|_, cx| ws.read(cx).session_fingerprint(cx)).unwrap();
        toggle_viewer(cx, ws);
        pause(cx, 60).await;
        draw(cx);
        let (folding, mid) = (cx.update(|_, cx| ws.read(cx).demo_right_folding()).unwrap(), right_w(ws, cx));
        snap("viewer-folding", cx);
        settle(cx).await;
        let (open, gone, w0) = (viewer_open(ws, cx), panel(ws, PanelId::History, cx), right_w(ws, cx));
        report(
            "a cell viewer folds the right dock away (sliding)",
            open && gone.is_none() && w0 == 0. && (reduced || (folding && mid > 0. && mid < history.w)),
            format!("viewer {open} {gone:?} right {w0}, mid {mid} folding {folding}"),
        );
        snap("viewer", cx);
        let same = cx.update(|_, cx| (ws.read(cx).layout.is_default(), matches!(ws.read(cx).side, Some(Side::History)), ws.read(cx).session_fingerprint(cx) == fp1)).unwrap();
        report("…for the moment: the arrangement, the history and the session unchanged", same == (true, true, true), format!("{same:?}"));
        // The history's toggle while it is folded away shows it again, over the viewer.
        press(cx, "cmd-y");
        settle(cx).await;
        let shown = (panel(ws, PanelId::History, cx), viewer_open(ws, cx), cx.update(|_, cx| matches!(ws.read(cx).side, Some(Side::History))).unwrap());
        report("⌘Y then shows the history again (not shut out of sight), the viewer open", shown.0 == Some(history) && shown.1 && shown.2, format!("{shown:?}"));
        // The viewer shut: nothing moves. Opened again, it folds the dock again; shut, the dock
        // comes back exactly as it was.
        toggle_viewer(cx, ws);
        settle(cx).await;
        let kept = (viewer_open(ws, cx), panel(ws, PanelId::History, cx));
        toggle_viewer(cx, ws);
        settle(cx).await;
        let refold = (viewer_open(ws, cx), panel(ws, PanelId::History, cx));
        toggle_viewer(cx, ws);
        settle(cx).await;
        let back = (viewer_open(ws, cx), panel(ws, PanelId::History, cx));
        report(
            "another viewer folds it again; the last one shut, it is back as it was",
            !kept.0 && kept.1 == Some(history) && refold.0 && refold.1.is_none() && !back.0 && back.1 == Some(history) && layout(ws, cx).is_default(),
            format!("{kept:?} {refold:?} {back:?}"),
        );
    }

    // The history by its grip onto the explorer's right edge: a column of its own beside it.
    let to = (explorer.right() - 20., explorer.y + explorer.h / 2.);
    let start = grip(history);
    drag(cx, start, &[(start.0 - 200., start.1 + 200.), (to.0 + 80., to.1), to], true).await;
    draw(cx);
    let (target, preview, shown) = cx.update(|_, cx| ws.read(cx).panel_drag.as_ref().map(|g| (g.target, g.preview, g.shown))).unwrap().unwrap_or((None, None, None));
    let want = Place::column(Dock::Left, 1);
    report("the explorer's right edge offers a column beside it", target == Some(want), format!("{target:?}"));
    let expected = cx.update(|window, cx| ws.read(cx).rest_frame(&ws.read(cx).layout.moved(PanelId::History, want), window).panel(PanelId::History)).unwrap();
    report("the preview is the area it will take", preview.is_some() && preview == expected, format!("{preview:?} vs {expected:?}"));
    report("the preview is drawn", shown.is_some(), format!("{shown:?}"));
    if stage == "dock-drag" {
        return ok;
    }
    up(cx, to, MouseButton::Left, 1);
    let moving = cx.update(|_, cx| !ws.read(cx).dock_moves.is_empty() || cx.reduce_motion()).unwrap();
    let l = layout(ws, cx);
    report("dropped, it settles into place", panels(&l.left) == [vec![PanelId::Explorer], vec![PanelId::History]] && l.right.is_empty() && moving, format!("{l:?} moving {moving}"));
    if stage == "dock-moving" {
        return ok;
    }
    // Half-way it is neither here nor there; once settled, exactly the preview.
    pause(cx, 60).await;
    draw(cx);
    let right_mid = cx.update(|_, cx| ws.read(cx).frame_now.dock(Dock::Right).rect.w).unwrap();
    let reduced = cx.update(|_, cx| cx.reduce_motion()).unwrap();
    report("mid-move the right dock folds as the new column opens", reduced || (right_mid > 0. && right_mid < history.w), format!("right {right_mid}"));
    settle(cx).await;
    let landed = panel(ws, PanelId::History, cx);
    report("landed on the preview", landed.zip(expected).is_some_and(|(a, b)| near(a, b, 0.01)), format!("{landed:?} vs {expected:?}"));
    report("laid out exactly there", laid_right(ws, Dock::Left, cx), format!("{:?}", laid_out(ws, Dock::Left, cx)));
    report("the session notices", cx.update(|_, cx| ws.read(cx).session_fingerprint(cx)).unwrap() != fp0, String::new());
    let lg = logo(ws, cx);
    report("beside it, \"DATAMAN\" stays over the explorer (not over the history)", logo_over(lg, explorer) && lg == logo0, format!("{lg:?} vs {logo0:?}"));
    snap("beside", cx);
    if stage == "dock-moved" {
        return ok;
    }

    // Sharing the explorer's dock, the history stays through the result zoom.
    if full_stage {
        zoom(ws, cx);
        settle(cx).await;
        let zoomed = cx.update(|_, cx| ws.read(cx).active_tab().is_some_and(|t| t.read(cx).zoomed)).unwrap();
        let kept = panel(ws, PanelId::History, cx);
        zoom(ws, cx);
        settle(cx).await;
        report("beside the explorer, the history stays through the result zoom", zoomed && kept.zip(landed).is_some_and(|(a, b)| near(a, b, 0.01)), format!("zoomed {zoomed} {kept:?}"));
    }

    // Onto the explorer's lower edge: stacked under it, the history's × only on its header's
    // hover.
    let e = panel(ws, PanelId::Explorer, cx).unwrap();
    let h = landed.unwrap();
    let to = (e.x + e.w / 2., e.bottom() - 30.);
    drag(cx, grip(h), &[(h.x + 40., h.y + 300.), (to.0, to.1 - 60.), to], false).await;
    settle(cx).await;
    let l = layout(ws, cx);
    let (e2, h2) = (panel(ws, PanelId::Explorer, cx).unwrap(), panel(ws, PanelId::History, cx).unwrap());
    report(
        "dropped on a panel's lower edge: stacked under it",
        panels(&l.left) == [vec![PanelId::Explorer, PanelId::History]] && (e2.bottom() - h2.y).abs() < 0.01 && e2.x == h2.x && (e2.h - h2.h).abs() < 0.01,
        format!("{l:?} {e2:?} {h2:?}"),
    );
    report("both laid out where the frame says", laid_right(ws, Dock::Left, cx), format!("{:?}", laid_out(ws, Dock::Left, cx)));
    let (lg, ec) = (logo(ws, cx), explorer_col(ws, cx));
    report("stacked, \"DATAMAN\" stays over their column", ec.is_some_and(|c| logo_over(lg, c)), format!("{lg:?} {ec:?}"));
    report("sharing a dock, the history's × waits for its header's hover", cx.update(|_, cx| history_view.read(cx).shares_dock).unwrap(), String::new());
    // The seam between them, dragged up: the explorer shorter, the history taller.
    let col = cx.update(|_, cx| ws.read(cx).frame_now.dock(Dock::Left).cols[0].rect).unwrap();
    let seam = (e2.x + 12., e2.bottom());
    let dest = (seam.0, seam.1 - 150.);
    down(cx, seam, MouseButton::Left, 1);
    move_to(cx, (seam.0, seam.1 - 60.), true);
    move_to(cx, dest, true);
    up(cx, dest, MouseButton::Left, 1);
    draw(cx);
    let split = layout(ws, cx).left[0].split;
    let want_split = (dest.1 - col.y) / col.h;
    let (e3, h3) = (panel(ws, PanelId::Explorer, cx).unwrap(), panel(ws, PanelId::History, cx).unwrap());
    report(
        "the seam between stacked panels moves their split",
        (split - want_split).abs() < 0.005 && (e3.bottom() - dest.1).abs() < 0.5 && (h3.y - dest.1).abs() < 0.5,
        format!("split {split} vs {want_split}, {e3:?} {h3:?}"),
    );
    report("…and the session notices", cx.update(|_, cx| ws.read(cx).session_fingerprint(cx)).unwrap() != fp0, String::new());
    if stage == "dock-stacked" {
        move_to(cx, (col.right() + 300., col.y + 300.), false);
        draw(cx);
        return ok;
    }
    // Hovering the history's header shows its × (a capture).
    move_to(cx, (h3.x + h3.w / 2., h3.y + 20.), false);
    snap("stacked-hover", cx);
    // A double click on the seam: an even split again.
    let seam = (e3.x + 12., e3.bottom());
    down(cx, seam, MouseButton::Left, 1);
    up(cx, seam, MouseButton::Left, 1);
    down(cx, seam, MouseButton::Left, 2);
    up(cx, seam, MouseButton::Left, 2);
    draw(cx);
    let split = layout(ws, cx).left[0].split;
    report("a double click on the seam splits it evenly", split == EVEN, format!("split {split}"));

    // Stacked with the explorer, the history stays through the result zoom too.
    zoom(ws, cx);
    settle(cx).await;
    let zoomed = cx.update(|_, cx| ws.read(cx).active_tab().is_some_and(|t| t.read(cx).zoomed)).unwrap();
    let kept = panel(ws, PanelId::History, cx);
    let stacked = kept;
    zoom(ws, cx);
    settle(cx).await;
    report("stacked with the explorer, the history stays through the result zoom", zoomed && kept.is_some() && kept == panel(ws, PanelId::History, cx), format!("zoomed {zoomed} {stacked:?}"));

    // Let go over the middle of the work surface: nothing moves.
    let before = layout(ws, cx);
    let c = cx.update(|_, cx| ws.read(cx).frame_now.center).unwrap();
    let e = panel(ws, PanelId::Explorer, cx).unwrap();
    drag(cx, grip(e), &[(c.x + c.w / 2., c.y + c.h / 2.)], true).await;
    let nothing = cx.update(|_, cx| ws.read(cx).panel_drag.as_ref().map(|g| (g.target, g.preview))).unwrap();
    report("over the middle: no target, no preview", nothing == Some((None, None)), format!("{nothing:?}"));
    up(cx, (c.x + c.w / 2., c.y + c.h / 2.), MouseButton::Left, 1);
    draw(cx);
    report("let go there: unchanged", layout(ws, cx) == before && cx.update(|_, cx| ws.read(cx).panel_drag.is_none()).unwrap(), format!("{:?}", layout(ws, cx)));

    // ⌘B shuts the explorer where it is (the history has the column), and opens it there again.
    command(ws, Box::new(crate::app::ToggleExplorer), cx);
    settle(cx).await;
    let shut = panel(ws, PanelId::Explorer, cx);
    let h4 = panel(ws, PanelId::History, cx);
    report("⌘B shuts it in its stack", shut.is_none() && h4.is_some_and(|h| (h.y - col.y).abs() < 0.01 && (h.h - col.h).abs() < 0.01), format!("{shut:?} {h4:?}"));
    command(ws, Box::new(crate::app::ToggleExplorer), cx);
    settle(cx).await;
    report("⌘B opens it there again", panel(ws, PanelId::Explorer, cx).is_some_and(|r| near(r, e, 0.01)), format!("{:?}", panel(ws, PanelId::Explorer, cx)));

    // The history onto the explorer's left edge: a column left of it. "DATAMAN" goes with the
    // explorer, over its column.
    if full_stage {
        let (e, h) = (panel(ws, PanelId::Explorer, cx).unwrap(), panel(ws, PanelId::History, cx).unwrap());
        let to = (e.x + 12., e.y + e.h / 2.);
        drag(cx, grip(h), &[(h.x + 200., h.y + 100.), (to.0 + 40., to.1), to], false).await;
        settle(cx).await;
        let l = layout(ws, cx);
        let (e7, lg) = (panel(ws, PanelId::Explorer, cx), logo(ws, cx));
        report(
            "left of the explorer, the history pushes it right; \"DATAMAN\" goes with the explorer",
            panels(&l.left) == [vec![PanelId::History], vec![PanelId::Explorer]] && e7.is_some_and(|e| e.x > 0. && logo_over(lg, e)),
            format!("{l:?} {e7:?} {lg:?}"),
        );
        snap("history-left", cx);
    }

    // A right click on the history's header: its menu; "Move to Right Dock" takes it home.
    let h = panel(ws, PanelId::History, cx).unwrap();
    let click = (h.x + 70., h.y + 20.);
    down(cx, click, MouseButton::Right, 1);
    up(cx, click, MouseButton::Right, 1);
    draw(cx);
    let menu = cx.update(|_, cx| ws.read(cx).panel_menu).unwrap();
    report("a right click on its header opens the panel's menu", menu.is_some_and(|(p, _)| p == PanelId::History), format!("{menu:?}"));
    if stage == "dock-menu" {
        // The pointer over the menu's first item, as a hand would rest there.
        if let Some((_, at)) = menu {
            move_to(cx, (at.x.pt() + 40., at.y.pt() + 4. + 13.), false);
        }
        return ok;
    }
    snap("menu", cx);
    if let Some((_, at)) = menu {
        // The second item (the popover's 4 pt padding, 26 pt rows).
        let item = (at.x.pt() + 40., at.y.pt() + 4. + 26. + 13.);
        move_to(cx, item, false);
        down(cx, item, MouseButton::Left, 1);
        up(cx, item, MouseButton::Left, 1);
    }
    settle(cx).await;
    report("Move to Right Dock from the menu", layout(ws, cx).is_default() && cx.update(|_, cx| ws.read(cx).panel_menu.is_none()).unwrap(), format!("{:?}", layout(ws, cx)));

    // The palette's commands: the explorer to the right dock (next to the work surface), then
    // everything home.
    command(ws, Box::new(crate::dock::MoveExplorerRight), cx);
    settle(cx).await;
    let l = layout(ws, cx);
    let (e5, h5) = (panel(ws, PanelId::Explorer, cx), panel(ws, PanelId::History, cx));
    report(
        "Move explorer to the right dock: a column beside the history",
        panels(&l.right) == [vec![PanelId::Explorer], vec![PanelId::History]] && l.left.is_empty() && e5.zip(h5).is_some_and(|(e, h)| (e.right() - h.x).abs() < 0.01),
        format!("{l:?} {e5:?} {h5:?}"),
    );
    report("in the right dock the explorer leaves the title bar without \"DATAMAN\"", logo(ws, cx).is_none(), format!("{:?}", logo(ws, cx)));
    if full_stage {
        // A cell viewer folds the whole right dock (the explorer too); ⌘B shows it again rather
        // than shutting the explorer out of sight.
        toggle_viewer(cx, ws);
        settle(cx).await;
        let folded = (viewer_open(ws, cx), panel(ws, PanelId::Explorer, cx), panel(ws, PanelId::History, cx), right_w(ws, cx));
        report("a viewer folds the right dock, explorer and all", folded.0 && folded.1.is_none() && folded.2.is_none() && folded.3 == 0., format!("{folded:?}"));
        press(cx, "cmd-b");
        settle(cx).await;
        let shown = (cx.update(|_, cx| ws.read(cx).explorer_open).unwrap(), panel(ws, PanelId::Explorer, cx), panel(ws, PanelId::History, cx));
        report("⌘B shows it again, the explorer still open", shown.0 && shown.1 == e5 && shown.2 == h5, format!("{shown:?}"));
        toggle_viewer(cx, ws);
        settle(cx).await;
        report("the viewer shut, both stay", !viewer_open(ws, cx) && panel(ws, PanelId::Explorer, cx) == e5 && panel(ws, PanelId::History, cx) == h5, String::new());
    }
    command(ws, Box::new(crate::dock::ResetPanelLayout), cx);
    settle(cx).await;
    let (e6, h6) = (panel(ws, PanelId::Explorer, cx), panel(ws, PanelId::History, cx));
    report(
        "Reset panel layout: every panel home",
        layout(ws, cx).is_default() && e6.is_some_and(|a| near(a, explorer, 0.01)) && h6.is_some_and(|a| near(a, history, 0.01)),
        format!("{e6:?} {h6:?}"),
    );
    report("…and \"DATAMAN\" over the explorer again", logo(ws, cx) == logo0, format!("{:?}", logo(ws, cx)));
    ok
}
