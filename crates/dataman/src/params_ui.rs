//! The parameter prompt: running a statement with `$1 … $n` or `:name` asks for one value per
//! parameter (with its type when the statement shows it), prefilled with the tab's last values.
//! ↩ moves to the next field (and runs from the last), ⇥ / ⇧⇥ move between fields, ⌘↩ runs,
//! esc cancels. `NULL` binds SQL NULL.

use gpui::{App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding, SharedString, Window, actions, div, prelude::*};
use crate::theme::px;

use crate::input::{InputEvent, TextInput};
use crate::theme::{ActiveTheme, mono};
use crate::ui::{self, Btn};

actions!(param_prompt, [NextField, PrevField, RunParams, CancelParams]);

const CONTEXT: &str = "ParamPrompt";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("tab", NextField, c),
        KeyBinding::new("shift-tab", PrevField, c),
        KeyBinding::new("cmd-enter", RunParams, c),
        KeyBinding::new("escape", CancelParams, c),
    ]);
}

pub enum ParamEvent {
    Run,
    Cancel,
}

pub struct ParamRow {
    pub key: String,
    pub hint: Option<String>,
    pub input: Entity<TextInput>,
}

pub struct ParamPrompt {
    pub rows: Vec<ParamRow>,
    focus: FocusHandle,
    /// The field ↩ moves to (focused on the next render, which has the window).
    pending_focus: Option<usize>,
}

impl EventEmitter<ParamEvent> for ParamPrompt {}

impl Focusable for ParamPrompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ParamPrompt {
    /// One row per (key, type hint, last value).
    pub fn new(params: Vec<(String, Option<String>, String)>, cx: &mut Context<Self>) -> Self {
        let mut rows = Vec::with_capacity(params.len());
        for (i, (key, hint, value)) in params.into_iter().enumerate() {
            let input = cx.new(|cx| {
                let mut t = TextInput::new(cx, "");
                t.set_text(value, cx);
                t.select_all(cx);
                t
            });
            cx.subscribe(&input, move |p: &mut ParamPrompt, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Submit) {
                    if i + 1 == p.rows.len() {
                        cx.emit(ParamEvent::Run);
                    } else {
                        p.pending_focus = Some(i + 1);
                        cx.notify();
                    }
                }
            })
            .detach();
            rows.push(ParamRow { key, hint, input });
        }
        ParamPrompt { rows, focus: cx.focus_handle(), pending_focus: None }
    }

    /// The values typed, by key.
    pub fn values(&self, cx: &App) -> Vec<(String, String)> {
        self.rows.iter().map(|r| (r.key.clone(), r.input.read(cx).text().to_string())).collect()
    }

    pub fn focus_field(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(r) = self.rows.get(i) {
            let f = r.input.focus_handle(cx);
            window.focus(&f, cx);
            r.input.update(cx, |t, cx| t.select_all(cx));
        }
    }

    fn focused_row(&self, window: &Window, cx: &App) -> Option<usize> {
        self.rows.iter().position(|r| r.input.focus_handle(cx).is_focused(window))
    }

    fn step(&mut self, d: isize, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.rows.len() as isize;
        if n == 0 {
            return;
        }
        let i = self.focused_row(window, cx).map_or(0, |i| i as isize);
        self.focus_field(((i + d).rem_euclid(n)) as usize, window, cx);
    }
}

impl Render for ParamPrompt {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(i) = self.pending_focus.take() {
            self.focus_field(i, window, cx);
        }
        let t = cx.theme();
        let rows = self.rows.iter().map(|r| {
            div()
                .h(px(30.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .child(div().w(px(96.)).flex_none().overflow_hidden().whitespace_nowrap().text_ellipsis().font_family(mono()).text_color(t.param).child(r.key.clone()))
                .child(div().flex_1().min_w_0().child(r.input.clone()))
                .child(
                    div()
                        .w(px(120.))
                        .flex_none()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .font_family(mono())
                        .text_size(px(11.))
                        .text_color(t.typ)
                        .child(SharedString::from(r.hint.clone().unwrap_or_default())),
                )
        });
        ui::modal(&t, "param-prompt")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|p, _: &NextField, w, cx| p.step(1, w, cx)))
            .on_action(cx.listener(|p, _: &PrevField, w, cx| p.step(-1, w, cx)))
            .on_action(cx.listener(|_, _: &RunParams, _, cx| cx.emit(ParamEvent::Run)))
            .on_action(cx.listener(|_, _: &CancelParams, _, cx| cx.emit(ParamEvent::Cancel)))
            .w(px(440.))
            .child(ui::modal_header(&t, None, "Parameters", ui::modal_close(&t, "pp-close").on_click(cx.listener(|_, _, _, cx| cx.emit(ParamEvent::Cancel)))))
            .child(div().px(px(20.)).py(px(16.)).flex().flex_col().gap(px(6.)).children(rows))
            .child(
                ui::modal_footer(&t)
                    .child(div().flex_1())
                    .child(
                        ui::button(&t, "pp-cancel", Btn::Secondary)
                            .h(px(28.))
                            .child("Cancel")
                            .child(ui::key_hint("esc"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ParamEvent::Cancel))),
                    )
                    .child(
                        ui::button(&t, "pp-run", Btn::Primary)
                            .h(px(28.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Run")
                            .child(ui::key_hint("⌘↩"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ParamEvent::Run))),
                    ),
            )
    }
}
