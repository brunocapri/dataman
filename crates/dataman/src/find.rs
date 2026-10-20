// The owner renders the bar beside its own key context, not inside it, so the editor's and
// the grid's keys (arrows, ↩, ⌘Z…) never act while the field has the focus.

use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, Focusable, KeyBinding, SharedString, Subscription, Window, actions,
    div, prelude::*,
};
use crate::theme::px;
use dataman_core::find::FindOptions;

use crate::assets::Icon;
use crate::input::{InputEvent, TextInput};
use crate::theme::{ActiveTheme, mono};
use crate::ui;

actions!(
    find,
    [
        Find,
        FindReplace,
        FindNext,
        FindPrevious,
        CloseFind,
        NextField,
    ]
);

pub const CONTEXT: &str = "FindBar";

const FIELD_W: f32 = 196.;

pub fn bind_keys(cx: &mut App) {
    let b = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("cmd-f", Find, b),
        KeyBinding::new("cmd-alt-f", FindReplace, b),
        KeyBinding::new("cmd-g", FindNext, b),
        KeyBinding::new("cmd-shift-g", FindPrevious, b),
        KeyBinding::new("shift-enter", FindPrevious, b),
        KeyBinding::new("escape", CloseFind, b),
        KeyBinding::new("tab", NextField, b),
        KeyBinding::new("shift-tab", NextField, b),
    ]);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Count {
    pub cur: Option<usize>,
    pub total: usize,
    pub capped: bool,
    pub busy: bool,
    pub invalid: bool,
    pub query: bool,
}

impl Count {
    pub fn label(&self) -> String {
        if !self.query || self.invalid {
            return if self.busy { "…".into() } else { String::new() };
        }
        if self.busy && self.total == 0 {
            return "…".into();
        }
        let n = |v: usize| crate::grid::ui_count(v);
        let cur = self.cur.map_or(0, |c| c + 1);
        format!("{}/{}{}{}", n(cur), n(self.total), if self.capped { "+" } else { "" }, if self.busy { "…" } else { "" })
    }
}

pub enum FindBarEvent {
    Changed,
    Next,
    Prev,
    Close,
    Replace,
    ReplaceAll,
}

pub struct FindBar {
    query: Entity<TextInput>,
    replace: Entity<TextInput>,
    opts: FindOptions,
    replacing: bool,
    can_replace: bool,
    count: Count,
    _subs: [Subscription; 2],
}

impl EventEmitter<FindBarEvent> for FindBar {}

impl FindBar {
    pub fn new(can_replace: bool, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| TextInput::new(cx, "Find").bare());
        let replace = cx.new(|cx| TextInput::new(cx, "Replace").bare());
        let subs = [
            cx.subscribe(&query, |_, _, ev: &InputEvent, cx| {
                cx.emit(match ev {
                    InputEvent::Changed => FindBarEvent::Changed,
                    InputEvent::Submit => FindBarEvent::Next,
                })
            }),
            cx.subscribe(&replace, |_, _, ev: &InputEvent, cx| {
                if let InputEvent::Submit = ev {
                    cx.emit(FindBarEvent::Replace)
                }
            }),
        ];
        FindBar { query, replace, opts: FindOptions::default(), replacing: false, can_replace, count: Count::default(), _subs: subs }
    }

    pub fn query(&self, cx: &App) -> String {
        self.query.read(cx).text().to_string()
    }

    pub fn replacement(&self, cx: &App) -> String {
        self.replace.read(cx).text().to_string()
    }

    pub fn options(&self) -> FindOptions {
        self.opts
    }

    pub fn set_query(&mut self, q: &str, cx: &mut Context<Self>) {
        self.query.update(cx, |i, cx| i.set_text(q, cx));
    }

    #[cfg(feature = "checks")]
    pub fn set_replacement(&mut self, r: &str, cx: &mut Context<Self>) {
        self.replace.update(cx, |i, cx| i.set_text(r, cx));
    }

    #[cfg(feature = "checks")]
    pub fn set_options(&mut self, opts: FindOptions, cx: &mut Context<Self>) {
        self.opts = opts;
        cx.notify();
    }

    pub fn set_replacing(&mut self, on: bool, cx: &mut Context<Self>) {
        self.replacing = on && self.can_replace;
        cx.notify();
    }

    pub fn set_count(&mut self, c: Count, cx: &mut Context<Self>) {
        if self.count != c {
            self.count = c;
            cx.notify();
        }
    }

    #[cfg(feature = "checks")]
    pub fn count(&self) -> Count {
        self.count
    }

    pub fn focus_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let h = self.query.focus_handle(cx);
        window.focus(&h, cx);
        self.query.update(cx, |i, cx| i.select_all(cx));
    }

    pub fn has_focus(&self, window: &Window, cx: &App) -> bool {
        self.query.focus_handle(cx).is_focused(window) || self.replace.focus_handle(cx).is_focused(window)
    }

    fn toggle(&mut self, f: fn(&mut FindOptions), window: &mut Window, cx: &mut Context<Self>) {
        f(&mut self.opts);
        if !self.has_focus(window, cx) {
            let h = self.query.focus_handle(cx);
            window.focus(&h, cx);
        }
        cx.emit(FindBarEvent::Changed);
        cx.notify();
    }
}

