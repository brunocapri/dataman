//! Small shared components from the Components board: keycaps, buttons, pills, chips,
//! tooltips, segmented controls, checkboxes, menus, the modal card and the pixel-font tags.
//!
//! They are plain element builders (no state): callers add ids, handlers and layout.

use gpui::{
    AnyView, App, Context, Div, ElementId, FontWeight, Hsla, IntoElement, ParentElement, Render, SharedString,
    Stateful, Styled, Window, div, prelude::*,
};
use crate::theme::px;

use crate::assets::{Icon, icon};
use crate::theme::{ActiveTheme, PIXEL, Theme};

/// A keycap (`⌘↩`, `esc`): 18 pt, square, 10.5 pt.
pub fn keycap(t: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .h(px(18.))
        .px(px(5.))
        .flex()
        .items_center()
        .bg(t.key)
        .border_1()
        .border_color(t.line)
        .border_b_1()
        .text_size(px(10.5))
        .line_height(px(16.))
        .text_color(t.tx2)
        .child(text.into())
}

/// The smaller keycap inside buttons (17 pt, 10 pt).
pub fn keycap_sm(bg: Hsla, fg: Hsla, text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .h(px(17.))
        .px(px(4.))
        .flex()
        .items_center()
        .bg(bg)
        .text_size(px(10.))
        .line_height(px(14.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(fg)
        .child(text.into())
}

/// A shortcut hint after a label, in the label's color at 70 % (`Fetch all ⇧⌘↓`).
pub fn key_hint(text: impl Into<SharedString>) -> Div {
    div().flex_none().text_size(px(10.5)).opacity(0.7).child(text.into())
}

/// The 8 pt connection color chip (square, a dark hairline inside).
pub fn chip(color: Hsla, size: f32) -> Div {
    div()
        .flex_none()
        .size(px(size))
        .bg(color)
        .border_1()
        .border_color(gpui::hsla(0., 0., 0., 0.25))
}

/// The violet dot of a LAR server (6 pt).
pub fn lar_dot(t: &Theme) -> Div {
    div().flex_none().size(px(6.)).rounded(px(3.)).bg(t.lar)
}

/// A caps section label (EXPLORER, HISTORY): 10.5 pt, tx3.
pub fn caps(t: &Theme, text: impl Into<SharedString>) -> Div {
    div().flex_none().text_size(px(10.5)).text_color(t.tx3).child(text.into())
}

/// A pixel-font tag: `[ NO RESULT ]`, `[ EMPTY ]`.
pub fn pixel(text: impl Into<SharedString>, size: f32, color: Hsla) -> Div {
    div().font_family(PIXEL).text_size(px(size)).line_height(px(size * 1.15)).text_color(color).child(text.into())
}

/// The `◆ LAR` badge (pixel font on violet). Badges are square, like buttons.
pub fn lar_badge(t: &Theme, text: &'static str) -> Div {
    div()
        .flex_none()
        .h(px(17.))
        .px(px(6.))
        .flex()
        .items_center()
        .bg(t.lar)
        .text_color(t.on_lar)
        .font_family(PIXEL)
        .text_size(px(11.5))
        .child(text)
}

/// The small outlined `LAR` tag of explorer rows.
pub fn lar_tag(t: &Theme) -> Div {
    div()
        .flex_none()
        .h(px(15.))
        .px(px(4.))
        .flex()
        .items_center()
        .border_1()
        .border_color(t.lar)
        .bg(t.lar_s)
        .text_color(t.lar_t)
        .text_size(px(9.5))
        .child("LAR")
}

/// The dashed "not reported" pill: a catalog gap, never an error.
pub fn not_reported(t: &Theme) -> Div {
    div()
        .flex_none()
        .h(px(15.))
        .px(px(4.))
        .flex()
        .items_center()
        .border_1()
        .border_dashed()
        .border_color(t.line2)
        .text_color(t.tx3)
        .text_size(px(10.))
        .child("not reported")
}

/// A state badge (`▶ RUNNING`, `‖ PAUSED`, `✓ DONE`…): 20 pt, square, caps 10.5 semibold.
pub fn pill(glyph: &str, label: impl Into<SharedString>, fg: Hsla, bg: Hsla) -> Div {
    div()
        .flex_none()
        .h(px(20.))
        .px(px(7.))
        .flex()
        .items_center()
        .gap(px(6.))
        .bg(bg)
        .text_color(fg)
        .text_size(px(10.5))
        .font_weight(FontWeight::SEMIBOLD)
        .whitespace_nowrap()
        .child(div().text_size(px(10.)).child(SharedString::from(glyph.to_string())))
        .child(label.into())
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Btn {
    /// Accent fill (Run, Save & connect).
    Primary,
    /// Hover-tinted with a ring (Fetch all, Test).
    Secondary,
    /// Transparent (History, Cancel in dialogs).
    Ghost,
    /// Error-tinted with an error ring (Cancel while running).
    Danger,
    /// LAR fill.
    Lar,
}

/// A 26 pt button (square, padding 0 9) with its hover and pressed states. Everything is
/// square except the result grid's cell selection (and the dots and knobs, which are glyphs).
pub fn button(t: &Theme, id: impl Into<ElementId>, kind: Btn) -> Stateful<Div> {
    let base = div()
        .id(id)
        .flex_none()
        .h(px(26.))
        .px(px(9.))
        .flex()
        .items_center()
        .gap(px(7.))
        .text_size(px(12.))
        .whitespace_nowrap();
    match kind {
        Btn::Primary => {
            let (h, p) = (t.acc_h, t.acc_p);
            base.bg(t.acc).text_color(t.on_acc).hover(move |s| s.bg(h)).active(move |s| s.bg(p))
        }
        Btn::Secondary => {
            let (h, p) = (t.prs, t.line2);
            base.bg(t.hov).border_1().border_color(t.line2).text_color(t.tx).hover(move |s| s.bg(h)).active(move |s| s.bg(p))
        }
        Btn::Ghost => {
            let (h, p) = (t.hov, t.prs);
            base.text_color(t.tx2).hover(move |s| s.bg(h)).active(move |s| s.bg(p))
        }
        Btn::Danger => {
            let (h, p) = (t.err.opacity(0.22), t.err.opacity(0.30));
            base.bg(t.err_s).border_1().border_color(t.err).text_color(t.err).hover(move |s| s.bg(h)).active(move |s| s.bg(p))
        }
        Btn::Lar => {
            let (h, p) = (t.lar_h, t.lar_p);
            base.bg(t.lar).text_color(t.on_lar).hover(move |s| s.bg(h)).active(move |s| s.bg(p))
        }
    }
}

/// A square icon button (24 or 26 pt) in tx3 that lights up on hover.
pub fn icon_button(t: &Theme, id: impl Into<ElementId>, i: Icon, box_size: f32, icon_size: f32) -> Stateful<Div> {
    let (hov, prs) = (t.hov, t.prs);
    div()
        .id(id)
        .flex_none()
        .size(px(box_size))
        .flex()
        .items_center()
        .justify_center()
        .text_color(t.tx3)
        .hover(move |s| s.bg(hov))
        .active(move |s| s.bg(prs))
        .child(icon(i).size(px(icon_size)).text_color(t.tx3))
}

/// A focused field's halo: a 3 pt band of `color` just outside its 1 pt border, drawn as a
/// border of its own (the field must be `relative`). Not a spread `BoxShadow`: GPUI paints a
/// shadow under the whole box, which would also tint a translucent field's inside.
pub fn halo_ring(color: Hsla) -> Div {
    // Absolute insets count from inside the field's border (1 pt at any zoom, like every
    // hairline); the band itself scales with the UI.
    let o = px(-3.) - gpui::px(1.);
    div().absolute().top(o).left(o).right(o).bottom(o).border(px(3.)).border_color(color)
}

/// A vertical separator (1 x 18, line2).
pub fn vsep(t: &Theme, h: f32) -> Div {
    div().flex_none().w(px(1.)).h(px(h)).bg(t.line2)
}

/// The switch (26 x 14, a square track, a round knob).
pub fn toggle(t: &Theme, on: bool) -> Div {
    div()
        .flex_none()
        .w(px(26.))
        .h(px(14.))
        .relative()
        .bg(if on { t.acc } else { t.line2 })
        .child(
            div()
                .absolute()
                .top(px(2.))
                .when(on, |d| d.right(px(2.)))
                .when(!on, |d| d.left(px(2.)))
                .size(px(10.))
                .rounded(px(5.))
                .bg(if on { gpui::white() } else { t.tx3 }),
        )
}

/// A 14 pt checkbox.
pub fn checkbox(t: &Theme, on: bool) -> Div {
    div()
        .flex_none()
        .size(px(14.))
        .flex()
        .items_center()
        .justify_center()
        .when(on, |d| d.bg(t.acc).child(icon(Icon::Check).size(px(11.)).text_color(t.on_acc)))
        .when(!on, |d| d.bg(t.field).border_1().border_color(t.line2))
}

/// The segmented control container (28 pt, field background, line2 ring; square, a button).
pub fn segmented(t: &Theme) -> Div {
    div().flex().flex_row().h(px(28.)).p(px(2.)).bg(t.field).border_1().border_color(t.line2)
}

/// One segment; the selected one is a raised `pop` tile.
pub fn segment(t: &Theme, id: impl Into<ElementId>, label: impl Into<SharedString>, on: bool) -> Stateful<Div> {
    let hov = t.hov;
    div()
        .id(id)
        .flex_1()
        .px(px(10.))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(11.5))
        .when(on, |d| d.bg(t.pop).text_color(t.tx).border_1().border_color(t.line2))
        .when(!on, |d| d.text_color(t.tx3).hover(move |s| s.bg(hov)))
        .child(label.into())
}

/// A popover container (menus, autocomplete): pop background, square, the `line2` ring
/// (no drop shadow).
pub fn popover(t: &Theme) -> Div {
    div().bg(t.pop).shadow(t.ring()).p(px(4.)).occlude()
}

/// A 26 pt menu item with a right-aligned shortcut.
pub fn menu_item(
    t: &Theme,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    key: Option<&str>,
    enabled: bool,
) -> Stateful<Div> {
    let (acc, on_acc) = (t.acc, t.on_acc);
    div()
        .id(id)
        .h(px(26.))
        .px(px(10.))
        .flex()
        .items_center()
        .gap(px(10.))
        .text_size(px(12.))
        .text_color(if enabled { t.tx } else { t.tx4 })
        .when(enabled, |d| d.hover(move |s| s.bg(acc).text_color(on_acc)))
        .child(div().flex_1().child(label.into()))
        .children(key.map(|k| div().text_size(px(11.)).opacity(0.7).child(k.to_string())))
}

pub fn menu_sep(t: &Theme) -> Div {
    div().h(px(1.)).mx(px(6.)).my(px(4.)).bg(t.line)
}

/// A tooltip: a title, an optional line of explanation and an optional keycap.
pub struct Tooltip {
    title: SharedString,
    desc: Option<SharedString>,
    key: Option<SharedString>,
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .max_w(px(320.))
            .px(px(11.))
            .py(px(9.))
            .bg(t.pop)
            .shadow(t.ring())
            .font_family(crate::theme::mono())
            .text_size(px(11.5))
            .line_height(px(17.))
            .text_color(t.tx2)
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .text_color(t.tx)
                    .child(self.title.clone())
                    .children(self.key.clone().map(|k| keycap(&t, k))),
            )
            .children(self.desc.clone())
    }
}

