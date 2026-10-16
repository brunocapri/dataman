//! `dock-check` (`--demo`): panels moved between the docks with real mouse and key events
//! through the window — the explorer held over the work surface's bottom edge finding no dock
//! there, Esc letting go of that drag (and of a tab's) without doing anything else, the history
//! dropped beside the explorer as a column of its own (the preview is the area it then takes,
//! and the laid-out panel is exactly there once it has settled), the result zoom keeping it
//! there, then stacked under the explorer with the seam between them dragged and double-clicked
//! back to even, the history's header menu moving it home, ⌘B still shutting the explorer where
//! it now is, the session noticing, and "Reset panel layout" bringing every panel home. PASS/FAIL
//! lines like `fk-check`. Along the way: a JSON cell's value sheet (⇧Space in the grid) opening
//! over the window without moving a dock, the arrangement or the session, and the title bar's
//! "DATAMAN" staying over the explorer's column (beside the history, stacked with it, and in the
//! right dock).
//!
//! The explorer is always its dock's outermost column: the history dropped on its outer side
//! lands inside it (the preview shows that spot), the explorer dragged left of the history in
//! the right dock lands at the window's edge. In the right dock it carries the window's header
//! (its toggle mirrored at the right edge, the arrows, "DATAMAN" over its column) and the
//! search goes past the traffic lights. Beside the explorer the history is under the tab band
//! (the tabs are not pushed past it) but keeps its own fill and toggles; alone in the left dock it
//! takes the header slot (the tabs start past it), and the explorer docked beside it slides the
//! tabs over it (caught mid-way; at once with Reduce Motion). Stacked with the explorer (always
//! under it), the history takes its glass, ⌘B hides both, and ⌘Y, its palette entry and the
//! toolbar's History button are off; beside it, ⌘B hides the explorer alone.
//!
//! Captures (`DATAMAN_SHOT`): `dock-drag` holds the history mid-drag over the explorer's right
//! edge (the preview), `dock-moved` shows it settled beside the explorer, `dock-moving` the same
//! move caught on its way (`DATAMAN_DOCK_FREEZE=0.5`), `dock-stacked` the two stacked at a moved
//! split, `dock-menu` the history's header menu. `DATAMAN_SHOT_DIR=dir` also captures
//! `dock-esc.png` (a drag let go of), `dock-stacked-hover.png` (the history's × on its header's
//! hover), `dock-value.png` (a value sheet over the docks, which stay), `dock-beside.png` /
//! `dock-history-left.png` (the history beside the explorer, under the tab band, its own fill),
//! `dock-explorer-right.png` (the header mirrored), `dock-outermost-preview.png` (the explorer
//! dragged left of the history: the preview at the window's edge), `dock-history-alone-left.png`
//! (the header slot), `dock-tabs-sliding.png` (the tabs on their way over the history),
//! `dock-both-hidden.png` (⌘B with the history beside).

use std::time::Duration;

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

fn value_open(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|_, cx| ws.read(cx).value_view.is_some()).unwrap_or(false)
}

/// A click on a jsonb cell of the result (`orders.address`, the demo query's fourth column; the
/// grid takes the focus), then ⇧Space: its value sheet.
fn open_value(cx: &mut AsyncWindowContext, ws: &Entity<Workspace>) {
    let p = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.read(cx).grid.read(cx).demo_cell_point(1, 3))).ok().flatten();
    if let Some(p) = p {
        let p = (p.x.pt(), p.y.pt());
        down(cx, p, MouseButton::Left, 1);
        up(cx, p, MouseButton::Left, 1);
    }
    press(cx, "shift-space");
}

