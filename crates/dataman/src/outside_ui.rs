//! Edits of connections.toml or settings.toml made outside Dataman (any program of this user,
//! an agent with a shell among them) that loosen protection or move a connection: held, with
//! what was approved applying meanwhile (see `dataman_core::approved`). The review sheet shows
//! them when they appear; Not now is the default and keeps them held, until they change or the
//! palette's "Review edits made outside Dataman"; Apply anyway applies what was shown and logs
//! it in Audit.

use dataman_core::Core;
use dataman_core::agents_config::Change;
use dataman_core::project::ConnReview;
use dataman_mcp::audit::{ApprovalNote, Audit, Decision, Entry, Outcome};
use gpui::{AnyElement, App, Context, FontWeight, MouseButton, Window, actions, div, prelude::*};

use crate::app::Workspace;
use crate::theme::{ActiveTheme, px};
use crate::ui::{self, Btn};

actions!(outside_ui, [OutsideKeep, OutsideApply, ReviewOutsideEdits]);

const SHEET: &str = "OutsideReview";

pub fn bind_keys(cx: &mut App) {
    let c = Some(SHEET);
    cx.bind_keys([crate::keymap::kb("escape", OutsideKeep, c), crate::keymap::kb("enter", OutsideKeep, c), crate::keymap::kb("cmd-enter", OutsideApply, c)]);
}

/// What waits, as shown: Apply anyway applies this and nothing since.
#[derive(Clone, Debug, PartialEq)]
pub struct Outside {
    pub conns: Vec<ConnReview>,
    pub settings: Vec<Change>,
}

impl Outside {
    pub fn of(core: &Core) -> Option<Outside> {
        let o = Outside { conns: core.config.held(), settings: core.settings.held() };
        (!o.conns.is_empty() || !o.settings.is_empty()).then_some(o)
    }

    pub fn loosened(&self) -> usize {
        self.conns.iter().flat_map(|c| &c.changes).chain(&self.settings).filter(|c| c.loosens).count()
    }
}

