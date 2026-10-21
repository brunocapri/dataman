//! Agents › MCP: the server itself — whether agents may connect at all, how to connect one, and
//! the tools they can call (the built-in ones and those in mcp-tools.toml).

use gpui::{AnyElement, Context, SharedString, Window, div, prelude::*};

use crate::agents::{AgentsPage, Tip, label_tip, seg};
use crate::theme::{Theme, px};
use crate::ui;

/// One tool as the page lists it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ToolRow {
    pub name: String,
    pub what: String,
    pub writes: bool,
    pub custom: bool,
}

fn policy_label(writes: bool) -> &'static str {
    if writes { "writes" } else { "reads" }
}

/// The body's children before the first tool row (for scrolling a row into view).
pub(crate) const TOOLS_OFFSET: usize = 2;

impl AgentsPage {
    pub(crate) fn tool_rows(&self, cx: &mut gpui::App) -> (Vec<ToolRow>, Vec<String>) {
        let Ok((_, reg)) = crate::mcp::shared(&self.core, cx).hub() else { return (Vec::new(), Vec::new()) };
        let builtin = reg.builtin_names();
        let rows = reg
            .all()
            .iter()
            .map(|t| ToolRow {
                name: t.name().to_string(),
                what: t.description().split(". ").next().unwrap_or_default().trim_end_matches('.').to_string(),
                writes: t.need() == Some(dataman_mcp::policy::Need::Write),
                custom: !builtin.iter().any(|b| b == t.name()),
            })
            .collect();
        (rows, reg.user_errors())
    }

    pub(crate) fn render_mcp(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let _ = window;
        let on = self.core.settings.get().mcp_enabled;
        let (tools, errors) = self.tool_rows(cx);
        if self.tool_sel.is_some_and(|k| k >= tools.len()) {
            self.tool_sel = None;
        }
        let access = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(label_tip(
                t,
                "agents-mcp-access-tip",
                "AGENT ACCESS",
                Tip::new("Agent access")
                    .lead("Whether AI agents (Claude Code, Claude Desktop, any MCP client) may use Dataman at all.")
                    .item("On", "Each connection then follows its own MCP tier, privacy and limits (Permissions, Privacy).")
                    .item("Off", "Every agent call is refused. The same switch as Settings › MCP."),
            ))
            .child(seg(t, "agents-mcp-access", &[(false, "Off"), (true, "On")], on, false, cx, |v, on, cx| v.edit_settings(|s| s.mcp_enabled = on, cx)));
        let cmd = crate::mcp::stdio_command();
        let copy = cmd.clone();
        let connect = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(label_tip(
                t,
                "agents-mcp-connect-tip",
                "CONNECT AN AGENT",
                Tip::new("Connect an agent").lead("Run this once in a terminal to add Dataman to Claude Code. Other MCP clients start the same command over stdio."),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().min_w_0().px(px(10.)).py(px(6.)).bg(t.field).border_1().border_color(t.line).text_size(px(11.5)).text_color(t.tx).overflow_hidden().whitespace_nowrap().text_ellipsis().child(cmd))
                    .child(ui::button(t, "agents-mcp-copy", ui::Btn::Secondary).h(px(28.)).child("Copy").on_click(cx.listener(move |_, _, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()))))),
            );
        let rows = tools.iter().enumerate().map(|(k, r)| {
            let sel = self.tool_sel == Some(k);
            let hov = t.hov;
            let id = SharedString::from(format!("agents-tool-{}", r.name));
            div()
                .children(ui::here(&id.clone().into()))
                .id(id)
                .h(px(44.))
                .flex_none()
                .px(px(8.))
                .mx(px(-8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(14.))
                .border_b_1()
                .border_color(t.line)
                .when(sel, |d| d.bg(t.prs))
                .when(!sel, move |d| d.hover(move |s| s.bg(hov)))
                .child(div().w(px(170.)).flex_none().text_color(if on { t.tx } else { t.tx3 }).child(r.name.clone()))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.5)).text_color(t.tx3).child(r.what.clone()))
                .when(r.custom, |d| d.child(div().flex_none().text_size(px(11.)).text_color(t.tx3).child("custom")))
                .child(div().w(px(48.)).flex_none().text_size(px(11.)).text_color(t.tx3).child(policy_label(r.writes)))
                .on_click(cx.listener(move |v, _, window, cx| {
                    v.focus_list(window, cx);
                    v.tool_sel = Some(k);
                    cx.notify();
                }))
        });
        let head = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .pb(px(4.))
            .child(label_tip(
                t,
                "agents-mcp-tools-tip",
                "TOOLS",
                Tip::new("Tools")
                    .lead("What agents can call. Reads run as the connection's tier and privacy allow; writes also wait for your Allow where the tier says so.")
                    .item("custom", "Your own tools, from mcp-tools.toml next to settings.toml. They follow the same rules."),
            ))
            .children(errors.into_iter().map(|e| div().text_size(px(11.5)).text_color(t.err).child(format!("mcp-tools.toml: {e}"))));
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.header(t, "MCP"))
            .child(
                div()
                    .id("agents-mcp-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.tools_scroll)
                    .border_t_1()
                    .border_color(t.line)
                    .px(px(32.))
                    .py(px(24.))
                    .flex()
                    .flex_col()
                    .child(div().pb(px(28.)).flex().flex_row().gap(px(40.)).child(div().w(px(260.)).flex_none().child(access)).child(div().flex_1().min_w_0().child(connect)))
                    .child(head)
                    .children(rows),
            )
            .into_any_element()
    }
}
