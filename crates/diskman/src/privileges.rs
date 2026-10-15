//! The privileges panel (the explorer menu's "Privileges" on a table or schema): a compact
//! grant matrix, role × privilege, read with `aclexplode` (the owner first, PUBLIC last; `*`
//! marks WITH GRANT OPTION), in the side panel. GRANT… and REVOKE… open templates in a new tab
//! for the picked role; nothing here runs a statement.

use std::sync::Arc;

use gpui::{App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, FontWeight, ScrollHandle, SharedString, Window, div, prelude::*};
use crate::theme::px;
use diskman_core::Connection;
use diskman_core::schema::Privileges;
use diskman_core::schema::objects::Cell;
use diskman_core::sqlgen;

use crate::assets::{Icon, icon};
use crate::theme::ActiveTheme;
use crate::ui::{self, Btn};

pub enum PrivilegesEvent {
    /// A template for a new tab on the panel's connection and database.
    Template(String),
    Close,
}

enum State {
    Loading,
    Ready(Privileges),
    NotReported,
    Failed(String),
}

pub struct PrivilegesPanel {
    pub conn_id: String,
    pub db: String,
    schema: String,
    table: Option<String>,
    state: State,
    /// The role the templates name.
    picked: Option<String>,
    focus: FocusHandle,
    scroll: ScrollHandle,
}

impl EventEmitter<PrivilegesEvent> for PrivilegesPanel {}

impl Focusable for PrivilegesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The matrix's column heads: three letters for a table's privileges.
fn abbrev(p: &str) -> &str {
    match p {
        "SELECT" => "SEL",
        "INSERT" => "INS",
        "UPDATE" => "UPD",
        "DELETE" => "DEL",
        "TRUNCATE" => "TRU",
        "REFERENCES" => "REF",
        "TRIGGER" => "TRG",
        "MAINTAIN" => "MNT",
        other => other,
    }
}

impl PrivilegesPanel {
    /// Reads the grants of `schema.table` (or of `schema`) on `conn`, off the UI thread.
    pub fn new(conn: Arc<Connection>, db: String, schema: String, table: Option<String>, cx: &mut Context<Self>) -> Self {
        let task = conn.privileges(&db, &schema, table.as_deref());
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |p, cx| {
                p.state = match r {
                    Ok(Some(v)) => State::Ready(v),
                    Ok(None) => State::NotReported,
                    Err(e) => State::Failed(format!("{e:#}")),
                };
                cx.notify();
            });
        })
        .detach();
        PrivilegesPanel {
            conn_id: conn.profile.id.clone(),
            db,
            schema,
            table,
            state: State::Loading,
            picked: None,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
        }
    }

    /// What the panel is about: `public.orders`, or the schema.
    pub fn target(&self) -> String {
        match &self.table {
            Some(t) => sqlgen::qualified(Some(&self.schema), t),
            None => sqlgen::quote_ident(&self.schema),
        }
    }

    /// Demo screenshots: pick a role once the grants are in.
    pub fn pick(&mut self, role: &str, cx: &mut Context<Self>) {
        self.picked = Some(role.to_string());
        cx.notify();
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.state, State::Ready(_))
    }

    fn template(&self, grant: bool, cx: &mut Context<Self>) {
        let role = self.picked.as_deref();
        let sql = if grant {
            sqlgen::grant_template(&self.schema, self.table.as_deref(), role)
        } else {
            sqlgen::revoke_template(&self.schema, self.table.as_deref(), role)
        };
        cx.emit(PrivilegesEvent::Template(sql));
    }
}