impl Workspace {
    /// After the files were read: what newly waits is shown, once.
    pub(crate) fn check_outside_edits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let core = self.store.read(cx).core.clone();
        let now = Outside::of(&core);
        if now.is_none() {
            self.outside_declined = None;
            if self.outside_review.take().is_some() {
                self.close_outside_review(false, window, cx);
            }
            return;
        }
        if self.outside_review.is_none() && self.outside_declined != now {
            self.show_outside_review(window, cx);
        }
    }

    pub(crate) fn show_outside_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let core = self.store.read(cx).core.clone();
        let Some(o) = Outside::of(&core) else {
            self.notice("Nothing edited outside Dataman waits for review", cx);
            return;
        };
        if self.outside_review.is_none() {
            self.outside_review_return = window.focused(cx);
        }
        self.outside_review = Some(o);
        window.focus(&self.outside_review_focus, cx);
        cx.notify();
    }

    pub(crate) fn close_outside_review(&mut self, apply: bool, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self.outside_review.take();
        match self.outside_review_return.take() {
            Some(f) => window.focus(&f, cx),
            None => self.focus_editor(window, cx),
        }
        let Some(o) = shown else { return cx.notify() };
        if !apply {
            self.outside_declined = Some(o);
            self.notice("Still held: the palette's \"Review edits made outside Dataman\" shows them again", cx);
            return cx.notify();
        }
        let core = self.store.read(cx).core.clone();
        let started = std::time::Instant::now();
        let result = core.config.accept(&o.conns).and_then(|()| core.settings.accept(&o.settings));
        let reasons: Vec<String> = o.conns.iter().flat_map(|c| c.changes.iter().filter(|x| x.loosens).map(move |x| format!("{} › {}", c.name, x.what))).chain(o.settings.iter().filter(|x| x.loosens).map(|x| x.what.clone())).take(20).collect();
        let (outcome, error) = match &result {
            Ok(()) => (Outcome::Ok, None),
            Err(e) => (Outcome::Error, Some(format!("{e:#}"))),
        };
        Audit::new(core.dirs.mcp_audit_file()).write(&Entry {
            id: None,
            ts: chrono::Utc::now(),
            client: "Dataman".into(),
            transport: "app".into(),
            tool: "outside_review".into(),
            connection: None,
            database: None,
            sql: None,
            query: Some(format!("Applied edits made outside Dataman: {} loosening protection", o.loosened())),
            rows: None,
            ms: started.elapsed().as_secs_f64() * 1000.,
            outcome,
            error,
            approval: Some(ApprovalNote { decision: Decision::Approved, by: Some(dataman_mcp::audit::local_user()), reasons, rows: None }),
            draft: false,
            favorite: None,
            allowed_by: None,
            rewritten: None,
            seen: None,
        });
        self.connections_reloaded(window, cx);
        if let Some(v) = self.settings_view.clone() {
            v.update(cx, |v, cx| v.reload(cx));
        }
        crate::refresh_menus(cx);
        match result {
            Ok(()) => self.notice("Applied the edits made outside Dataman", cx),
            Err(e) => {
                self.notice(format!("{e:#}"), cx);
                self.check_outside_edits(window, cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn render_outside_review(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let o = self.outside_review.as_ref()?;
        let t = cx.theme();
        let mut body = div().flex().flex_col().gap(px(14.)).child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .line_height(px(18.))
                .child(div().text_color(t.tx2).child("Another program changed connections.toml or settings.toml, not Dataman: it gives agents or this Mac more than you approved, or sends connections elsewhere. What you approved applies until you apply this."))
                .child(div().children(ui::here(&"outside-review-loosens".into())).text_color(t.warn).child(format!("{} loosen protection", o.loosened()))),
        );
        let mut k = 0;
        let mut section = |title: String, tag: Option<&'static str>, changes: &[Change]| {
            let rows: Vec<AnyElement> = changes
                .iter()
                .map(|c| {
                    k += 1;
                    crate::project_ui::change_row(&t, "outside-review-change", k - 1, c)
                })
                .collect();
            body = std::mem::replace(&mut body, div()).child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().flex().flex_row().gap(px(8.)).child(ui::caps(&t, title)).children(tag.map(|s| div().text_size(px(10.5)).text_color(t.tx3).child(s))))
                    .children(rows),
            );
        };
        for c in &o.conns {
            let tag = match (c.new, c.moved) {
                (true, _) => Some("new connection: not shown until applied"),
                (_, true) => Some("moves elsewhere: until applied, it still goes where you approved"),
                _ => None,
            };
            section(c.name.to_uppercase(), tag, &c.changes);
        }
        if !o.settings.is_empty() {
            section("SETTINGS".into(), None, &o.settings);
        }
        Some(
            div()
                .id("outside-review-sheet")
                .absolute()
                .inset_0()
                .bg(t.scrim)
                .occlude()
                .key_context(format!("{SHEET} Modal").as_str())
                .track_focus(&self.outside_review_focus)
                .on_action(cx.listener(|w, _: &OutsideKeep, window, cx| w.close_outside_review(false, window, cx)))
                .on_action(cx.listener(|w, _: &OutsideApply, window, cx| w.close_outside_review(true, window, cx)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .flex()
                .items_center()
                .justify_center()
                .child(
                    ui::modal(&t, "outside-review-card")
                        .w(px(680.))
                        .child(ui::modal_header(&t, None, "Edited outside Dataman", ui::modal_close(&t, "outside-review-close").on_click(cx.listener(|w, _, window, cx| w.close_outside_review(false, window, cx)))))
                        .child(div().id("outside-review-body").max_h(px(460.)).overflow_y_scroll().px(px(20.)).py(px(16.)).child(body))
                        .child(
                            ui::modal_footer(&t)
                                .text_size(px(11.))
                                .gap(px(16.))
                                .child(div().flex_1().min_w_0().text_color(t.tx3).child("Applied all at once, and logged in Audit."))
                                .child(
                                    div()
                                        .flex_none()
                                        .flex()
                                        .flex_row()
                                        .gap(px(8.))
                                        .child(
                                            ui::button(&t, "outside-review-keep", Btn::Primary)
                                                .h(px(28.))
                                                .px(px(10.))
                                                .gap(px(8.))
                                                .child("Not now")
                                                .child(ui::key_hint("↩"))
                                                .on_click(cx.listener(|w, _, window, cx| w.close_outside_review(false, window, cx))),
                                        )
                                        .child(
                                            ui::button(&t, "outside-review-apply", Btn::Danger)
                                                .h(px(28.))
                                                .px(px(10.))
                                                .gap(px(8.))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child("Apply anyway")
                                                .child(ui::key_hint("⌘↩"))
                                                .on_click(cx.listener(|w, _, window, cx| w.close_outside_review(true, window, cx))),
                                        ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}
