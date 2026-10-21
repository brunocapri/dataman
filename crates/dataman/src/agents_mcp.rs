//! Agents › MCP: the server itself — whether agents may connect at all, and how to connect one.

use gpui::{AnyElement, Context, div, prelude::*};

use crate::agents::{AgentsPage, Tip, label_tip, seg};
use crate::theme::{Theme, px};
use crate::ui;

impl AgentsPage {
    pub(crate) fn render_mcp(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let on = self.core.settings.get().mcp_enabled;
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
                    .border_t_1()
                    .border_color(t.line)
                    .px(px(32.))
                    .py(px(24.))
                    .flex()
                    .flex_col()
                    .gap(px(28.))
                    .child(access)
                    .child(div().max_w(px(760.)).child(connect)),
            )
            .into_any_element()
    }
}
