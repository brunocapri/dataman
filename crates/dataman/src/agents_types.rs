//! Agents › Masks: the types recognized and masked inside any value an agent receives, one list
//! for every connection (settings' `agent_types`). A click on one, built-in or yours, opens it in
//! the editor on the right; + at the end of the list starts a new one.

use dataman_core::privacy::{self, CustomType, Mask, TypeRow, Validator};
use gpui::{AnyElement, App, ClickEvent, Context, Focusable, FontWeight, SharedString, StyledText, TextRun, Window, div, prelude::*};

use crate::agents::{Act, AgentsPage, Tip, dropdown, field, qmark, seg, switch};
use crate::assets::{Icon, icon};
use crate::theme::{Theme, px};
use crate::ui;

impl AgentsPage {
    pub(crate) fn type_rows(&self) -> Vec<TypeRow> {
        self.core.settings.get().agent_types.rows()
    }

    pub(crate) fn flip_type(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(on) = self.core.settings.get().agent_types.is_on(id) else { return };
        let id = id.to_string();
        self.edit_settings(|s| {
            if privacy::builtin(&id).is_some() {
                s.agent_types.set_builtin(&id, Some(!on), None);
            } else if let Some(c) = s.agent_types.custom.iter_mut().find(|c| c.id == id) {
                c.on = !c.on;
            }
        }, cx);
    }

    pub(crate) fn set_type_mask(&mut self, id: &str, m: Mask, cx: &mut Context<Self>) {
        let id = id.to_string();
        self.edit_settings(|s| {
            if privacy::builtin(&id).is_some() {
                s.agent_types.set_builtin(&id, None, Some(m));
            } else if let Some(c) = s.agent_types.custom.iter_mut().find(|c| c.id == id) {
                c.mask = m;
            }
        }, cx);
    }

    pub(crate) fn edit_type(&mut self, id: Option<&str>, cx: &mut Context<Self>) {
        let types = self.core.settings.get().agent_types;
        let c = id.and_then(|id| match privacy::builtin(id) {
            Some(b) => Some(types.builtin_def(b)),
            None => types.custom.into_iter().find(|c| c.id == id),
        });
        let e = &mut self.editor;
        e.confirm_delete = false;
        match c {
            Some(c) => {
                e.editing = Some(c.id.clone());
                e.mask = c.mask;
                e.validator = c.validator;
                e.ignore_case = c.ignore_case;
                e.keep = c.keep;
                e.more = c.validator != Validator::None || c.ignore_case;
                e.name.update(cx, |i, cx| i.set_text(c.name.clone(), cx));
                e.regex.update(cx, |i, cx| i.set_text(c.pattern.clone(), cx));
            }
            None => {
                e.editing = None;
                e.mask = Mask::Hidden;
                e.validator = Validator::None;
                e.ignore_case = false;
                e.keep = 4;
                e.more = false;
                e.name.update(cx, |i, cx| i.set_text("", cx));
                e.regex.update(cx, |i, cx| i.set_text("", cx));
            }
        }
        cx.notify();
    }

    /// The type as the editor holds it now.
    pub(crate) fn drafted(&self, cx: &App) -> CustomType {
        let e = &self.editor;
        let mut c = CustomType::new(e.name.read(cx).text(), e.regex.read(cx).text(), e.mask);
        if let Some(id) = &e.editing {
            c.id = id.clone();
        }
        c.validator = e.validator;
        c.ignore_case = e.ignore_case;
        c.keep = e.keep;
        c
    }

