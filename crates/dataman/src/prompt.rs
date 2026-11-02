//! `window.prompt` on Linux, where there is no native alert: GPUI's fallback is a narrow white box
//! that clips its text and takes no keys, so prompts are drawn as the app's own sheets. Windows
//! has them too, in place of a TaskDialog: the app's look, and the check stages can answer them. The first
//! button is the default (Enter); Esc answers the Cancel button, else the last.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, PromptButton, PromptHandle, PromptLevel, PromptResponse,
    RenderablePromptHandle, SharedString, Window, div, prelude::*,
};

use crate::theme::{ActiveTheme, px};
use crate::ui;

pub fn build(
    level: PromptLevel,
    message: &str,
    detail: Option<&str>,
    actions: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let view = cx.new(|cx| Prompt {
        level,
        message: message.to_string().into(),
        detail: detail.map(|d| d.to_string().into()),
        actions: actions.iter().map(|a| a.label().clone()).collect(),
        cancel: actions.iter().position(|a| a.is_cancel()).unwrap_or(actions.len().saturating_sub(1)),
        focus: cx.focus_handle(),
    });
    handle.with_view(view, window, cx)
}

struct Prompt {
    level: PromptLevel,
    message: SharedString,
    detail: Option<SharedString>,
    actions: Vec<SharedString>,
    cancel: usize,
    focus: FocusHandle,
}

impl EventEmitter<PromptResponse> for Prompt {}

impl Focusable for Prompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Prompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let t = &*t;
        let cancel = self.cancel;
        let title = div().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).text_color(match self.level {
            PromptLevel::Critical => t.err,
            _ => t.tx,
        });
        // The default last, on the right, as GNOME and KDE place it.
        let buttons = self.actions.iter().enumerate().rev().map(|(i, label)| {
            let b = ui::button(t, ("prompt-button", i), if i == 0 { ui::Btn::Primary } else { ui::Btn::Secondary }).h(px(28.)).child(label.clone());
            let b = if i == 0 {
                b.child(ui::key_hint("↩"))
            } else if i == cancel {
                b.child(ui::key_hint("esc"))
            } else {
                b
            };
            b.on_click(cx.listener(move |_, _, _, cx| cx.emit(PromptResponse(i))))
        });
        div()
            .id("prompt")
            .absolute()
            .inset_0()
            .bg(t.scrim)
            .occlude()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|_, e: &gpui::KeyDownEvent, _, cx| {
                if e.keystroke.key == "enter" && !e.keystroke.modifiers.modified() {
                    cx.stop_propagation();
                    cx.emit(PromptResponse(0));
                }
            }))
            .child(ui::esc_closes(None, cx.listener(move |_, _: &(), _, cx| cx.emit(PromptResponse(cancel)))))
            .flex()
            .items_center()
            .justify_center()
            .child(
                ui::modal(t, "prompt-card")
                    .w(px(420.))
                    .child(
                        div()
                            .px(px(20.))
                            .pt(px(18.))
                            .pb(px(16.))
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .child(title.child(self.message.clone()))
                            .children(self.detail.clone().map(|d| div().text_color(t.tx2).line_height(px(18.)).child(d))),
                    )
                    .child(ui::modal_footer(t).child(div().flex_1()).children(buttons)),
            )
    }
}
