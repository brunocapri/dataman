//! The update notice: the title bar's pill ("Update available · 0.2.1") and the sheet it opens
//! (the version, its notes, Install & Relaunch / Later, the progress as text, a failure with
//! Retry). The state is `updates`; this only draws it and answers its buttons.

use gpui::{AnyElement, App, Context, FontWeight, KeyBinding, MouseButton, SharedString, Window, actions, div, prelude::*};

use crate::app::Workspace;
use crate::theme::{ActiveTheme, Theme, px};
use crate::ui::{self, Btn};
use crate::updates::{self, State, Step};

actions!(
    update_ui,
    [
        /// The update sheet: Later (hides the notice for this version).
        UpdateLater,
        /// The update sheet: Install & Relaunch, or Retry.
        UpdateInstall,
    ]
);

const SHEET: &str = "UpdateSheet";

pub fn bind_keys(cx: &mut App) {
    let c = Some(SHEET);
    cx.bind_keys([KeyBinding::new("escape", UpdateLater, c), KeyBinding::new("enter", UpdateInstall, c)]);
}

/// Bytes as the Finder says them (decimal megabytes).
fn mb(n: u64) -> String {
    let m = n as f64 / 1e6;
    if m < 10. { format!("{m:.1} MB") } else { format!("{m:.0} MB") }
}

/// The progress line under way, or why it stopped.
fn status(state: &State) -> Option<(SharedString, bool)> {
    match state {
        State::Working { step: Step::Downloading { got, total }, .. } => Some((format!("Downloading… {} of {}", mb(*got), mb(*total)).into(), false)),
        State::Working { step: Step::Verifying, .. } => Some(("Verifying…".into(), false)),
        State::Working { step: Step::Installing, .. } => Some(("Installing…".into(), false)),
        State::Ready { .. } => Some(("Ready to install".into(), false)),
        State::Failed { error, .. } => Some((error.clone().into(), true)),
        State::Available { blocked: Some(b), .. } => Some((b.clone().into(), false)),
        _ => None,
    }
}

