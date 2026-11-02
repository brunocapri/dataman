//! Protection › Sensitive columns › Edit: how one column is masked (Redacted, or Partial with the
//! characters kept at the start and at the end), with the column as an agent would get it: a few
//! of its own values in a results grid (a made-up one when none can be read). Nothing is saved
//! before Save.

use dataman_core::privacy::{self, AgentView, Keep, Mask, Names};
use dataman_core::{ConnectionProfile, Engine};
use gpui::{AnyElement, ClickEvent, Context, FontWeight, SharedString, div, prelude::*};

use crate::agents::AgentsPage;
use crate::agents_rules::{choices, masked_example};
use crate::theme::{Theme, px};
use crate::ui;

/// The sheet's column (as the rules name it) and the mask being picked.
pub(crate) struct SensEdit {
    pub(crate) col: String,
    pub(crate) how: Mask,
    /// Values of the column and the table they were read from, once read.
    pub(crate) sample: Option<(Vec<String>, String)>,
}

/// Long values are cut: the preview shows how the mask looks, not the whole value.
const SAMPLE_MAX: usize = 80;
/// The rows the preview reads and shows.
const SAMPLE_ROWS: u32 = 6;
const GRID_ROW_H: f32 = 22.;
const GRID_HEAD_H: f32 = 28.;
/// The grid keeps its horizontal scrollbar's lane under the rows.
const GRID_BAR: f32 = 10.;

/// A value as the preview shows it: Redacted by its name, like the row's example.
fn shown(how: Mask, v: &str) -> String {
    match how {
        Mask::Hidden => crate::agents_rules::REDACTED.into(),
        how => privacy::mask_value(how, v),
    }
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
        if let Some(p) = self.selected_profile(cx) {
            self.read_sample(&p, &col, cx);
        }
        self.sens_edit = Some(SensEdit { col, how, sample: None });
        self.sync_sens_grid(cx);
        cx.notify();
    }

    /// The preview grid: the column under its own name, masked as picked.
    pub(crate) fn sync_sens_grid(&mut self, cx: &mut Context<Self>) {
        let Some(e) = self.sens_edit.as_ref() else { return };
        let field = e.col.rsplit_once('.').map_or(e.col.as_str(), |(_, f)| f).to_string();
        let rows: Vec<serde_json::Value> = match &e.sample {
            Some((vals, _)) => vals.iter().map(|v| serde_json::json!([shown(e.how, v)])).collect(),
            None => vec![serde_json::json!([masked_example(&field, e.how)])],
        };
        let reply = serde_json::json!({ "columns": [[field, ""]], "rows": rows }).to_string();
        let (table, rt) = (crate::agents::reply_table(&reply).map(|(rs, _)| rs), self.core.handle());
        self.sens_grid.update(cx, |g, cx| g.set_result(table, rt, cx));
    }

    /// Values that aren't null of the first column the entry matches (a pattern too), read only
    /// while connected; MongoDB's fields by their path.
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
                    Some((schema, table, c)) => conn.sample_column_values(&db, &schema, &table, &c, SAMPLE_ROWS).await.ok().map(|v| (v, table)),
                    None => None,
                }
            } else if engine == Engine::MongoDb && !col.contains('*') {
                match col.split_once('.') {
                    Some((coll, path)) => conn.sample_field_values(&db, coll, path, SAMPLE_ROWS as i64).await.ok().map(|v| (v, coll.to_string())),
                    None => None,
                }
            } else {
                None
            };
            let Some((vals, from)) = found else { return };
            let vals: Vec<String> = vals.into_iter().filter(|v| !v.trim().is_empty()).map(|v| v.chars().take(SAMPLE_MAX).collect()).collect();
            if vals.is_empty() {
                return;
            }
            let _ = this.update(cx, |page, cx| {
                if let Some(e) = page.sens_edit.as_mut().filter(|e| e.col == col) {
                    e.sample = Some((vals, from));
                    page.sync_sens_grid(cx);
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
        let masks: Vec<(Mask, &str)> = Mask::ALL.iter().map(|m| (*m, m.label())).collect();
        let counts: Vec<(u8, String)> = (0..=Keep::MAX).map(|n| (n, n.to_string())).collect();
        let counts: Vec<(u8, &str)> = counts.iter().map(|(n, s)| (*n, s.as_str())).collect();
        let keep = e.how.kept();
        let from = e.sample.as_ref().map(|(_, table)| format!("from {table}"));
        let shown_rows = e.sample.as_ref().map_or(1, |(v, _)| v.len()) as f32;
        let line = |label: &str, control: AnyElement| div().flex().flex_row().items_center().gap(px(16.)).child(div().w(px(84.)).flex_none().text_color(t.tx2).child(label.to_string())).child(control);
        let body = div()
            .p(px(20.))
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(line(
                "Mask",
                choices(t, "agents-sens-edit-mask", &masks, Mask::ALL.into_iter().find(|m| m.is_partial() == e.how.is_partial()).unwrap_or(Mask::Hidden), cx, |v, m, cx| {
                    if let Some(e) = v.sens_edit.as_mut() {
                        // Back to Partial keeps the characters picked before.
                        e.how = m.keeping(e.how.kept());
                    }
                    v.sync_sens_grid(cx);
                })
                .into_any_element(),
            ))
            .when(e.how.is_partial(), |d| {
                d.child(line("Keep first", choices(t, "agents-sens-edit-first", &counts, keep.first, cx, |v, n, cx| set_keep(v, |k| k.first = n, cx)).into_any_element()))
                    .child(line("Keep last", choices(t, "agents-sens-edit-last", &counts, keep.last, cx, |v, n, cx| set_keep(v, |k| k.last = n, cx)).into_any_element()))
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
                    .child(div().children(ui::here(&"agents-sens-edit-example".into())).h(px(GRID_HEAD_H + GRID_ROW_H * shown_rows + GRID_BAR + 2.)).border_1().border_color(t.line).child(self.sens_grid.clone())),
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

fn set_keep(v: &mut AgentsPage, f: impl FnOnce(&mut Keep), cx: &mut Context<AgentsPage>) {
    if let Some(e) = v.sens_edit.as_mut() {
        let mut k = e.how.kept();
        f(&mut k);
        e.how = e.how.keeping(k);
    }
    v.sync_sens_grid(cx);
}