impl Render for PrivilegesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let header = div()
            .h(px(40.))
            .flex_none()
            .pl(px(14.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .border_b_1()
            .border_color(t.line)
            .child(ui::caps(&t, "PRIVILEGES"))
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(self.target()))
            .child(
                ui::icon_button(&t, "priv-close", Icon::Close, 24., 13.)
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(PrivilegesEvent::Close))),
            );
        let body: gpui::AnyElement = match &self.state {
            State::Loading => div().px(px(14.)).py(px(10.)).italic().text_color(t.tx3).child("loading…").into_any_element(),
            State::NotReported => div().px(px(14.)).py(px(12.)).flex().child(ui::not_reported(&t)).into_any_element(),
            State::Failed(e) => div().px(px(14.)).py(px(10.)).text_color(t.err).child(e.lines().next().unwrap_or("").to_string()).into_any_element(),
            State::Ready(p) => {
                let cols = p.columns();
                let cw = if p.schema_level { 60. } else { 30. };
                let head = div()
                    .h(px(26.))
                    .px(px(14.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .text_size(px(10.5))
                    .text_color(t.tx3)
                    .child(div().flex_1().child(ui::caps(&t, "ROLE")))
                    .children(cols.iter().enumerate().map(|(j, c)| {
                        div()
                            .id(("priv-col", j))
                            .w(px(cw))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .child(SharedString::from(abbrev(c).to_string()))
                            .tooltip(ui::tooltip(c.clone(), None, None))
                    }));
                let owner = p.owner.clone();
                let rows = p.roles().into_iter().enumerate().map(|(i, role)| {
                    let picked = self.picked.as_deref() == Some(role.as_str());
                    let hov = t.hov;
                    let is_owner = owner.as_deref() == Some(role.as_str());
                    let r2 = role.clone();
                    div()
                        .id(("priv-row", i))
                        .h(px(26.))
                        .mx(px(6.))
                        .px(px(8.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .when(picked, |d| d.bg(t.acc_s))
                        .when(!picked, |d| d.hover(move |s| s.bg(hov)))
                        .on_click(cx.listener(move |p, _: &ClickEvent, _, cx| {
                            p.picked = if p.picked.as_deref() == Some(r2.as_str()) { None } else { Some(r2.clone()) };
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(6.))
                                .child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_color(if picked { t.tx } else { t.tx2 })
                                        .when(is_owner, |d| d.font_weight(FontWeight::SEMIBOLD))
                                        .child(role.clone()),
                                )
                                .when(is_owner, |d| d.child(div().flex_none().text_size(px(10.5)).text_color(t.tx3).child("owner"))),
                        )
                        .children(cols.iter().map(|c| {
                            let cell: gpui::AnyElement = match p.cell(&role, c) {
                                Cell::No => div().text_color(t.tx4).child("·").into_any_element(),
                                Cell::Yes => icon(Icon::Check).size(px(12.)).text_color(t.ok).into_any_element(),
                                Cell::Grantable => div()
                                    .flex()
                                    .flex_row()
                                    .items_start()
                                    .child(icon(Icon::Check).size(px(12.)).text_color(t.ok))
                                    .child(div().text_size(px(10.)).line_height(px(10.)).text_color(t.tx3).child("*"))
                                    .into_any_element(),
                            };
                            div().w(px(cw)).flex_none().flex().justify_center().child(cell)
                        }))
                });
                div()
                    .id("priv-matrix")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .pt(px(4.))
                    .child(head)
                    .children(rows)
                    .into_any_element()
            }
        };
        let ready = self.is_ready();
        let footer = div()
            .flex_none()
            .px(px(14.))
            .py(px(10.))
            .flex()
            .flex_row()
            .gap(px(8.))
            .border_t_1()
            .border_color(t.line)
            .child(
                ui::button(&t, "priv-grant", Btn::Secondary)
                    .child("GRANT…")
                    .on_click(cx.listener(|p, _: &ClickEvent, _, cx| p.template(true, cx))),
            )
            .child(
                ui::button(&t, "priv-revoke", Btn::Secondary)
                    .child("REVOKE…")
                    .on_click(cx.listener(|p, _: &ClickEvent, _, cx| p.template(false, cx))),
            )
            .child(div().flex_1())
            .children(self.picked.clone().filter(|_| ready).map(|r| {
                div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.)).text_color(t.tx3).child(r)
            }));
        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .text_size(px(12.))
            .font_family(crate::theme::mono())
            .child(header)
            .child(body)
            .child(footer)
    }
}