    /// The editor's mask saved, and kept open on it.
    pub(crate) fn save_type(&mut self, cx: &mut Context<Self>) {
        let c = self.drafted(cx);
        if c.name.is_empty() || privacy::check_pattern(&c.pattern).is_err() {
            return;
        }
        let id = c.id.clone();
        self.edit_settings(|s| {
            if privacy::builtin(&c.id).is_some() {
                s.agent_types.set_builtin_def(&c);
                return;
            }
            let n = s.agent_types.custom.len();
            match s.agent_types.custom.iter_mut().find(|x| x.id == c.id) {
                Some(x) => *x = CustomType { on: x.on, ..c },
                None if n < privacy::MAX_CUSTOM => s.agent_types.custom.push(c),
                None => {}
            }
        }, cx);
        self.select_type(&id, cx);
    }

    /// Selects a mask in the list and opens it in the editor.
    pub(crate) fn select_type(&mut self, id: &str, cx: &mut Context<Self>) {
        self.type_sel = self.type_rows().iter().position(|r| r.id == id);
        self.edit_type(self.type_sel.is_some().then_some(id), cx);
    }

    /// A mask of yours deleted; a built-in one back to how it ships.
    pub(crate) fn delete_type(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.editor.editing.clone() else { return };
        if privacy::builtin(&id).is_some() {
            self.edit_settings(|s| s.agent_types.reset_builtin(&id), cx);
            self.select_type(&id, cx);
            return;
        }
        self.edit_settings(|s| s.agent_types.custom.retain(|c| c.id != id), cx);
        self.type_sel = None;
        self.edit_type(None, cx);
    }