impl Workspace {
    pub(crate) fn open_update_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.update_sheet || updates::offered(cx).is_none() {
            return;
        }
        // Another sheet is up: the title bar's notice stays, to open it later.
        if self.modal_open(cx) {
            return;
        }
        self.modal_return = window.focused(cx);
        self.update_sheet = true;
        window.focus(&self.update_focus, cx);
        cx.notify();
    }

    /// Closes the sheet; `later`: the notice hides until a newer version (or the next launch).
    pub(crate) fn close_update_sheet(&mut self, later: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.update_sheet {
            return;
        }
        self.update_sheet = false;
        if later && matches!(updates::state(cx), State::Available { .. }) {
            updates::later(cx);
        }
        if let Some(f) = self.modal_return.take() {
            window.focus(&f, cx);
        } else {
            self.focus_editor(window, cx);
        }
        cx.notify();
    }

    /// The sheet's primary button (and ↩): Install & Relaunch, Retry, or the download page when
    /// this copy cannot replace itself.
    pub(crate) fn update_primary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match updates::state(cx) {
            State::Available { blocked: None, .. } | State::Failed { .. } => updates::install(cx),
            State::Available { feed, blocked: Some(_) } => cx.open_url(&feed.url),
            State::Ready { .. } => self.install_update(window, cx),
            _ => {}
        }
        cx.notify();
    }

    /// The title bar's notice, when an update is on offer.
    pub(crate) fn render_update_pill(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !updates::notice_shown(cx) {
            return None;
        }
        let state = updates::state(cx);
        let feed = state.feed()?;
        let (label, dot) = match &state {
            State::Failed { .. } => (format!("Update failed · {}", feed.version), t.err),
            State::Working { .. } | State::Ready { .. } => (format!("Updating to {}", feed.version), t.acc),
            _ => (format!("Update available · {}", feed.version), t.acc),
        };
        let bg = t.acc_s;
        let hover = gpui::Hsla { a: (bg.a * 1.7).min(1.), ..bg };
        let bounds = self.update_pill_bounds.clone();
        Some(
            div()
                .flex_none()
                .ml(px(12.))
                .on_children_prepainted(move |b, _, _| *bounds.borrow_mut() = b)
                .child(
                    div()
                        .id("update-pill")
                        .h(px(22.))
                        .px(px(8.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .bg(bg)
                        .hover(move |s| s.bg(hover))
                        .text_size(px(11.))
                        .text_color(t.acc_t)
                        .whitespace_nowrap()
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|w, _, window, cx| w.open_update_sheet(window, cx)))
                        .child(div().size(px(6.)).rounded_full().bg(dot))
                        .child(label),
                )
                .into_any_element(),
        )
    }

    /// The update sheet: the version, the notes, the progress or the failure, and the buttons.
    pub(crate) fn render_update_sheet(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.update_sheet {
            return None;
        }
        let state = updates::state(cx);
        let feed = state.feed()?.clone();
        let t = cx.theme();
        let working = matches!(state, State::Working { .. });
        let primary: &'static str = match &state {
            State::Failed { .. } => "Retry",
            State::Available { blocked: Some(_), .. } => "Download",
            _ => "Install & Relaunch",
        };
        let notes: Vec<String> = feed.notes.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
        let size = if feed.size > 0 { format!(" · {}", mb(feed.size)) } else { String::new() };
        let bounds = self.update_button_bounds.clone();
        let later = |w: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| w.close_update_sheet(true, window, cx);
        Some(
            div()
                .id("update-sheet")
                .absolute()
                .inset_0()
                .bg(t.scrim)
                .occlude()
                .key_context(format!("{SHEET} Modal").as_str())
                .track_focus(&self.update_focus)
                .on_action(cx.listener(move |w, _: &UpdateLater, window, cx| later(w, window, cx)))
                .on_action(cx.listener(|w, _: &UpdateInstall, window, cx| w.update_primary(window, cx)))
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(MouseButton::Left, cx.listener(move |w, _, window, cx| later(w, window, cx)))
                .child(
                    ui::modal(&t, "update-card")
                        .w(px(440.))
                        .child(ui::modal_header(
                            &t,
                            None,
                            format!("Dataman {}", feed.version),
                            ui::modal_close(&t, "update-close").on_click(cx.listener(move |w, _, window, cx| later(w, window, cx))),
                        ))
                        .child(
                            div()
                                .px(px(20.))
                                .py(px(16.))
                                .flex()
                                .flex_col()
                                .gap(px(10.))
                                .child(div().text_size(px(11.5)).text_color(t.tx3).child(format!("You have {}{size}", crate::VERSION)))
                                .when(!notes.is_empty(), |d| {
                                    d.child(
                                        div()
                                            .id("update-notes")
                                            .max_h(px(168.))
                                            .overflow_y_scroll()
                                            .flex()
                                            .flex_col()
                                            .gap(px(4.))
                                            .text_color(t.tx2)
                                            .line_height(px(18.))
                                            .children(notes.into_iter().map(|l| div().child(l))),
                                    )
                                }),
                        )
                        .child(
                            ui::modal_footer(&t)
                                .children(status(&state).map(|(s, err)| {
                                    div()
                                        .id("update-status")
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_size(px(11.5))
                                        .text_color(if err { t.err } else { t.tx2 })
                                        .child(s)
                                }))
                                .when(status(&state).is_none(), |d| d.child(div().flex_1()))
                                .child({
                                    let bounds = bounds.clone();
                                    div()
                                        .flex_none()
                                        .flex()
                                        .flex_row()
                                        .gap(px(8.))
                                        .on_children_prepainted(move |b, _, _| *bounds.borrow_mut() = b)
                                        .child(
                                            ui::button(&t, "update-later", Btn::Secondary)
                                                .h(px(28.))
                                                .child(if working { "Hide" } else { "Later" })
                                                .child(ui::key_hint("esc"))
                                                .on_click(cx.listener(move |w, _, window, cx| later(w, window, cx))),
                                        )
                                        // Under way, the progress line takes its place.
                                        .when(!working, |d| {
                                            d.child(
                                                ui::button(&t, "update-install", Btn::Primary)
                                                    .h(px(28.))
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .child(primary)
                                                    .child(ui::key_hint("↩"))
                                                    .on_click(cx.listener(|w, _, window, cx| w.update_primary(window, cx))),
                                            )
                                        })
                                }),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_lines() {
        assert_eq!(mb(12_345_678), "12 MB");
        assert_eq!(mb(1_234_567), "1.2 MB");
        let feed = dataman_core::update::Feed {
            version: "0.2.1".into(),
            notes: String::new(),
            url: String::new(),
            size: 33_000_000,
            ed25519: String::new(),
            minimum_macos: None,
            published: None,
        };
        let s = |st: State| status(&st).map(|(s, e)| (s.to_string(), e));
        assert_eq!(s(State::Working { feed: feed.clone(), step: Step::Downloading { got: 12_000_000, total: 33_000_000 } }), Some(("Downloading… 12 MB of 33 MB".into(), false)));
        assert_eq!(s(State::Working { feed: feed.clone(), step: Step::Verifying }), Some(("Verifying…".into(), false)));
        assert_eq!(s(State::Failed { feed: feed.clone(), error: "signature mismatch".into() }), Some(("signature mismatch".into(), true)));
        assert_eq!(s(State::Available { feed, blocked: None }), None);
    }
}