/// Builds a tooltip callback for `.tooltip(…)`.
pub fn tooltip(
    title: impl Into<SharedString>,
    desc: Option<&str>,
    key: Option<&str>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let title = title.into();
    let desc: Option<SharedString> = desc.map(|d| SharedString::from(d.to_string()));
    let key: Option<SharedString> = key.map(|k| SharedString::from(k.to_string()));
    move |_, cx| {
        let (title, desc, key) = (title.clone(), desc.clone(), key.clone());
        cx.new(|_| Tooltip { title, desc, key }).into()
    }
}

/// The widest a text tooltip gets, and the most lines it shows.
pub const TIP_MAX_W: f32 = 360.;
pub const TIP_LINES: usize = 14;
/// The tooltip's mono 11.5 pt: an advance of about 0.6 em; its padding.
const TIP_CHAR: f32 = 6.9;
const TIP_PAD_X: f32 = 11.;
const TIP_LINE_H: f32 = 17.;

/// A long text cut to fit a tooltip: at most [`TIP_LINES`] lines of what fits in
/// [`TIP_MAX_W`] (wrapped lines counted a little short, words wrap before the edge), the rest
/// replaced by an ellipsis; and the width it needs (narrow texts get a narrow box).
pub fn fit_tip(text: &str) -> (String, f32) {
    let per_line = ((TIP_MAX_W - 2. * TIP_PAD_X) / TIP_CHAR).floor() as usize;
    let wraps = (per_line * 17 / 20).max(1);
    let width = |longest: usize| (longest as f32 * TIP_CHAR + 2. * TIP_PAD_X + 2.).clamp(60., TIP_MAX_W);
    let mut out = String::new();
    let (mut lines, mut longest) = (0usize, 0usize);
    for line in text.trim().lines() {
        let line = line.trim_end();
        let n = line.chars().count();
        let rows = n.div_ceil(wraps).max(1);
        if lines + rows > TIP_LINES {
            let room = (TIP_LINES - lines) * wraps;
            if room == 0 {
                // No line left: the ellipsis ends the last one.
                out.pop();
            } else {
                let cut: String = line.chars().take(room.saturating_sub(1)).collect();
                out.push_str(cut.trim_end());
            }
            out.push('…');
            return (out, width(per_line.max(longest)));
        }
        lines += rows;
        longest = longest.max(n.min(per_line));
        out.push_str(line);
        out.push('\n');
    }
    out.pop();
    (out, width(longest))
}

