//! Protection › Sensitive columns › Edit: how one column is masked (Redacted, or Partial with the
//! characters kept at the start and at the end), with what an agent would get from a value read
//! from the column itself (a made-up one when none can be read). Nothing is saved before Save.

use dataman_core::privacy::{self, AgentView, Keep, Mask, Names};
use dataman_core::{ConnectionProfile, Engine};
use gpui::{AnyElement, ClickEvent, Context, FontWeight, SharedString, div, prelude::*};

use crate::agents::AgentsPage;
use crate::agents_rules::{choices, masked_example};
use crate::theme::{Theme, mono, px};
use crate::ui;

/// The sheet's column (as the rules name it) and the mask being picked.
pub(crate) struct SensEdit {
    pub(crate) col: String,
    pub(crate) how: Mask,
    /// A value of the column and the table it was read from, once read.
    pub(crate) sample: Option<(String, String)>,
}

/// Long values are cut: the preview shows how the mask looks, not the whole value.
const SAMPLE_MAX: usize = 80;

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
        if let Some(p) = self.selected_profile(cx) {
            self.read_sample(&p, &col, cx);
        }
        self.sens_edit = Some(SensEdit { col, how, sample: None });
        cx.notify();
    }

    /// The first value that isn't null of the first column the entry matches (a pattern too),
    /// read only while connected; MongoDB's fields by their path.
    fn read_sample(&mut self, p: &ConnectionProfile, col: &str, cx: &mut Context<Self>) {
        let Some(conn) = self.store.read(cx).connection(&p.id) else { return };
        let (col, engine) = (col.to_string(), p.engine);
        cx.spawn(async move |this, cx| {
            let db = conn.database().to_string();
            let found = if engine.is_sql() {
                let _ = conn.preload(&db).await;
                let snap = conn.schema.snapshot(&db);
                let at = snap.columns.iter().find_map(|(rel, cols)| {
                    let r = snap.relations.get(*rel)?;
                    let c = cols.iter().find(|c| privacy::sensitive_match(&col, Some(&r.schema), &r.name, &c.name, Names::of(engine)))?;
                    Some((r.schema.clone(), r.name.clone(), c.name.clone()))
                });
                match at {
                    Some((schema, table, c)) => conn.sample_column_value(&db, &schema, &table, &c).await.ok().flatten().map(|v| (v, table)),
                    None => None,
                }
            } else if engine == Engine::MongoDb && !col.contains('*') {
                match col.split_once('.') {
                    Some((coll, path)) => conn.sample_field_value(&db, coll, path).await.ok().flatten().map(|v| (v, coll.to_string())),
                    None => None,
                }
            } else {
                None
            };
            let Some((v, from)) = found.filter(|(v, _)| !v.trim().is_empty()) else { return };
            let v: String = v.chars().take(SAMPLE_MAX).collect();
            let _ = this.update(cx, |page, cx| {
                if let Some(e) = page.sens_edit.as_mut().filter(|e| e.col == col) {
                    e.sample = Some((v, from));
                    cx.notify();
                }
            });
        })
        .detach();
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
        let (got, from) = match (&e.sample, e.how) {
            (_, Mask::Hidden) => (privacy::HIDDEN.to_string(), None),
            (Some((v, table)), how) => (privacy::mask_value(how, v), Some(format!("a value of {table}"))),
            (None, how) => (masked_example(field, how), None),
        };
        let line = |label: &str, control: AnyElement| div().flex().flex_row().items_center().gap(px(16.)).child(div().w(px(84.)).flex_none().text_color(t.tx2).child(label.to_string())).child(control);
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
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(t.tx3).child("AGENTS GET"))
                            .children(from.map(|f| div().children(ui::here(&"agents-sens-edit-from".into())).id("agents-sens-edit-from").text_size(px(11.)).text_color(t.tx4).child(f))),
                    )
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
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(got),
                    ),
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