    pub(crate) fn render_types(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.type_rows();
        let profiles = self.profiles(cx);
        let any = profiles.iter().any(|p| p.mcp_rules.view == privacy::AgentView::Masked);
        let applies = if any { "Applied to every connection whose Privacy is Masked data" } else { "Applied to every connection whose Privacy is Masked data (none is yet)" };
        let head = self
            .header(t, "Masks")
            .child(qmark(t, "agents-types-tip", types_tip()))
            .child(div().flex_1().min_w(px(12.)))
            .child(div().children(ui::here(&"agents-types-applies".into())).min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.5)).text_color(t.tx3).child(applies));
        let broken: Vec<&TypeRow> = rows.iter().filter(|r| r.broken.is_some() && r.on).collect();
        let notice = broken.first().map(|r| div().pb(px(6.)).text_size(px(11.5)).text_color(t.err).child(format!("{} has an invalid pattern. Agent reads are refused while it is on.", r.name)));
        self.types_offset = notice.is_some() as usize;
        if self.type_sel.is_some_and(|k| k >= rows.len()) {
            self.type_sel = None;
        }
        let list: Vec<AnyElement> = rows.iter().map(|r| self.type_row(t, r, cx)).collect();
        let adding = self.editor.editing.is_none();
        let hov = t.hov;
        let add = div()
            .children(ui::here(&"agents-type-add".into()))
            .id("agents-type-add")
            .px(px(8.))
            .mx(px(-8.))
            .h(px(44.))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .text_color(t.tx2)
            .when(adding, |d| d.bg(t.prs).text_color(t.tx))
            .when(!adding, move |d| d.hover(move |s| s.bg(hov)))
            .child(div().w(px(28.)).flex().justify_center().child(icon(Icon::Plus).size(px(13.)).text_color(t.tx3)))
            .child("New mask")
            .on_click(cx.listener(|v, _, window, cx| {
                v.type_sel = None;
                v.edit_type(None, cx);
                let name = v.editor.name.clone();
                window.focus(&name.read(cx).focus_handle(cx), cx);
            }));
        // The list keeps room for a row's name; the editor gives way down to its narrowest.
        let left = div().id("agents-types-list").flex_1().min_w(px(380.)).overflow_y_scroll().track_scroll(&self.types_scroll).flex().flex_col().px(px(32.)).pt(px(8.)).pb(px(24.)).children(notice).children(list).child(add);
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(head)
            .child(div().flex_1().min_h_0().flex().flex_row().border_t_1().border_color(t.line).child(left).child(self.render_editor(t, window, cx)))
            .into_any_element()
    }

    fn type_row(&self, t: &Theme, r: &TypeRow, cx: &mut Context<Self>) -> AnyElement {
        let (flip_id, row_of, mask_id) = (r.id.clone(), r.id.clone(), SharedString::from(format!("agents-type-mask-{}", r.id)));
        let items: Vec<(SharedString, bool, Act)> = Mask::ALL.iter().map(|m| (m.label().into(), *m == r.mask, Act::TypeMask(r.id.clone(), *m))).collect();
        let open = mask_id.clone();
        let custom = r.custom;
        // A pattern of yours (a custom mask's, or a built-in one's you changed) shows as a pattern.
        let yours = r.rule.starts_with("Custom: ");
        let tip_rule = r.rule.trim_start_matches("Custom: ").to_string();
        let example = (!r.example.is_empty()).then_some(r.example.as_str());
        let mask = div()
            .id(SharedString::from(format!("agents-type-mask-btn-{}", r.id)))
            .child(dropdown(t, self, mask_id, r.mask.label(), r.on))
            .on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.open_menu(open.clone(), 120., items.clone(), cx)));
        let sw = SharedString::from(format!("agents-type-switch-{}", r.id));
        let row_id = SharedString::from(format!("agents-type-row-{}", r.id));
        let sel = self.editor.editing.as_deref() == Some(r.id.as_str());
        let hov = t.hov;
        div()
            .children(ui::here(&row_id.clone().into()))
            .id(row_id)
            .on_click(cx.listener(move |v, _, window, cx| {
                v.focus_list(window, cx);
                v.select_type(&row_of, cx);
            }))
            .when(sel, |d| d.bg(t.prs))
            .when(!sel, move |d| d.hover(move |s| s.bg(hov)))
            .px(px(8.))
            .mx(px(-8.))
            .h(px(44.))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .border_b_1()
            .border_color(t.line)
            .child(
                div()
                    .children(ui::here(&sw.clone().into()))
                    .id(sw.clone())
                    .when(!r.on, |d| d.opacity(0.55))
                    .child(switch(t, sw, r.on))
                    .on_click(cx.listener(move |v, _, _, cx| v.flip_type(&flip_id, cx))),
            )
            .child(
                div()
                    .id(SharedString::from(format!("agents-type-name-{}", r.id)))
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .when(!r.on, |d| d.opacity(0.55))
                    .text_color(if r.broken.is_some() { t.err } else { t.tx })
                    .child(r.name.clone()),
            )
            .child(div().flex_none().when(!r.on, |d| d.opacity(0.55)).child(qmark(t, &format!("agents-type-tip-{}", r.id), type_tip(&r.name, &tip_rule, example, yours))))
            .when(custom || r.edited, |d| d.child(div().flex_none().text_size(px(11.)).text_color(t.tx3).child(if custom { "custom" } else { "edited" })))
            .child(div().flex_1())
            .when(r.broken.is_some(), |d| {
                let id = r.id.clone();
                d.child(div().id(SharedString::from(format!("agents-fix-{}", r.id))).flex_none().text_size(px(11.5)).text_color(t.err).child("Fix").on_click(cx.listener(move |v, _, _, cx| v.select_type(&id, cx))))
            })
            .child(div().flex_none().when(!r.on, |d| d.opacity(0.55)).child(mask))
            .into_any_element()
    }

    fn render_editor(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let c = self.drafted(cx);
        let editing = self.editor.editing.is_some();
        let built_in = self.editor.editing.as_deref().is_some_and(|id| privacy::builtin(id).is_some());
        let edited = self.editor.editing.as_deref().is_some_and(|id| self.core.settings.get().agent_types.builtin_edited(id));
        let full = !editing && self.core.settings.get().agent_types.custom.len() >= privacy::MAX_CUSTOM;
        let problem = (!c.pattern.is_empty()).then(|| privacy::check_pattern(&c.pattern).err()).flatten();
        let samples: Vec<&str> = self.editor.samples.iter().map(String::as_str).collect();
        let tested = (problem.is_none() && !c.pattern.is_empty()).then(|| privacy::test_pattern(&c, &samples, &self.key).ok()).flatten();
        let ok = !c.name.is_empty() && !c.pattern.is_empty() && problem.is_none() && !full;
        let caps = |s: &str| ui::caps(t, s.to_string());
        let masks: Vec<(Mask, &str)> = Mask::ALL.iter().map(|m| (*m, m.label())).collect();
        let mut panel = div()
            .id("agents-editor")
            .w(px(400.))
            .min_w(px(300.))
            .flex_shrink(1.)
            .flex()
            .flex_col()
            .gap(px(16.))
            .px(px(24.))
            .py(px(20.))
            .border_l_1()
            .border_color(t.line)
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(caps(if built_in { "BUILT-IN MASK" } else if editing { "YOUR MASK" } else { "NEW MASK" }))
                    .child(
                        div()
                            .children(ui::here(&"agents-editor-title".into()))
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(13.))
                            .font_weight(FontWeight::BOLD)
                            .child(match &self.editor.editing {
                                Some(_) if !c.name.is_empty() => c.name.clone(),
                                Some(_) => "Unnamed".into(),
                                None => "New mask".into(),
                            }),
                    ),
            )
            .child(div().flex().flex_col().gap(px(6.)).child(caps("NAME")).child(field(t, "agents-field-name", &self.editor.name, window, cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(crate::agents::label_tip(
                        t,
                        "agents-regex-tip",
                        "REGEX",
                        Tip::new("Pattern")
                            .lead("Rust regex syntax, matched inside every value an agent receives. It runs in linear time, so no pattern can hang a reply.")
                            .item("Mask part of a match", "Name a group v and only it is masked:")
                            .code("password=(?P<v>\\S+)")
                            .item("Not supported", "Look-around (?=…) and back-references \\1. A pattern that matches empty text is refused.")
                            .note("Up to 500 characters. Test lines below show what an agent would get."),
                    ))
                    .child(field(t, "agents-field-regex", &self.editor.regex, window, cx))
                    .children(problem.clone().map(|p| div().text_size(px(11.)).text_color(t.err).child(p))),
            )
            .child(div().flex().flex_col().gap(px(8.)).child(crate::agents::label_tip(
                t,
                "agents-mask-tip",
                "MASK",
                Tip::new("Mask")
                    .item("Hidden", "The match becomes the type's name, like ‹customer code›.")
                    .item("Partial", "Keeps the last characters (4 by default; More options).")
                    .item("Hash", "A stable code like ‹h:K3P9QX2M7A›: the same value gives the same code everywhere, so agents can still match it."),
            )).child(seg(t, "agents-new-mask", &masks, self.editor.mask, false, cx, |v, m, _| v.editor.mask = m)));
        let more = self.editor.more;
        panel = panel.child(
            div()
                .children(ui::here(&"agents-more".into()))
                .id("agents-more")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .text_size(px(11.5))
                .text_color(t.tx2)
                .child("More options")
                .child(ui::disclosure("agents-more-arrow", more, 10., t.tx3))
                .on_click(cx.listener(|v, _, _, cx| {
                    v.editor.more = !v.editor.more;
                    cx.notify();
                })),
        );
        if more {
            let vals: Vec<(SharedString, bool, Act)> = Validator::ALL.iter().map(|x| (x.label().into(), *x == self.editor.validator, Act::Validator(*x))).collect();
            let keeps: Vec<(u8, String)> = (0..=6u8).map(|n| (n, n.to_string())).collect();
            let keeps: Vec<(u8, &str)> = keeps.iter().map(|(n, s)| (*n, s.as_str())).collect();
            panel = panel.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(12.))
                            .child(div().flex_1().text_color(t.tx2).child("Validator"))
                            .child(div().id("agents-validator-btn").child(dropdown(t, self, "agents-validator".into(), self.editor.validator.label(), true).w(px(130.))).on_click(cx.listener(move |v, _: &ClickEvent, _, cx| {
                                v.open_menu("agents-validator", 150., vals.clone(), cx)
                            }))),
                    )
                    .child(
                        div()
                            .id("agents-ignore-case")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.))
                            .child(crate::agents::checkbox(t, self.editor.ignore_case, false))
                            .child(div().text_color(t.tx2).child("Ignore case"))
                            .on_click(cx.listener(|v, _, _, cx| {
                                v.editor.ignore_case = !v.editor.ignore_case;
                                cx.notify();
                            })),
                    )
                    .when(self.editor.mask == Mask::Partial, |d| {
                        d.child(div().flex().flex_row().items_center().gap(px(12.)).child(div().flex_1().text_color(t.tx2).child("Keep last")).child(seg(t, "agents-keep", &keeps, self.editor.keep, true, cx, |v, n, _| v.editor.keep = n)))
                    }),
            );
        }
        let count = tested.as_ref().map(|r| r.matches).unwrap_or(0);
        let broad = tested.as_ref().is_some_and(|r| r.broad);
        let hit = t.prs;
        let lines = self.editor.samples.iter().enumerate().map(|(k, line)| {
            let found = tested.as_ref().and_then(|r| r.lines.iter().find(|(l, _, _)| l == line).cloned());
            let (spans, out) = found.map(|(_, s, o)| (s, o)).unwrap_or_default();
            let base = TextRun { len: 0, font: crate::editor::mono(), color: t.tx, background_color: None, underline: None, strikethrough: None };
            let mut runs = Vec::new();
            let mut at = 0;
            for r in &spans {
                if r.start > at {
                    runs.push(TextRun { len: r.start - at, ..base.clone() });
                }
                runs.push(TextRun { len: r.end - r.start, background_color: Some(hit), ..base.clone() });
                at = r.end;
            }
            if at < line.len() {
                runs.push(TextRun { len: line.len() - at, ..base.clone() });
            }
            let rid = SharedString::from(format!("agents-sample-{k}"));
            div()
                .id(rid.clone())
                .group(rid.clone())
                .relative()
                .flex()
                .flex_col()
                .gap(px(3.))
                .px(px(10.))
                .py(px(8.))
                .border_b_1()
                .border_color(t.line)
                .text_size(px(11.5))
                .child(div().pr(px(16.)).whitespace_nowrap().overflow_hidden().child(StyledText::new(line.clone()).with_runs(runs)))
                .child(div().whitespace_nowrap().overflow_hidden().text_ellipsis().text_color(t.tx3).child(out.unwrap_or_else(|| "no match".into())))
                .child(
                    div()
                        .id(SharedString::from(format!("agents-sample-rm-{k}")))
                        .absolute()
                        .top(px(6.))
                        .right(px(6.))
                        .invisible()
                        .group_hover(rid, |s| s.visible())
                        .text_color(t.tx3)
                        .child("×")
                        .on_click(cx.listener(move |v, _, _, cx| {
                            if k < v.editor.samples.len() {
                                v.editor.samples.remove(k);
                            }
                            cx.notify();
                        })),
                )
        });
        let sample_focused = self.editor.sample.read(cx).focus_handle(cx).is_focused(window);
        panel = panel.child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(div().flex_1().child(caps("TEST")))
                        .child(div().text_size(px(11.)).text_color(t.tx3).child(if count == 1 { "1 match".to_string() } else { format!("{count} matches") })),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .bg(t.field)
                        .border_1()
                        .border_color(if sample_focused { t.acc } else { t.field_b })
                        .children(lines)
                        .child(div().children(ui::here(&"agents-field-sample".into())).h(px(30.)).px(px(10.)).flex().items_center().child(self.editor.sample.clone())),
                )
                .when(broad, |d| d.child(div().text_size(px(11.)).text_color(t.warn).child("Matches most of these lines; it may be too broad."))),
        );
        if full {
            panel = panel.child(div().text_size(px(11.)).text_color(t.tx3).child(format!("{} masks of yours at most.", privacy::MAX_CUSTOM)));
        }
        let confirm = self.editor.confirm_delete;
        // Yours can be deleted; a built-in one goes back to its default once changed.
        let removable = editing && (!built_in || edited);
        let (verb, ask) = if built_in { ("Reset", format!("Reset {} to its default?", c.name)) } else { ("Delete", format!("Delete {}?", c.name)) };
        let buttons = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .when(removable && !confirm, |d| {
                d.child(
                    div()
                        .children(ui::here(&"agents-type-delete".into()))
                        .id("agents-type-delete")
                        .text_size(px(11.5))
                        .text_color(t.err)
                        .child(verb)
                        .on_click(cx.listener(|v, _, _, cx| {
                            v.editor.confirm_delete = true;
                            cx.notify();
                        })),
                )
            })
            .when(removable && confirm, |d| {
                d.child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.5)).text_color(t.err).child(ask))
                    .child(ui::button(t, "agents-type-delete-yes", ui::Btn::Danger).h(px(24.)).child(verb).on_click(cx.listener(|v, _, _, cx| v.delete_type(cx))))
                    .child(ui::button(t, "agents-type-delete-no", ui::Btn::Ghost).h(px(24.)).child("Keep").on_click(cx.listener(|v, _, _, cx| {
                        v.editor.confirm_delete = false;
                        cx.notify();
                    })))
            })
            .when(!confirm, |d| d.child(div().flex_1()))
            .child(ui::button(t, "agents-type-cancel", ui::Btn::Ghost).h(px(28.)).px(px(10.)).text_color(t.tx).child("Cancel").on_click(cx.listener(|v, _, _, cx| {
                v.type_sel = None;
                v.edit_type(None, cx);
            })))
            .child(
                ui::button(t, "agents-type-save", if ok { ui::Btn::Primary } else { ui::Btn::Secondary })
                    .h(px(28.))
                    .px(px(10.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .when(!ok, |d| d.opacity(0.5))
                    .child(if editing { "Save" } else { "Add" })
                    .on_click(cx.listener(|v, _, _, cx| v.save_type(cx))),
            );
        panel.child(div().flex_1()).child(buttons).into_any_element()
    }
}

