use std::time::Duration;

use dataman_core::layout::{Dock, NavView, PanelId, PanelLayout};
use gpui::{AsyncWindowContext, Entity, KeyDownEvent, KeyUpEvent, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels, Point, point};

use crate::app::Workspace;
use crate::dock::Rect;
use crate::theme::{Pt, metrics, px};

pub fn is_stage(s: &str) -> bool {
    matches!(s, "dock-check" | "dock-drag" | "dock-moved" | "dock-menu")
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

fn snap(name: &str, cx: &mut AsyncWindowContext) {
    let Ok(dir) = std::env::var("DATAMAN_SHOT_DIR") else { return };
    let path = format!("{dir}/dock-{name}.png");
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
    });
}

fn command(ws: &Entity<Workspace>, a: Box<dyn gpui::Action>, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        ws.update(cx, |w, cx| w.focus_editor(window, cx));
        window.dispatch_action(a, cx);
    });
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

async fn settle(cx: &mut AsyncWindowContext) {
    pause(cx, 400).await;
    draw(cx);
}

fn at(x: f32, y: f32) -> Point<Pixels> {
    point(px(x), px(y))
}

fn grip(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2., r.y + 5.)
}

fn panel(ws: &Entity<Workspace>, p: PanelId, cx: &mut AsyncWindowContext) -> Option<Rect> {
    cx.update(|_, cx| ws.read(cx).frame_now.panel(p)).ok().flatten()
}