impl Render for FindBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let q_on = self.query.focus_handle(cx).is_focused(window);
        let r_on = self.replace.focus_handle(cx).is_focused(window);
        let invalid = self.count.invalid;
        let field = |input: &Entity<TextInput>, focused: bool, err: bool| {
            div()
                .flex_none()
                .w(px(FIELD_W))
                .h(px(24.))
                .px(px(7.))
                .flex()
                .items_center()
                .bg(t.field)
                .border_1()
                .border_color(if err {
                    t.err
                } else if focused {
                    t.acc
                } else {
                    t.field_b
                })
                .text_size(px(12.))
                .text_color(t.tx)
                .child(input.clone())
        };
        let toggle = |id: &'static str, label: &'static str, on: bool| {
            let (hov, prs) = (t.hov, t.prs);
            div()
                .id(id)
                .flex_none()
                .h(px(22.))
                .min_w(px(24.))
                .px(px(4.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.))
                .when(on, |d| d.bg(t.acc_s).text_color(t.acc_t))
                .when(!on, |d| d.text_color(t.tx3).hover(move |s| s.bg(hov)).active(move |s| s.bg(prs)))
                .child(label)
        };
        let count: SharedString = self.count.label().into();
        let o = self.opts;
        let top = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.))
            .child(field(&self.query, q_on, invalid))
            .child(
                div()
                    .flex_none()
                    .min_w(px(54.))
                    .px(px(6.))
                    .text_size(px(11.))
                    .text_color(t.tx3)
                    .whitespace_nowrap()
                    .child(count),
            )
            .child(
                ui::icon_button(&t, "find-prev", Icon::ChevronUp, 24., 12.)
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(FindBarEvent::Prev))),
            )
            .child(
                ui::icon_button(&t, "find-next", Icon::ChevronDown, 24., 12.)
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(FindBarEvent::Next))),
            )
            .child(div().mx(px(3.)).child(ui::vsep(&t, 14.)))
            .child(toggle("find-case", "Aa", o.case).on_click(cx.listener(|b, _: &ClickEvent, w, cx| b.toggle(|o| o.case = !o.case, w, cx))))
            .child(toggle("find-word", "ab", o.word).on_click(cx.listener(|b, _: &ClickEvent, w, cx| b.toggle(|o| o.word = !o.word, w, cx))))
            .child(toggle("find-regex", ".*", o.regex).on_click(cx.listener(|b, _: &ClickEvent, w, cx| b.toggle(|o| o.regex = !o.regex, w, cx))))
            .child(div().mx(px(3.)).child(ui::vsep(&t, 14.)))
            .child(
                ui::icon_button(&t, "find-close", Icon::Close, 24., 12.)
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(FindBarEvent::Close))),
            );
        let bottom = self.replacing.then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .child(field(&self.replace, r_on, false))
                .child(
                    ui::button(&t, "find-replace", ui::Btn::Ghost)
                        .h(px(24.))
                        .px(px(8.))
                        .text_size(px(11.5))
                        .child("Replace")
                        .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(FindBarEvent::Replace))),
                )
                .child(
                    ui::button(&t, "find-replace-all", ui::Btn::Ghost)
                        .h(px(24.))
                        .px(px(8.))
                        .text_size(px(11.5))
                        .child("Replace All")
                        .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(FindBarEvent::ReplaceAll))),
                )
        });
        div()
            .id("find-bar")
            .key_context(CONTEXT)
            .occlude()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(4.))
            .bg(t.pop)
            .shadow(t.ring())
            .font_family(mono())
            .text_color(t.tx)
            .on_action(cx.listener(|b, _: &Find, w, cx| b.focus_query(w, cx)))
            .on_action(cx.listener(|b, _: &FindReplace, w, cx| {
                b.set_replacing(true, cx);
                b.focus_query(w, cx);
            }))
            .on_action(cx.listener(|_, _: &FindNext, _, cx| cx.emit(FindBarEvent::Next)))
            .on_action(cx.listener(|_, _: &FindPrevious, _, cx| cx.emit(FindBarEvent::Prev)))
            .on_action(cx.listener(|_, _: &CloseFind, _, cx| cx.emit(FindBarEvent::Close)))
            .on_action(cx.listener(|b, _: &NextField, w, cx| {
                if !b.replacing {
                    return;
                }
                let to = if b.query.focus_handle(cx).is_focused(w) { b.replace.clone() } else { b.query.clone() };
                let h = to.focus_handle(cx);
                w.focus(&h, cx);
                to.update(cx, |i, cx| i.select_all(cx));
            }))
            .child(top)
            .children(bottom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_labels() {
        let c = |cur, total, capped, busy| Count { cur, total, capped, busy, invalid: false, query: true };
        assert_eq!(c(Some(2), 17, false, false).label(), "3/17");
        assert_eq!(c(None, 0, false, false).label(), "0/0");
        assert_eq!(c(Some(0), 10_000, true, false).label(), "1/10,000+");
        assert_eq!(c(None, 0, false, true).label(), "…");
        assert_eq!(c(Some(0), 4, false, true).label(), "1/4…");
        assert_eq!(Count::default().label(), "");
        assert_eq!(Count { invalid: true, query: true, ..Default::default() }.label(), "");
    }
}
