//! The update notice: the title bar's pill ("Update available · 0.2.1") and the sheet it opens
//! (the version, its notes, Install & Relaunch / Later, the progress as text, a failure with
//! Retry). A check asked for shows in the same sheet first, as its check card ("Checking…",
//! then up to date or why it failed), which becomes the update when one is found; where no
//! sheet can show (another one is up, or no window is left), an alert says the outcome. The
//! state is `updates`; this only draws it and answers its buttons.

use gpui::{AnyElement, App, Context, FontWeight, KeyBinding, MouseButton, PromptLevel, SharedString, Window, actions, div, prelude::*};

use crate::app::Workspace;
use crate::theme::{ActiveTheme, Theme, px};
use crate::ui::{self, Btn};
use crate::updates::{self, State, Step};
use dataman_core::update::Feed;

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

    /// A check asked for (the menu, the palette, Settings): "Checking…" or, `done`, its outcome,
    /// where it can be seen. With Settings open, its About section says it; otherwise the sheet
    /// (the check card, or the update found), unless another sheet is up: an alert then says
    /// the outcome.
    pub(crate) fn show_update_check(&mut self, done: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(v) = self.settings_view.clone() {
            v.update(cx, |v, cx| v.show_update_status(window, cx));
            return;
        }
        if self.update_sheet {
            cx.notify();
        } else if !self.modal_open(cx) {
            self.modal_return = window.focused(cx);
            self.update_sheet = true;
            window.focus(&self.update_focus, cx);
            cx.notify();
        } else if done {
            alert_in_window(window, cx);
        }
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
    /// this copy cannot replace itself; the check card's OK.
    pub(crate) fn update_primary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match updates::state(cx) {
            State::Available { blocked: None, .. } | State::Failed { .. } => updates::install(cx),
            State::Available { feed, blocked: Some(_) } => cx.open_url(&feed.url),
            State::Ready { .. } => self.install_update(window, cx),
            State::Idle | State::Checking => self.close_update_sheet(false, window, cx),
            State::Working { .. } => {}
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

    /// The update sheet: the update on offer, or the check card while there is none.
    pub(crate) fn render_update_sheet(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.update_sheet {
            return None;
        }
        let state = updates::state(cx);
        let t = cx.theme();
        let card = match state.feed().cloned() {
            Some(feed) => self.render_update_card(feed, &state, &t, cx),
            None => self.render_check_card(&state, &t, cx)?,
        };
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
                .child(card)
                .into_any_element(),
        )
    }

    /// A check asked for, while there is no update to show: "Checking…", then up to date (and
    /// when) or why it failed, with Try Again. Nothing before any check.
    fn render_check_card(&self, state: &State, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let checking = matches!(state, State::Checking);
        let checked = updates::checked(cx);
        if !checking && checked.is_none() {
            return None;
        }
        let error = checked.as_ref().and_then(|c| c.error.clone()).filter(|_| !checking);
        let title = match (&error, checking) {
            (_, true) => "Checking for Updates",
            (Some(_), _) => "Could not check for updates",
            (None, _) => "Dataman is up to date",
        };
        let line: SharedString = match (&error, checking) {
            (_, true) => format!("Looking for a newer version than {}.", crate::VERSION).into(),
            (Some(e), _) => e.clone().into(),
            (None, _) => format!("{} is the newest version.", crate::VERSION).into(),
        };
        let at = checked.filter(|_| !checking).map(|c| format!("{} at {}", if error.is_some() { "Tried" } else { "Checked" }, updates::when(c.at)));
        let bounds = self.update_button_bounds.clone();
        let close = |w: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| w.close_update_sheet(false, window, cx);
        Some(
            ui::modal(t, "update-card")
                .w(px(400.))
                .child(ui::modal_header(t, None, title, ui::modal_close(t, "update-close").on_click(cx.listener(move |w, _, window, cx| close(w, window, cx)))))
                .child(
                    div()
                        .id("update-check-line")
                        .px(px(20.))
                        .py(px(16.))
                        .flex()
                        .flex_row()
                        .gap(px(8.))
                        .line_height(px(18.))
                        .when(error.is_none(), |d| d.child(div().flex_none().text_color(if checking { t.tx3 } else { t.ok }).child(if checking { "◌" } else { "✓" })))
                        .child(div().min_w_0().text_color(if error.is_some() { t.err } else { t.tx2 }).child(line)),
                )
                .child(
                    ui::modal_footer(t)
                        .child(div().id("update-status").flex_1().min_w_0().text_size(px(11.5)).text_color(t.tx3).children(if checking { Some("Checking…".to_string()) } else { at }))
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .flex_row()
                                .gap(px(8.))
                                .on_children_prepainted(move |b, _, _| *bounds.borrow_mut() = b)
                                .when(checking, |d| {
                                    d.child(
                                        ui::button(t, "check-hide", Btn::Secondary)
                                            .h(px(28.))
                                            .child("Hide")
                                            .child(ui::key_hint("esc"))
                                            .on_click(cx.listener(move |w, _, window, cx| close(w, window, cx))),
                                    )
                                })
                                .when(!checking && error.is_some(), |d| {
                                    d.child(ui::button(t, "check-retry", Btn::Secondary).h(px(28.)).child("Try Again").on_click(cx.listener(|_, _, _, cx| updates::check(true, cx))))
                                })
                                .when(!checking, |d| {
                                    d.child(
                                        ui::button(t, "check-ok", Btn::Primary)
                                            .h(px(28.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("OK")
                                            .child(ui::key_hint("↩"))
                                            .on_click(cx.listener(move |w, _, window, cx| close(w, window, cx))),
                                    )
                                }),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The update on offer: the version, the notes, the progress or the failure, and the buttons.
    fn render_update_card(&self, feed: Feed, state: &State, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
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
            )
            .into_any_element()
    }
}

/// A check's outcome when another sheet is up: a native alert on the window (an update found
/// also keeps its title bar notice). The update check stage records it instead.
pub(crate) fn alert_in_window(window: &mut Window, cx: &mut App) {
    let Some((msg, detail, err)) = updates::outcome_text(cx) else { return };
    let detail = if updates::offered(cx).is_some() { format!("{detail} The title bar's notice installs it.") } else { detail };
    if updates::testing(cx) {
        updates::record_alert(format!("window: {msg}"), cx);
        return;
    }
    let level = if err { PromptLevel::Warning } else { PromptLevel::Info };
    let _ = window.prompt(level, &msg, Some(&detail), &["OK"], cx);
}

/// A check's outcome with no window left (closed with the app still running): an alert of the
/// app's own; Download opens an update's DMG. The update check stage records it instead.
pub(crate) fn alert_without_window(cx: &mut App) {
    let Some((msg, detail, err)) = updates::outcome_text(cx) else { return };
    let url = updates::offered(cx).map(|f| f.url).filter(|u| !u.is_empty());
    if updates::testing(cx) {
        updates::record_alert(format!("app: {msg}"), cx);
        return;
    }
    let buttons: &'static [&'static str] = if url.is_some() { &["Download", "Later"] } else { &["OK"] };
    // The alert runs a modal loop of its own: from a task, so the app is not held meanwhile.
    cx.spawn(async move |cx| {
        if native::alert(&msg, &detail, err, buttons) == 0
            && let Some(url) = url
        {
            cx.update(|cx| cx.open_url(&url));
        }
    })
    .detach();
}

#[cfg(target_os = "macos")]
mod native {
    use std::ffi::CString;

    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    fn ns_string(s: &str) -> Retained<AnyObject> {
        let c = CString::new(s.replace('\0', "")).unwrap_or_default();
        // SAFETY: a NUL-terminated UTF-8 string; `stringWithUTF8String:` copies it.
        unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
    }

    /// An application-modal NSAlert (`warning`: its warning style); the index of the button
    /// clicked.
    pub fn alert(message: &str, detail: &str, warning: bool, buttons: &[&str]) -> usize {
        // SAFETY: AppKit's NSAlert as documented, on the main thread (GPUI's foreground tasks
        // run there); the strings are retained for the calls.
        unsafe {
            let alert: Retained<AnyObject> = msg_send![class!(NSAlert), new];
            let _: () = msg_send![&*alert, setMessageText: &*ns_string(message)];
            let _: () = msg_send![&*alert, setInformativeText: &*ns_string(detail)];
            // NSAlertStyleWarning, NSAlertStyleInformational.
            let _: () = msg_send![&*alert, setAlertStyle: if warning { 0usize } else { 1usize }];
            for b in buttons {
                let _: *mut AnyObject = msg_send![&*alert, addButtonWithTitle: &*ns_string(b)];
            }
            // NSAlertFirstButtonReturn is 1000, the next one 1001…
            let r: isize = msg_send![&*alert, runModal];
            (r - 1000).max(0) as usize
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    pub fn alert(message: &str, detail: &str, _: bool, _: &[&str]) -> usize {
        log::info!("{message}: {detail}");
        1
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