/// A tooltip for a long text (a connection's error): wrapped, at most [`TIP_MAX_W`] wide and
/// [`TIP_LINES`] lines high, so it always fits the window (GPUI moves it left or above the
/// pointer near the edges).
pub struct TextTip {
    text: SharedString,
    width: f32,
}

impl Render for TextTip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .w(px(self.width))
            .max_h(px(TIP_LINES as f32 * TIP_LINE_H + 20.))
            .overflow_hidden()
            .px(px(TIP_PAD_X))
            .py(px(9.))
            .bg(t.pop)
            .shadow(t.ring())
            .font_family(crate::theme::mono())
            .text_size(px(11.5))
            .line_height(px(TIP_LINE_H))
            .text_color(t.tx)
            .whitespace_normal()
            .child(div().w_full().line_clamp(TIP_LINES).text_ellipsis().child(self.text.clone()))
    }
}

/// Builds a [`TextTip`] callback for `.tooltip(…)`.
pub fn text_tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let (text, width) = fit_tip(&text.into());
    let text = SharedString::from(text);
    move |_, cx| {
        let text = text.clone();
        cx.new(|_| TextTip { text, width }).into()
    }
}

/// A modal card (connection sheet, export, import, settings, confirmations): `pop`, square,
/// the 1 pt ring, and clicks stop here (the scrim behind closes it).
pub fn modal(t: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        // Never past the window (a small one, or a zoomed UI): its own size is the most.
        .max_w(gpui::relative(0.96))
        .max_h(gpui::relative(0.94))
        .flex()
        .flex_col()
        .bg(t.pop)
        .shadow(t.ring())
        .text_size(px(12.))
        .text_color(t.tx)
        .occlude()
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// A modal's header (the connection sheet's): an optional leading mark, the title, what else
/// the sheet shows there (its target), and the close button, over a hairline.
pub fn modal_header(t: &Theme, leading: Option<gpui::AnyElement>, title: impl Into<SharedString>, close: impl IntoElement) -> Div {
    modal_header_with(t, leading, title, None, close)
}

pub fn modal_header_with(
    t: &Theme,
    leading: Option<gpui::AnyElement>,
    title: impl Into<SharedString>,
    extra: Option<gpui::AnyElement>,
    close: impl IntoElement,
) -> Div {
    div()
        .h(px(52.))
        .flex_none()
        .pl(px(20.))
        .pr(px(16.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.))
        .border_b_1()
        .border_color(t.line)
        .children(leading)
        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).child(title.into()))
        .children(extra)
        .child(close)
}