fn layout(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> PanelLayout {
    cx.update(|_, cx| ws.read(cx).layout.clone()).unwrap()
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
    let ks = crate::keymap::keystroke(k);
    input(cx, PlatformInput::KeyDown(KeyDownEvent { keystroke: ks.clone(), is_held: false, prefer_character_input: false }));
    input(cx, PlatformInput::KeyUp(KeyUpEvent { keystroke: ks }));
}

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

fn logo(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Rect> {
    cx.update(|_, cx| ws.read(cx).logo_bounds.get()).ok().flatten().map(|b| Rect { x: b.origin.x.pt(), y: b.origin.y.pt(), w: b.size.width.pt(), h: b.size.height.pt() })
}

fn logo_over(l: Option<Rect>, c: Rect) -> bool {
    l.is_some_and(|l| l.x >= c.x && (l.right() - (c.right() - 14.)).abs() < 1.5)
}

fn view_w(cx: &mut AsyncWindowContext) -> f32 {
    cx.update(|window, _| window.viewport_size().width.pt()).unwrap()
}

fn mark(ws: &Entity<Workspace>, name: &str, cx: &mut AsyncWindowContext) -> Option<Rect> {
    cx.update(|_, cx| ws.read(cx).title_marks.borrow().get(name).copied()).ok().flatten().map(|b| Rect { x: b.origin.x.pt(), y: b.origin.y.pt(), w: b.size.width.pt(), h: b.size.height.pt() })
}

fn rail(ws: &Entity<Workspace>, v: NavView, cx: &mut AsyncWindowContext) -> Option<Rect> {
    cx.update(|_, cx| ws.read(cx).rail_bounds.borrow().get(&v).copied())
        .ok()
        .flatten()
        .map(|b| Rect { x: b.origin.x.pt(), y: b.origin.y.pt(), w: b.size.width.pt(), h: b.size.height.pt() })
}

fn center(r: Rect) -> (f32, f32) {
    (r.x + r.w / 2., r.y + r.h / 2.)
}

fn click(cx: &mut AsyncWindowContext, p: (f32, f32)) {
    down(cx, p, MouseButton::Left, 1);
    up(cx, p, MouseButton::Left, 1);
}

fn state(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> (NavView, bool, bool) {
    cx.update(|window, cx| {
        let w = ws.read(cx);
        (w.view, w.explorer_open, gpui::Focusable::focus_handle(&w.history, cx).contains_focused(window, cx))
    })
    .unwrap()
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    settle(cx).await;
    let Some(explorer) = panel(ws, PanelId::Explorer, cx) else {
        report("the sidebar on screen", false, String::new());
        return false;
    };
    report("the default arrangement: the sidebar on the left", layout(ws, cx).is_default() && explorer.x == 0., format!("{explorer:?}"));
    let icons = NavView::ALL.map(|v| rail(ws, v, cx));
    report(
        "the rail: three icons down the window's left edge, inside the sidebar, under the title bar",
        icons.iter().all(|r| r.is_some_and(|r| r.x >= 0. && r.right() <= metrics::RAIL_W && r.y >= metrics::TITLE_H)) && icons.windows(2).all(|p| p[0].unwrap().y < p[1].unwrap().y),
        format!("{icons:?}"),
    );
    let logo0 = logo(ws, cx);
    report("\"DATAMAN\" sits over the sidebar, at its right edge", logo_over(logo0, explorer), format!("{logo0:?} {explorer:?}"));
    let toolbar = cx.update(|_, cx| ws.read(cx).palette_items(cx).iter().any(|i| i.key == "⌘Y")).unwrap();
    report("no ⌘Y any more", !toolbar, String::new());

    let fp0 = cx.update(|_, cx| ws.read(cx).session_fingerprint(cx)).unwrap();
    if let Some(f) = icons[1] {
        click(cx, center(f));
    }
    settle(cx).await;
    let st = state(ws, cx);
    let fav = cx.update(|_, cx| ws.read(cx).history.read(cx).showing_favorites()).unwrap();
    report(
        "Favorites: its view in the sidebar's place, focused",
        st == (NavView::Favorites, true, true) && fav && panel(ws, PanelId::Explorer, cx) == Some(explorer) && laid_right(ws, Dock::Left, cx),
        format!("{st:?} {fav}"),
    );
    snap("favorites", cx);
    let fp1 = cx.update(|_, cx| ws.read(cx).session_fingerprint(cx)).unwrap();
    report("the session notices the view", fp1 != fp0, String::new());
    if let Some(h) = icons[2] {
        click(cx, center(h));
    }
    settle(cx).await;
    let st = state(ws, cx);
    let fav = cx.update(|_, cx| ws.read(cx).history.read(cx).showing_favorites()).unwrap();
    report("History: its view in the same place", st == (NavView::History, true, true) && !fav && panel(ws, PanelId::Explorer, cx) == Some(explorer), format!("{st:?} {fav}"));
    snap("history", cx);
    if let Some(h) = icons[2] {
        click(cx, center(h));
    }
    settle(cx).await;
    let st = state(ws, cx);
    let gone = panel(ws, PanelId::Explorer, cx).is_none();
    report("a click on the icon showing hides the sidebar (the rail too), the focus back in the editor", st == (NavView::History, false, false) && gone, format!("{st:?} {gone}"));
    snap("hidden", cx);
    press(cx, "cmd-b");
    settle(cx).await;
    let st = state(ws, cx);
    report("⌘B: back on the history", st.0 == NavView::History && st.1 && panel(ws, PanelId::Explorer, cx) == Some(explorer), format!("{st:?}"));
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.focus_region(crate::app::Region::Explorer, window, cx)));
    press(cx, "cmd-b");
    settle(cx).await;
    let st = state(ws, cx);
    report("⌘B from inside the history hides it and gives the focus back", !st.1 && !st.2, format!("{st:?}"));
    press(cx, "cmd-b");
    settle(cx).await;
    if let Some(e) = rail(ws, NavView::Explorer, cx) {
        click(cx, center(e));
    }
    settle(cx).await;
    report("Explorer again", state(ws, cx).0 == NavView::Explorer, format!("{:?}", state(ws, cx)));

    command(ws, Box::new(crate::app::ToggleMcpLog), cx);
    settle(cx).await;
    let side = panel(ws, PanelId::Side, cx);
    report("the side panel opens in the right dock", side.is_some_and(|s| (s.right() - view_w(cx)).abs() < 0.5), format!("{side:?}"));
    if let Some(sd) = side {
        drag(cx, grip(sd), &[(sd.x - 300., sd.y + 200.), (60., sd.y + 200.)], false).await;
        settle(cx).await;
        report("the side panel does not move on its own", layout(ws, cx).is_default(), format!("{:?}", layout(ws, cx)));
    }

    let c = cx.update(|_, cx| ws.read(cx).frame_now.center).unwrap();
    let start = grip(explorer);
    let near_side = (c.x + c.w * 0.3, c.y + 200.);
    drag(cx, start, &[(start.0 + 80., start.1 + 100.), near_side], true).await;
    draw(cx);
    let found = cx.update(|_, cx| ws.read(cx).panel_drag.as_ref().map(|g| (g.target, g.preview))).unwrap();
    report("on its own side, the drag offers nothing", found == Some((None, None)), format!("{found:?}"));
    press(cx, "escape");
    let (dragging, none_left) = cx.update(|_, cx| (cx.has_active_drag(), ws.read(cx).panel_drag.is_none())).unwrap();
    up(cx, near_side, MouseButton::Left, 1);
    settle(cx).await;
    report("esc lets go of the drag: nothing drops", !dragging && none_left && layout(ws, cx).is_default(), format!("dragging {dragging}"));

    let far = (c.x + c.w * 0.8, c.y + 200.);
    drag(cx, start, &[(start.0 + 200., start.1 + 100.), (c.x + c.w * 0.6, c.y + 200.), far], true).await;
    draw(cx);
    let preview = cx.update(|_, cx| ws.read(cx).panel_drag.as_ref().and_then(|g| g.preview)).unwrap();
    if stage == "dock-drag" {
        return ok;
    }
    up(cx, far, MouseButton::Left, 1);
    settle(cx).await;
    let (l, e, sd, vw) = (layout(ws, cx), panel(ws, PanelId::Explorer, cx), panel(ws, PanelId::Side, cx), view_w(cx));
    report(
        "dropped past the middle: the sidebar on the right (where the preview was), the side panel on the left",
        l == PanelLayout::explorer_at(Dock::Right)
            && e.is_some_and(|e| (e.right() - vw).abs() < 0.5)
            && e.zip(preview).is_some_and(|(a, b)| near(a, b, 0.5))
            && sd.is_some_and(|s| s.x == 0.),
        format!("{l:?} {e:?} {preview:?} {sd:?}"),
    );
    let r = NavView::ALL.map(|v| rail(ws, v, cx));
    report(
        "the rail went with it, at the window's right edge",
        r.iter().all(|r| r.is_some_and(|r| r.right() <= vw && r.x >= vw - metrics::RAIL_W)),
        format!("{r:?}"),
    );
    let (lg, toggle) = (logo(ws, cx), mark(ws, "toggle", cx));
    report(
        "the header too: \"DATAMAN\" over the sidebar, the toggle at the right edge",
        e.zip(lg).is_some_and(|(e, l)| l.x >= e.x && l.right() <= e.right()) && toggle.is_some_and(|t| t.right() > vw - 40.),
        format!("{lg:?} {toggle:?} {e:?}"),
    );
    report("both docks laid out where the frame says", laid_right(ws, Dock::Left, cx) && laid_right(ws, Dock::Right, cx), format!("{:?}", laid_out(ws, Dock::Right, cx)));
    if stage == "dock-moved" {
        return ok;
    }
    if let Some(f) = rail(ws, NavView::Favorites, cx) {
        click(cx, center(f));
    }
    settle(cx).await;
    report("on the right, Favorites in the same place", state(ws, cx).0 == NavView::Favorites && panel(ws, PanelId::Explorer, cx) == e, format!("{:?}", state(ws, cx)));

    if let Some(e) = e {
        let p = grip(e);
        down(cx, p, MouseButton::Right, 1);
        up(cx, p, MouseButton::Right, 1);
        draw(cx);
        let open = cx.update(|_, cx| ws.read(cx).panel_menu.is_some()).unwrap();
        report("a right click on its grip opens its menu", open, String::new());
        if stage == "dock-menu" {
            return ok;
        }
        press(cx, "escape");
        let _ = cx.update(|_, cx| ws.update(cx, |w, cx| {
            w.panel_menu = None;
            cx.notify();
        }));
    }
    command(ws, Box::new(crate::dock::MoveExplorerLeft), cx);
    settle(cx).await;
    report("\"Move sidebar to the left\": home again, the side panel right", layout(ws, cx).is_default() && panel(ws, PanelId::Side, cx).is_some_and(|s| (s.right() - view_w(cx)).abs() < 0.5), format!("{:?}", layout(ws, cx)));
    command(ws, Box::new(crate::dock::MoveExplorerRight), cx);
    settle(cx).await;
    command(ws, Box::new(crate::dock::ResetPanelLayout), cx);
    settle(cx).await;
    report("reset: the sidebar home", layout(ws, cx).is_default(), format!("{:?}", layout(ws, cx)));
    ok
}
