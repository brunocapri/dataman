//! Protection › Sensitive columns › Edit: how one column is masked (Redacted, or Partial with the
//! characters kept at the start and at the end), with what an agent would get. Nothing is saved
//! before Save.

use dataman_core::privacy::{AgentView, Keep, Mask};
use gpui::{AnyElement, ClickEvent, Context, FontWeight, SharedString, div, prelude::*};

use crate::agents::AgentsPage;
use crate::agents_rules::{choices, masked_example};
use crate::theme::{Theme, mono, px};
use crate::ui;

/// The sheet's column (as the rules name it) and the mask being picked.
pub(crate) struct SensEdit {
    pub(crate) col: String,
    pub(crate) how: Mask,
}

/// The row's words for a mask: `Partial · first 2, last 3`.
pub(crate) fn summary(how: Mask) -> String {
    match how {
        Mask::Hidden => "Redacted".into(),
        Mask::Partial(Keep { first: 0, last }) => format!("Partial · last {last}"),
        Mask::Partial(Keep { first, last: 0 }) => format!("Partial · first {first}"),
        Mask::Partial(Keep { first, last }) => format!("Partial · first {first}, last {last}"),
    }
}

impl AgentsPage {
    pub(crate) fn open_sens_edit(&mut self, col: String, how: Mask, cx: &mut Context<Self>) {
        self.sens_edit = Some(SensEdit { col, how });
        cx.notify();
    }

    pub(crate) fn close_sens_edit(&mut self, cx: &mut Context<Self>) {
        self.sens_edit = None;
        cx.notify();
    }

    /// Sensitive columns are inert outside Masked data: Save there changes nothing.
    pub(crate) fn save_sens_edit(&mut self, cx: &mut Context<Self>) {
        let Some(e) = self.sens_edit.take() else { return };
        if let Some(id) = self.conn.clone().filter(|id| self.core.config.get(id).is_some_and(|p| p.mcp_rules.view == AgentView::Masked)) {
            self.edit_conn(&id, |p| {
                if let Some(s) = p.mcp_rules.sensitive.iter_mut().find(|s| s.col == e.col) {
                    (s.how, s.on) = (e.how, true);
                }
            }, cx);
        }
        cx.notify();
    }

    pub(crate) fn sens_sheet(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let e = self.sens_edit.as_ref()?;
        let field = e.col.rsplit_once('.').map_or(e.col.as_str(), |(_, f)| f);
        let masks: Vec<(Mask, &str)> = Mask::ALL.iter().map(|m| (*m, m.label())).collect();
        let counts: Vec<(u8, String)> = (0..=Keep::MAX).map(|n| (n, n.to_string())).collect();
        let counts: Vec<(u8, &str)> = counts.iter().map(|(n, s)| (*n, s.as_str())).collect();
        let keep = e.how.kept();
        let line = |label: &str, control: AnyElement| div().flex().flex_row().items_center().gap(px(16.)).child(div().w(px(84.)).flex_none().text_color(t.tx2).child(label.to_string())).child(control);
        let as_comes = match e.how {
            Mask::Hidden => "Nothing of the value is kept.",
            Mask::Partial(k) if k == Keep::DEFAULT => "As it comes: e-mails keep their first letter and their domain.",
            Mask::Partial(_) => "Letters and digits between them become •; separators stay. A value too short for this is masked whole.",
        };
        let body = div()
            .p(px(20.))
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(line(
                "Mask",
                choices(t, "agents-sens-edit-mask", &masks, Mask::ALL.into_iter().find(|m| m.is_partial() == e.how.is_partial()).unwrap_or(Mask::Hidden), cx, |v, m, _| {
                    if let Some(e) = v.sens_edit.as_mut() {
                        // Back to Partial keeps the characters picked before.
                        e.how = m.keeping(e.how.kept());
                    }
                })
                .into_any_element(),
            ))
            .when(e.how.is_partial(), |d| {
                d.child(line("Keep first", choices(t, "agents-sens-edit-first", &counts, keep.first, cx, |v, n, _| set_keep(v, |k| k.first = n)).into_any_element()))
                    .child(line("Keep last", choices(t, "agents-sens-edit-last", &counts, keep.last, cx, |v, n, _| set_keep(v, |k| k.last = n)).into_any_element()))
            })
            .child(
                div()
                    .mt(px(4.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(t.tx3).child("AGENTS GET"))
                    .child(
                        div()
                            .id("agents-sens-edit-example")
                            .h(px(34.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .bg(t.field)
                            .border_1()
                            .border_color(t.line2)
                            .font_family(mono())
                            .text_size(px(12.5))
                            .child(masked_example(field, e.how)),
                    )
                    .child(div().text_size(px(11.)).text_color(t.tx3).child(as_comes)),
            );
        let footer = ui::modal_footer(t)
            .child(div().flex_1())
            .child(ui::button(t, "agents-sens-edit-cancel", ui::Btn::Secondary).h(px(28.)).child("Cancel").child(ui::key_hint("esc")).on_click(cx.listener(|v, _: &ClickEvent, _, cx| v.close_sens_edit(cx))))
            .child(ui::button(t, "agents-sens-edit-save", ui::Btn::Primary).h(px(28.)).font_weight(FontWeight::SEMIBOLD).child("Save").on_click(cx.listener(|v, _, _, cx| v.save_sens_edit(cx))));
        Some(
            div()
                .id("agents-sens-edit-sheet")
                .absolute()
                .inset_0()
                .bg(t.scrim)
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .child(ui::esc_closes(None, cx.listener(|v, _: &(), _, cx| v.close_sens_edit(cx))))
                .child(
                    ui::modal(t, "agents-sens-edit-card")
                        .w(px(500.))
                        .child(ui::modal_header(t, None, SharedString::from(e.col.clone()), ui::modal_close(t, "agents-sens-edit-close").cursor_pointer().on_click(cx.listener(|v, _: &ClickEvent, _, cx| v.close_sens_edit(cx)))))
                        .child(body)
                        .child(footer),
                )
                .into_any_element(),
        )
    }
}

fn set_keep(v: &mut AgentsPage, f: impl FnOnce(&mut Keep)) {
    if let Some(e) = v.sens_edit.as_mut() {
        let mut k = e.how.kept();
        f(&mut k);
        e.how = e.how.keeping(k);
    }
}