/// The × of a modal's header (add its `on_click`).
pub fn modal_close(t: &Theme, id: impl Into<ElementId>) -> Stateful<Div> {
    icon_button(t, id, Icon::Close, 26., 14.)
}

/// A modal's footer: its buttons in a row under a hairline (no fill, see [`modal`]).
pub fn modal_footer(t: &Theme) -> Div {
    div()
        .h(px(56.))
        .flex_none()
        .pl(px(20.))
        .pr(px(16.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .border_t_1()
        .border_color(t.line)
}

/// An empty-state block: a pixel tag and, at most, one short line.
pub fn empty_state(t: &Theme, tag: &str, size: f32, msg: Option<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(10.))
        .text_center()
        .child(pixel(SharedString::from(tag.to_string()), size, t.tx4))
        .children(msg.map(|m| div().text_color(t.tx2).line_height(px(18.)).child(m)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short text gets a narrow box and stays whole; a huge one is cut to the line budget,
    /// ends with an ellipsis and never asks for more than the widest box.
    #[test]
    fn text_tips_fit() {
        let (t, w) = fit_tip("connection refused");
        assert_eq!(t, "connection refused");
        assert!(w < 200. && w >= 60., "{w}");
        let huge = format!("FATAL: {}\nDETAIL: {}\n{}", "x".repeat(900), "y ".repeat(300), "z\n".repeat(50));
        let (t, w) = fit_tip(&huge);
        assert!(w > 340. && w <= TIP_MAX_W, "{w}");
        assert!(t.ends_with('…') && t.chars().count() < 900, "{}", t.len());
        let (t, _) = fit_tip(&"line\n".repeat(40));
        assert_eq!(t.lines().count(), TIP_LINES);
        assert!(t.ends_with('…'));
    }
}
