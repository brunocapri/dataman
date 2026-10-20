use std::time::Duration;

use gpui::{
    AsyncWindowContext, ClipboardItem, Entity, Focusable, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PlatformInput, Pixels, Point, point, px,
};

use crate::app::Workspace;
use crate::input::TextInput;

pub fn is_stage(s: &str) -> bool {
    s == "input-check" || s == "input-scroll"
}

const LONG: &str = "thequickbrownfoxjumpsoverthelazydogandthequickbrownfoxjumpsoverthelazycat";

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
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(Keystroke::parse(k).unwrap(), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn typ(cx: &mut AsyncWindowContext, s: &str) {
    for c in s.chars() {
        key(cx, &c.to_string());
    }
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn state(f: &Entity<TextInput>, cx: &mut AsyncWindowContext) -> (f32, f32, (f32, f32, f32, f32)) {
    draw(cx);
    cx.update(|_, cx| {
        let i = f.read(cx);
        (i.demo_scroll(), i.demo_caret_x().unwrap_or(-1.), i.demo_bounds().unwrap_or_default())
    })
    .unwrap_or_default()
}

fn mouse(cx: &mut AsyncWindowContext, p: Point<Pixels>, down: Option<bool>, count: usize) {
    let m = Modifiers::default();
    let e = match down {
        None => PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: Some(MouseButton::Left), modifiers: m }),
        Some(true) => PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: p, modifiers: m, click_count: count, first_mouse: false }),
        Some(false) => PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: p, modifiers: m, click_count: count }),
    };
    input(cx, e);
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let Ok(tree) = cx.update(|_, cx| ws.read(cx).tree.clone()) else { return false };
    let f = cx.update(|_, cx| tree.read(cx).demo_filter()).unwrap();
    let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.demo_focus_filter(window, cx)));
    typ(cx, LONG);
    if stage == "input-scroll" {
        for _ in 0..8 {
            key(cx, "shift-left");
        }
        draw(cx);
        return true;
    }

    let (s, x, (bx, by, bw, bh)) = state(&f, cx);
    report("typing past the edge scrolls with the caret", s > 0. && (x - bw).abs() < 1.5, format!("scroll={s} caret={x} width={bw}"));
    let end = s;

    key(cx, "cmd-left");
    let (s, x, _) = state(&f, cx);
    report("⌘← shows the start", s == 0. && x == 0., format!("scroll={s} caret={x}"));
    key(cx, "cmd-right");
    let (s, x, _) = state(&f, cx);
    report("⌘→ shows the end", (s - end).abs() < 0.5 && (x - bw).abs() < 1.5, format!("scroll={s} (end {end}) caret={x}"));

    for _ in 0..5 {
        key(cx, "left");
    }
    let (s, x, _) = state(&f, cx);
    report("arrows inside the view leave the scroll", (s - end).abs() < 0.5 && x < bw && x > 0., format!("scroll={s} caret={x}"));

    let at = point(px(bx + bw * 0.4), px(by + bh / 2.));
    mouse(cx, at, None, 0);
    mouse(cx, at, Some(true), 1);
    mouse(cx, at, Some(false), 1);
    let (s, x, _) = state(&f, cx);
    let sel = cx.update(|_, cx| f.read(cx).demo_selected()).unwrap();
    report(
        "a click on scrolled text lands on the character under it",
        (s - end).abs() < 0.5 && (x - bw * 0.4).abs() < 6. && sel.start > LONG.len() / 2,
        format!("scroll={s} caret={x} want≈{} sel={sel:?}", bw * 0.4),
    );

    let from = point(px(bx + bw * 0.6), px(by + bh / 2.));
    mouse(cx, from, None, 0);
    mouse(cx, from, Some(true), 1);
    let past = point(px(bx - 24.), px(by + bh / 2.));
    mouse(cx, past, None, 0);
    let (first, _, _) = state(&f, cx);
    pause(cx, 2500).await;
    let (s, _, _) = state(&f, cx);
    let sel = cx.update(|_, cx| f.read(cx).demo_selected()).unwrap();
    mouse(cx, past, Some(false), 1);
    report(
        "a drag resting past the edge scrolls to the start, selecting",
        first > 0. && s == 0. && sel.start == 0 && sel.end > LONG.len() / 2,
        format!("scroll {first} → {s} sel={sel:?}"),
    );

    let saved = cx.update(|_, cx| cx.read_from_clipboard()).ok().flatten();
    let _ = cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string(format!("{LONG}x{LONG}"))));
    key(cx, "cmd-a");
    key(cx, "cmd-v");
    let (s, x, _) = state(&f, cx);
    let n = cx.update(|_, cx| f.read(cx).text().len()).unwrap();
    report("a long paste ends in view", n == 2 * LONG.len() + 1 && s > end && (x - bw).abs() < 1.5, format!("len={n} scroll={s} caret={x}"));
    let _ = cx.update(|_, cx| match saved {
        Some(item) => cx.write_to_clipboard(item),
        None => cx.write_to_clipboard(ClipboardItem::new_string(String::new())),
    });

    for _ in 0..20 {
        key(cx, "backspace");
    }
    let (s, x, _) = state(&f, cx);
    report("deleting scrolls back, no room left on the right", s > 0. && (x - bw).abs() < 1.5, format!("scroll={s} caret={x}"));
    key(cx, "cmd-backspace");
    typ(cx, "orders");
    let (s, x, _) = state(&f, cx);
    report("a text that fits does not scroll", s == 0. && x > 0. && x < bw, format!("scroll={s} caret={x}"));

    typ(cx, LONG);
    let (s, _, _) = state(&f, cx);
    let _ = cx.update(|window, cx| window.focus(&tree.focus_handle(cx), cx));
    let (s2, _, _) = state(&f, cx);
    report("leaving the field shows its start", s > 0. && s2 == 0., format!("scroll {s} → {s2}"));
    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.set_filter("", cx)));
    ok
}