fn right_w(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> f32 {
    cx.update(|_, cx| ws.read(cx).frame_now.dock(Dock::Right).rect.w).unwrap()
}

fn view_w(cx: &mut AsyncWindowContext) -> f32 {
    cx.update(|window, _| window.viewport_size().width.pt()).unwrap()
}

fn title(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> crate::dock::TitleFrame {
    cx.update(|_, cx| ws.read(cx).title_now).unwrap()
}

/// The title bar's toggle, arrows or search as laid out.
fn mark(ws: &Entity<Workspace>, name: &str, cx: &mut AsyncWindowContext) -> Option<Rect> {
    cx.update(|_, cx| ws.read(cx).title_marks.borrow().get(name).copied()).ok().flatten().map(|b| Rect { x: b.origin.x.pt(), y: b.origin.y.pt(), w: b.size.width.pt(), h: b.size.height.pt() })
}

/// The first query tab as laid out.
fn first_tab(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Rect> {
    cx.update(|_, cx| ws.read(cx).tab_bounds.borrow().first().copied()).ok().flatten().map(|b| Rect { x: b.origin.x.pt(), y: b.origin.y.pt(), w: b.size.width.pt(), h: b.size.height.pt() })
}

/// Where the (only) tab strip's content ends.
fn strip_end(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> f32 {
    cx.update(|_, cx| ws.read(cx).strip_parts.borrow()[0].iter().map(|b| b.right().pt()).fold(0., f32::max)).unwrap()
}

/// ⌘Y as the keys, the menu and the palette see it: its handler is there, its entry live, and
/// the toolbar's parts (the History button among them).
fn history_toggles(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> (bool, bool, usize) {
    cx.update(|window, cx| {
        // As the keys see it from the editor.
        ws.update(cx, |w, cx| w.focus_editor(window, cx));
        let key = window.is_action_available(&crate::app::ToggleHistory, cx);
        let entry = ws.read(cx).palette_items(cx).iter().any(|i| i.label == "History" && i.section == crate::palette::Section::Commands && !i.disabled);
        (key, entry, ws.read(cx).toolbar_parts.borrow()[0].len())
    })
    .unwrap()
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
    let toggles0 = history_toggles(ws, cx);
    report("alone in its dock, the history has ⌘Y, its palette entry and its toolbar button", toggles0.0 && toggles0.1, format!("{toggles0:?}"));
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

        // A JSON cell's value sheet (⇧Space in the grid) sits over the window: the docks, the
        // arrangement, the history's state and the session stay as they were.
        let fp1 = cx.update(|_, cx| ws.read(cx).session_fingerprint(cx)).unwrap();
        open_value(cx, ws);
        settle(cx).await;
        let (open, kept, w0) = (value_open(ws, cx), panel(ws, PanelId::History, cx), right_w(ws, cx));
        report("⇧Space on a JSON cell opens its value sheet; the right dock stays", open && kept == Some(history) && w0 > 0., format!("sheet {open} {kept:?} right {w0}"));
        snap("value", cx);
        press(cx, "escape");
        settle(cx).await;
        let same = cx.update(|_, cx| (ws.read(cx).layout.is_default(), matches!(ws.read(cx).side, Some(Side::History)), ws.read(cx).session_fingerprint(cx) == fp1)).unwrap();
        report(
            "esc closes it; the arrangement, the history and the session unchanged",
            !value_open(ws, cx) && panel(ws, PanelId::History, cx) == Some(history) && same == (true, true, true),
            format!("{same:?}"),
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
    let (tf, tab) = (title(ws, cx), first_tab(ws, cx));
    report(
        "beside the explorer, the tabs are not pushed past the history: the band runs over it",
        landed.zip(tab).is_some_and(|(h, tb)| (tf.lead - explorer.right()).abs() < 0.5 && tb.x >= explorer.right() && tb.right() < h.right() && h.y >= metrics::TITLE_H - 0.01),
        format!("{tf:?} {tab:?} {landed:?}"),
    );
    // Beside the explorer the history stays its own: its fill, its × and its toggles.
    let own = cx.update(|_, cx| !ws.read(cx).history_with_explorer() && !history_view.read(cx).shares_dock).unwrap();
    report("…and stays its own beside it: its own fill and its ×", own, String::new());
    let toggles = history_toggles(ws, cx);
    report("beside the explorer, ⌘Y, its palette entry and the toolbar's History button stay on", toggles == toggles0, format!("{toggles:?} vs {toggles0:?}"));
    press(cx, "cmd-y");
    settle(cx).await;
    let shut = (panel(ws, PanelId::History, cx), panel(ws, PanelId::Explorer, cx));
    report("…⌘Y shuts the history alone", shut.0.is_none() && shut.1 == Some(explorer), format!("{shut:?}"));
    press(cx, "cmd-y");
    settle(cx).await;
    report("…and opens it again in its place", panel(ws, PanelId::History, cx) == landed, format!("{:?}", panel(ws, PanelId::History, cx)));
    // ⌘B hides the explorer only, and shows it again.
    press(cx, "cmd-b");
    settle(cx).await;
    let hidden = (panel(ws, PanelId::Explorer, cx), panel(ws, PanelId::History, cx));
    snap("explorer-hidden-beside", cx);
    report("⌘B hides the explorer, not the history beside it", hidden.0.is_none() && hidden.1.is_some(), format!("{hidden:?}"));
    press(cx, "cmd-b");
    settle(cx).await;
    let back = (panel(ws, PanelId::Explorer, cx), panel(ws, PanelId::History, cx));
    report("…and shows it again", back.0 == Some(explorer) && back.1 == landed, format!("{back:?}"));
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
    report("stacked with the explorer, the history has no × (the explorer controls it)", cx.update(|_, cx| history_view.read(cx).shares_dock).unwrap(), String::new());
    let toggles = history_toggles(ws, cx);
    report("stacked, ⌘Y, its palette entry and the toolbar's History button are off", !toggles.0 && !toggles.1 && toggles.2 + 1 == toggles0.2, format!("{toggles:?} vs {toggles0:?}"));
    press(cx, "cmd-y");
    settle(cx).await;
    report("…⌘Y does nothing then", panel(ws, PanelId::History, cx) == Some(h2), format!("{:?}", panel(ws, PanelId::History, cx)));
    // Onto the explorer's top half: under it all the same (the explorer is always on top).
    let to = (e2.x + e2.w / 2., e2.y + 30.);
    drag(cx, grip(h2), &[(h2.x + 40., h2.y + 60.), (to.0, to.1 + 60.), to], true).await;
    let pv = cx.update(|_, cx| ws.read(cx).panel_drag.as_ref().map(|g| g.target)).unwrap();
    up(cx, to, MouseButton::Left, 1);
    settle(cx).await;
    report("over the explorer's top half the history stays under it", matches!(pv, Some(None)) && panels(&layout(ws, cx).left) == [vec![PanelId::Explorer, PanelId::History]], format!("{pv:?} {:?}", layout(ws, cx)));
    let (tf, ec) = (title(ws, cx), explorer_col(ws, cx));
    report("stacked, the tabs start past their column", ec.is_some_and(|c| (tf.lead - c.right()).abs() < 0.5), format!("{tf:?} {ec:?}"));
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

    // ⌘B hides the explorer where it is, the history stacked with it too, and shows both again.
    command(ws, Box::new(crate::app::ToggleExplorer), cx);
    settle(cx).await;
    let shut = panel(ws, PanelId::Explorer, cx);
    let h4 = panel(ws, PanelId::History, cx);
    report("⌘B hides it and the history stacked with it", shut.is_none() && h4.is_none(), format!("{shut:?} {h4:?}"));
    command(ws, Box::new(crate::app::ToggleExplorer), cx);
    settle(cx).await;
    report("⌘B shows them there again", panel(ws, PanelId::Explorer, cx).is_some_and(|r| near(r, e, 0.01)) && panel(ws, PanelId::History, cx).is_some(), format!("{:?}", panel(ws, PanelId::Explorer, cx)));

    // The history onto the explorer's left edge: the explorer is the leftmost column, so it lands
    // right of it, and the preview says so. "DATAMAN" stays over the explorer.
    if full_stage {
        let (e, h) = (panel(ws, PanelId::Explorer, cx).unwrap(), panel(ws, PanelId::History, cx).unwrap());
        let to = (e.x + 12., e.y + e.h / 2.);
        drag(cx, grip(h), &[(h.x + 200., h.y + 100.), (to.0 + 40., to.1), to], true).await;
        draw(cx);
        let (target, preview) = cx.update(|_, cx| ws.read(cx).panel_drag.as_ref().map(|g| (g.target, g.preview))).unwrap().unwrap_or((None, None));
        let want = cx.update(|window, cx| ws.read(cx).rest_frame(&ws.read(cx).layout.moved(PanelId::History, Place::column(Dock::Left, 1)), window)).unwrap();
        let ex = want.panel(PanelId::Explorer);
        report(
            "left of the explorer resolves to right of it, and the preview shows that spot",
            target == Some(Place::column(Dock::Left, 1)) && preview.is_some() && preview == want.panel(PanelId::History) && ex.zip(preview).is_some_and(|(x, p)| x.x == 0. && (p.x - x.right()).abs() < 0.5 && p.x > to.0),
            format!("{target:?} {preview:?} explorer {e:?}"),
        );
        up(cx, to, MouseButton::Left, 1);
        settle(cx).await;
        let l = layout(ws, cx);
        let (e7, lg) = (panel(ws, PanelId::Explorer, cx), logo(ws, cx));
        report(
            "dropped there, the explorer stays the leftmost column; \"DATAMAN\" over it",
            panels(&l.left) == [vec![PanelId::Explorer], vec![PanelId::History]] && e7.is_some_and(|e| e.x == 0. && logo_over(lg, e)),
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
    let vw = view_w(cx);
    report(
        "Move explorer to the right dock: the outermost column, right of the history",
        panels(&l.right) == [vec![PanelId::History], vec![PanelId::Explorer]] && l.left.is_empty() && e5.zip(h5).is_some_and(|(e, h)| (h.right() - e.x).abs() < 0.01 && (e.right() - vw).abs() < 0.01),
        format!("{l:?} {e5:?} {h5:?}"),
    );
    if let Some(e) = e5 {
        // The header goes with it, mirrored; the search past the traffic lights.
        let (tf, lg, toggle, back, fwd, search) = (title(ws, cx), logo(ws, cx), mark(ws, "toggle", cx), mark(ws, "back", cx), mark(ws, "forward", cx), mark(ws, "search", cx));
        let lights = metrics::lights_right_at(crate::theme::zoom());
        report("\"DATAMAN\" goes with it, at its column's left side", tf.header == Some(Dock::Right) && lg.is_some_and(|l| (l.x - (e.x + 14.)).abs() < 1.5), format!("{lg:?} {e:?}"));
        report(
            "its toggle at the window's right edge, the arrows before it, over its column",
            toggle.zip(back).zip(fwd).is_some_and(|((t, b), f)| (t.right() - (vw - 10.)).abs() < 1.5 && b.x > e.x && b.right() <= f.x + 0.5 && f.right() <= t.x + 0.5),
            format!("{toggle:?} {back:?} {fwd:?}"),
        );
        report("the search sits past the traffic lights, at the left", search.is_some_and(|s| s.x > lights && s.x < lights + 30.), format!("{search:?} lights {lights}"));
        let (tab, end) = (first_tab(ws, cx), strip_end(ws, cx));
        report(
            "the tabs run over the history beside it and end at the explorer",
            h5.zip(tab).is_some_and(|(h, tb)| tb.x < h.x && end > h.x && end <= e.x + 0.5) && tf.joined[1] == 1.,
            format!("{tab:?} end {end} {h5:?} {e:?} {tf:?}"),
        );
        snap("explorer-right", cx);
        // Its toggle, clicked: the explorer hidden, the history beside it stays; again: back.
        if let Some(t) = toggle {
            let c = (t.x + t.w / 2., t.y + t.h / 2.);
            down(cx, c, MouseButton::Left, 1);
            up(cx, c, MouseButton::Left, 1);
            settle(cx).await;
            let hidden = (panel(ws, PanelId::Explorer, cx), panel(ws, PanelId::History, cx), right_w(ws, cx), mark(ws, "toggle", cx));
            report(
                "its toggle hides it, not the history beside it (the header stays at the right edge)",
                hidden.0.is_none() && hidden.1.is_some() && hidden.2 > 0. && hidden.3.is_some_and(|t| (t.right() - (vw - 10.)).abs() < 1.5),
                format!("{hidden:?}"),
            );
            down(cx, c, MouseButton::Left, 1);
            up(cx, c, MouseButton::Left, 1);
            settle(cx).await;
            report("…and shows it again", panel(ws, PanelId::Explorer, cx) == e5 && panel(ws, PanelId::History, cx) == h5, String::new());
        }
    }
    if full_stage {
        // A value sheet leaves the right dock, explorer and all, where it is.
        open_value(cx, ws);
        settle(cx).await;
        let both = (value_open(ws, cx), panel(ws, PanelId::Explorer, cx), panel(ws, PanelId::History, cx));
        report("a value sheet leaves the right dock as it is, explorer and all", both.0 && both.1 == e5 && both.2 == h5, format!("{both:?}"));
        press(cx, "escape");
        settle(cx).await;
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
    if !full_stage {
        return ok;
    }

    // The explorer dragged onto the history's left edge (the right dock): it lands right of it,
    // at the window's edge, and the preview shows exactly that.
    let (explorer, history) = (panel(ws, PanelId::Explorer, cx).unwrap(), panel(ws, PanelId::History, cx).unwrap());
    let vw = view_w(cx);
    let to = (history.x + 20., history.y + history.h / 2.);
    drag(cx, grip(explorer), &[(explorer.right() + 200., to.1), (to.0 + 40., to.1), to], true).await;
    draw(cx);
    pause(cx, 200).await;
    let (target, preview) = cx.update(|_, cx| ws.read(cx).panel_drag.as_ref().map(|g| (g.target, g.preview))).unwrap().unwrap_or((None, None));
    report(
        "left of the history in the right dock resolves to right of it: the preview at the window's edge",
        target == Some(Place::column(Dock::Right, 1)) && preview.is_some_and(|p| (p.right() - vw).abs() < 0.5 && p.x > to.0),
        format!("{target:?} {preview:?}"),
    );
    snap("outermost-preview", cx);
    up(cx, to, MouseButton::Left, 1);
    settle(cx).await;
    let l = layout(ws, cx);
    let (e, h) = (panel(ws, PanelId::Explorer, cx), panel(ws, PanelId::History, cx));
    report(
        "dropped, the explorer is the right dock's outermost column",
        panels(&l.right) == [vec![PanelId::History], vec![PanelId::Explorer]] && e.is_some_and(|e| (e.right() - vw).abs() < 0.01) && e.zip(preview).is_some_and(|(a, b)| near(a, b, 0.01)),
        format!("{l:?} {e:?} {preview:?}"),
    );
    // The history onto the explorer's right edge: it is inside it already, nothing to offer.
    if let (Some(e), Some(h)) = (e, h) {
        let to = (e.right() - 12., e.y + e.h / 2.);
        drag(cx, grip(h), &[(h.x + 60., h.y + 200.), (to.0 - 40., to.1), to], true).await;
        draw(cx);
        let found = cx.update(|_, cx| ws.read(cx).panel_drag.as_ref().map(|g| (g.target, g.preview))).unwrap();
        report("past the explorer's outer edge there is no place for the history", found == Some((None, None)), format!("{found:?}"));
        press(cx, "escape");
        up(cx, to, MouseButton::Left, 1);
        draw(cx);
    }

    // The history alone in the left dock (the explorer on the right): the header slot, the tabs
    // pushed past it; ⌘Y its own again.
    command(ws, Box::new(crate::dock::MoveHistoryLeft), cx);
    settle(cx).await;
    let h = panel(ws, PanelId::History, cx);
    let (tf, tab, search) = (title(ws, cx), first_tab(ws, cx), mark(ws, "search", cx));
    report(
        "alone in the left dock, the history takes the header slot: the tabs start past it",
        h.zip(tab).is_some_and(|(h, tb)| (tf.lead - h.right()).abs() < 0.5 && tb.x >= h.right() && search.is_some_and(|s| s.right() < h.right())) && tf.joined[0] == 0.,
        format!("{tf:?} {tab:?} {h:?} {search:?}"),
    );
    let toggles = history_toggles(ws, cx);
    report("…with ⌘Y, its palette entry and its toolbar button", toggles == toggles0, format!("{toggles:?} vs {toggles0:?}"));
    snap("history-alone-left", cx);
    // The explorer docked into the left dock beside it: the tabs slide over the history.
    let lead0 = tf.lead;
    command(ws, Box::new(crate::dock::MoveExplorerLeft), cx);
    let reduced = cx.update(|_, cx| cx.reduce_motion()).unwrap();
    pause(cx, 90).await;
    draw(cx);
    let mid = title(ws, cx);
    snap("tabs-sliding", cx);
    settle(cx).await;
    let l = layout(ws, cx);
    let (e, h, tf, tab) = (panel(ws, PanelId::Explorer, cx), panel(ws, PanelId::History, cx), title(ws, cx), first_tab(ws, cx));
    report(
        "the explorer docked beside it: the tabs slide over the history (caught mid-way)",
        reduced || (mid.lead < lead0 - 1. && mid.lead > tf.lead + 1. && mid.joined[0] > 0. && mid.joined[0] < 1.),
        format!("{lead0} → {mid:?} → {}", tf.lead),
    );
    report(
        "…and land past the explorer, over the history, which takes its glass",
        panels(&l.left) == [vec![PanelId::Explorer], vec![PanelId::History]]
            && e.zip(h).zip(tab).is_some_and(|((e, h), tb)| e.x == 0. && (tf.lead - e.right()).abs() < 0.5 && tb.x < h.right() && tb.x >= e.right())
            && tf.joined[0] == 1.,
        format!("{l:?} {e:?} {h:?} {tab:?} {tf:?}"),
    );
    // With Reduce Motion the band is simply there.
    if !reduced {
        let _ = cx.update(|_, cx| crate::motion::set_reduce_motion_pref(true, cx));
        command(ws, Box::new(crate::dock::MoveExplorerRight), cx);
        draw(cx);
        let now = (title(ws, cx), cx.update(|_, cx| ws.read(cx).dock_moves.is_empty()).unwrap(), panel(ws, PanelId::History, cx));
        let _ = cx.update(|_, cx| crate::motion::set_reduce_motion_pref(false, cx));
        report(
            "with Reduce Motion the tabs are at once past the history again (its header slot)",
            now.1 && now.0.joined[0] == 0. && now.2.is_some_and(|h| (now.0.lead - h.right()).abs() < 0.5),
            format!("{now:?}"),
        );
    }
    command(ws, Box::new(crate::dock::ResetPanelLayout), cx);
    settle(cx).await;
    report("reset: every panel home", layout(ws, cx).is_default(), format!("{:?}", layout(ws, cx)));
    ok
}
