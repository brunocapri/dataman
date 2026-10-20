use gpui::{App, AppContext, Bounds, Context, Div, Entity, Hsla, IntoElement, ParentElement, Pixels, Point, Render, SharedString, Styled, WeakEntity, Window, div, prelude::FluentBuilder};

use crate::app::Workspace;
use crate::theme::{ActiveTheme, Theme, px};
use crate::ui;

pub struct Ghost {
    label: SharedString,
    chip: Option<Hsla>,
}

impl Render for Ghost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .h(px(24.))
            .max_w(px(240.))
            .px(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .bg(t.pop.opacity(0.7))
            .border_1()
            .border_color(t.line2.opacity(0.6))
            .font_family(crate::theme::mono())
            .text_size(px(12.))
            .text_color(t.tx2)
            .opacity(0.8)
            .whitespace_nowrap()
            .overflow_hidden()
            .when_some(self.chip, |d, c| d.child(ui::chip(c, 8.)))
            .child(self.label.clone())
    }
}

pub fn ghost(label: impl Into<SharedString>, chip: Option<Hsla>, cx: &mut App) -> Entity<Ghost> {
    let label = label.into();
    cx.new(|_| Ghost { label, chip })
}

/// A keystroke interceptor runs before any binding and any key handler, in every window.
pub(crate) fn cancel_on_escape(ws: WeakEntity<Workspace>, cx: &mut App) {
    cx.intercept_keystrokes(move |ev, window, cx| {
        if ev.keystroke.key != "escape" || !cx.has_active_drag() {
            return;
        }
        cx.stop_active_drag(window);
        cx.stop_propagation();
        let _ = ws.update(cx, |w, cx| w.drag_cancelled(cx));
    })
    .detach();
}

pub fn slot_at(i: usize, bounds: &Bounds<Pixels>, pos: Point<Pixels>, horizontal: bool) -> Option<usize> {
    if !bounds.contains(&pos) {
        return None;
    }
    let mid = bounds.center();
    let past = if horizontal { pos.x > mid.x } else { pos.y > mid.y };
    Some(if past { i + 1 } else { i })
}

pub fn drop_line(t: &Theme, horizontal: bool, after: bool) -> Div {
    let d = div().absolute().bg(t.acc.opacity(0.45));
    match (horizontal, after) {
        (true, false) => d.top_0().bottom_0().left(px(-1.5)).w(px(1.)),
        (true, true) => d.top_0().bottom_0().right(px(-1.5)).w(px(1.)),
        (false, false) => d.left_0().right_0().top_0().h(px(1.)),
        (false, true) => d.left_0().right_0().bottom_0().h(px(1.)),
    }
}
