//! Protection › Copy from…: a connection's rules for agents as another connection of its engine
//! has them (`agents_config::copied` says what carries over). A copy replaces whole lists that no
//! undo brings back, so a sheet shows what changes first, what loosens flagged. It writes as any
//! edit of the page does; agents have no way to do this.

use dataman_core::agents_config::{self, Change, ConnRules};
use dataman_core::ConnectionProfile;
use gpui::{AnyElement, Context, FontWeight, KeyDownEvent, SharedString, Window, div, prelude::*};

use crate::agents::{Act, AgentsPage};
use crate::theme::{Theme, px};
use crate::ui;

const BUTTON: &str = "agents-copy-from";

pub(crate) struct CopySheet {
    to: String,
    from: String,
    from_name: String,
    changes: Vec<Change>,
}

impl AgentsPage {
    pub(crate) fn copy_button(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> Option<AnyElement> {
        let items: Vec<(SharedString, bool, Act)> =
            self.profiles(cx).into_iter().filter(|o| o.engine == p.engine && o.id != p.id).map(|o| (o.name.into(), false, Act::CopyFrom(o.id))).collect();
        if items.is_empty() {
            return None;
        }
        Some(
            div()
                .relative()
                .flex_none()
                .child(self.anchor(BUTTON))
                .child(ui::button(t, BUTTON, ui::Btn::Secondary).h(px(28.)).child("Copy from…").on_click(cx.listener(move |v, _, _, cx| v.open_menu(BUTTON, 240., items.clone(), cx))))
                .into_any_element(),
        )
    }

    /// The rules a copy would write now, and what they change, from the saved profiles.
    fn copy_plan(&self, to: &str, from: &str) -> Option<(ConnRules, Vec<Change>, String)> {
        let (p, source) = (self.core.config.get(to)?, self.core.config.get(from)?);
        let c = agents_config::copied(&p, &source);
        let changes = agents_config::conn_changes(&p, &c);
        Some((c, changes, source.name))
    }

    pub(crate) fn open_copy(&mut self, from: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(to) = self.conn.clone() else { return };
        let Some((_, changes, from_name)) = self.copy_plan(&to, &from) else { return };
        self.copy = Some(CopySheet { to, from, from_name, changes });
        window.focus(&self.copy_focus, cx);
        cx.notify();
    }

    fn close_copy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.copy = None;
        self.focus_list(window, cx);
        cx.notify();
    }

    fn apply_copy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.copy.as_ref() else { return };
        let to = s.to.clone();
        let Some((c, changes, _)) = self.copy_plan(&to, &s.from).filter(|_| !s.changes.is_empty()) else {
            self.close_copy(window, cx);
            return;
        };
        // What is written is what the sheet shows: changed since, it shows that first.
        if changes != s.changes {
            if let Some(s) = self.copy.as_mut() {
                s.changes = changes;
            }
            cx.notify();
            return;
        }
        self.edit_conn(&to, |p| agents_config::write_rules(p, &c), cx);
        self.allow_error = None;
        self.yolo_confirm = false;
        self.scan_note = None;
        self.close_copy(window, cx);
    }

    pub(crate) fn render_copy(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let s = self.copy.as_ref()?;
        let n = s.changes.len();
        let loose = s.changes.iter().filter(|c| c.loosens).count();
        let body = if n == 0 {
            div().children(ui::here(&"agents-copy-same".into())).text_color(t.tx2).child("Nothing to change: these settings are already here.")
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(14.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(10.))
                        .child(div().text_color(t.tx).child(format!("{n} {}", if n == 1 { "change" } else { "changes" })))
                        .when(loose > 0, |d| d.child(div().children(ui::here(&"agents-copy-loosens".into())).text_color(t.warn).child(format!("{loose} loosen what agents get")))),
                )
                .child(div().flex().flex_col().gap(px(2.)).children(s.changes.iter().enumerate().map(|(k, c)| Self::change_row(t, "agents-copy-change", k, c))))
        };
        let (ok, loosens) = (n > 0, loose > 0);
        let cancel_first = loosens || !ok;
        let key = |s: &str| ui::keycap(t, s.to_string());
        let cancel = ui::button(t, "agents-copy-cancel", if cancel_first { ui::Btn::Primary } else { ui::Btn::Secondary })
            .h(px(28.))
            .px(px(10.))
            .gap(px(8.))
            .child("Cancel")
            .child(key(if cancel_first { "↩" } else { "esc" }))
            .on_click(cx.listener(|v, _, window, cx| v.close_copy(window, cx)));
        let apply = ok.then(|| {
            ui::button(t, "agents-copy-apply", if loosens { ui::Btn::Danger } else { ui::Btn::Primary })
                .h(px(28.))
                .px(px(10.))
                .gap(px(8.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(if loosens { "Copy anyway" } else { "Copy" })
                .child(key(if loosens { "⌘↩" } else { "↩" }))
                .on_click(cx.listener(|v, _, window, cx| v.apply_copy(window, cx)))
        });
        Some(
            div()
                .id("agents-copy-sheet")
                .absolute()
                .inset_0()
                .bg(t.scrim)
                .occlude()
                .track_focus(&self.copy_focus)
                .on_key_down(cx.listener(move |v, ev: &KeyDownEvent, window, cx| {
                    let k = &ev.keystroke;
                    match k.key.as_str() {
                        "escape" => v.close_copy(window, cx),
                        "enter" if k.modifiers.platform && ok => v.apply_copy(window, cx),
                        "enter" if cancel_first => v.close_copy(window, cx),
                        "enter" => v.apply_copy(window, cx),
                        _ => return,
                    }
                    cx.stop_propagation();
                }))
                .child(ui::esc_closes(None, cx.listener(|v, _: &(), window, cx| v.close_copy(window, cx))))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    ui::modal(t, "agents-copy-card")
                        .w(px(680.))
                        .child(ui::modal_header(t, None, format!("Copy protection from {}", s.from_name), ui::modal_close(t, "agents-copy-close").on_click(cx.listener(|v, _, window, cx| v.close_copy(window, cx)))))
                        .child(div().id("agents-copy-body").max_h(px(460.)).overflow_y_scroll().px(px(20.)).py(px(16.)).child(body))
                        .child(ui::modal_footer(t).child(div().flex_1()).child(cancel).children(apply)),
                )
                .into_any_element(),
        )
    }
}