/// How a type is found, as its `?` shows it.
fn type_tip(name: &str, rule: &str, example: Option<&str>, custom: bool) -> Tip {
    let tip = Tip::new(format!("How {name} is found"));
    let tip = if custom {
        let (pattern, validator) = rule.split_once(" · ").map_or((rule, None), |(p, v)| (p, Some(v)));
        let tip = tip.lead("Your pattern, matched inside every value an agent receives:").code(pattern.to_string());
        match validator {
            Some(v) => tip.item("Validator", format!("{v}: a match must also pass it.")),
            None => tip,
        }
    } else {
        tip.lead(rule.to_string())
    };
    let tip = match example {
        Some(e) => tip.example(e.to_string()),
        None => tip,
    };
    tip.note("On every connection whose agent sees Masked data: cells, documents, keys, printed lines and errors.")
}

pub(crate) fn types_tip() -> Tip {
    Tip::new("Masks")
        .lead("One list for every connection whose Privacy is Masked data: matching values are masked in everything sent to the agent, whether or not their column is a Sensitive column.")
        .item("Where", "Rows, documents, keys, printed lines, tool replies, schema resources and errors.")
        .item("Not on", "Connections on Data or Structure only: nothing is masked there.")
        .item("How", "Each one masks its matches as its menu says: Hidden, Partial or Hash.")
        .item("Edit", "Click one, built-in or yours, to change it on the right; + adds one.")
        .note("Replies about no connection in particular, such as the list of connections or of favorites, aren't masked.")
}
