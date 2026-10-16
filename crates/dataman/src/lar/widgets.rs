//! Pieces the LAR boards repeat: the 40 pt panel header with its pixel-font title, caps
//! section labels, sparklines, small and violet buttons.

use gpui::{
    Div, ElementId, FontWeight, Hsla, ParentElement, SharedString, Stateful, Styled, div,
    prelude::*,
};
use crate::theme::px;

use crate::theme::{PIXEL, Theme};

/// The 40 pt header of a LAR side panel: pixel title in `color`, then whatever follows.
pub fn header(t: &Theme, title: &str, color: Hsla) -> Div {
    div()
        .h(px(40.))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .pl(px(14.))
        .pr(px(10.))
        .border_b_1()
        .border_color(t.line)
        .child(div().font_family(PIXEL).text_size(px(13.)).text_color(color).child(SharedString::from(title.to_string())))
}

/// `NOTIFICATIONS · ch`: 10.5 pt caps in tx3.
pub fn caps(t: &Theme, text: impl Into<SharedString>) -> Div {
    div().text_size(px(10.5)).text_color(t.tx3).child(text.into())
}

/// Vertical bars, oldest left (heights scaled to the largest value, at least 2 pt).
pub fn sparkline(values: &[f32], color: Hsla, height: f32, gap: f32) -> Div {
    let max = values.iter().cloned().fold(0f32, f32::max);
    div().h(px(height)).flex().flex_row().items_end().gap(px(gap)).children(values.iter().map(move |v| {
        let h = if max > 0. { (v / max * height).max(2.) } else { 2. };
        div().flex_1().h(px(h)).bg(color)
    }))
}

/// A small secondary button (24 pt).
pub fn small_btn(t: &Theme, id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    let (h, p) = (t.prs, t.line2);
    div()
        .id(id)
        .flex_none()
        .h(px(24.))
        .px(px(8.))
        .flex()
        .items_center()
        .gap(px(6.))
        .bg(t.hov)
        .border_1()
        .border_color(t.line2)
        .text_color(t.tx)
        .text_size(px(11.))
        .whitespace_nowrap()
        .cursor_pointer()
        .hover(move |s| s.bg(h))
        .active(move |s| s.bg(p))
        .child(label.into())
}

/// A violet 28 pt button (the state card's action).
pub fn lar_btn(t: &Theme, id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    let h = t.lar_h;
    div()
        .id(id)
        .h(px(28.))
        .flex()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .text_size(px(12.))
        .cursor_pointer()
        .bg(t.lar)
        .text_color(t.on_lar)
        .font_weight(FontWeight::SEMIBOLD)
        .hover(move |s| s.bg(h))
        .child(label.into())
}
