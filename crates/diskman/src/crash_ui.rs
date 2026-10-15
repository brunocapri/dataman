//! The crash toast: on launch, after the first frame and off the UI thread, the report the last
//! session left (`diskman_core::crash`); one line with Show (Finder), Copy, the macOS report when
//! there is one, and Send when an endpoint is configured. `DISKMAN_CRASH_TEST=panic` (or `abort`)
//! crashes the app inside a GPUI callback a moment after the first frame, to test the whole path.

use std::time::Duration;

use gpui::{AnyElement, App, Context, FontWeight, WeakEntity, div, prelude::*};
use crate::theme::px;
use diskman_core::crash;

use crate::app::Workspace;
use crate::assets::Icon;
use crate::theme::ActiveTheme;
use crate::ui::{self, Btn};

/// How long the toast stays unless dismissed.
const SHOW_FOR: Duration = Duration::from_secs(30);

pub fn check(ws: WeakEntity<Workspace>, cx: &mut App) {
    let task = cx.background_spawn(async move { crash::take_pending(&crash::dir(), &crash::system_dir()) });
    cx.spawn(async move |cx| {
        let Some(p) = task.await else { return };
        log::info!("crash report from the last session: {}", p.primary().display());
        let _ = ws.update(cx, |w, cx| {
            w.crash = Some(p);
            cx.notify();
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SHOW_FOR).await;
                let _ = this.update(cx, |w, cx| {
                    w.crash = None;
                    cx.notify();
                });
            })
            .detach();
        });
    })
    .detach();
}

/// The test crash, inside a GPUI callback (a deferred effect run from the platform's run loop:
/// the panic cannot unwind out of it, so the process aborts right after the hook).
pub fn maybe_crash_for_test(cx: &mut App) {
    let Some(mode) = std::env::var("DISKMAN_CRASH_TEST").ok() else { return };
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_millis(500)).await;
        cx.update(|cx| {
            cx.defer(move |_| {
                if mode == "abort" {
                    std::process::abort();
                }
                panic!("DISKMAN_CRASH_TEST: a deliberate panic in a GPUI callback");
            })
        });
    })
    .detach();
}

impl Workspace {
    pub(crate) fn render_crash_toast(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.crash.as_ref()?;
        let t = cx.theme();
        let when = chrono::DateTime::<chrono::Local>::from(p.at).format("%H:%M").to_string();
        let primary = p.primary().to_path_buf();
        let system = p.report.is_some().then(|| p.system.clone()).flatten();
        let text = p.text();
        let can_send = crash::reporter(self.store.read(cx).core.settings.get().crash_report_url.as_deref()).is_some();
        let note = self.crash_note;
        let small = |id: &'static str, label: &'static str| ui::button(&t, id, Btn::Ghost).h(px(24.)).px(px(8.)).text_size(px(11.5)).child(label);
        Some(
            crate::chrome::toast(&t, t.err)
                .right(px(28.))
                .bottom(px(48.))
                .items_center()
                .py(px(10.))
                .child(div().text_size(px(11.5)).text_color(t.tx).font_weight(FontWeight::SEMIBOLD).whitespace_nowrap().child(format!("Crashed last session · {when}")))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(2.))
                        .child(small("crash-show", "Show").on_click(move |_, _, cx| cx.reveal_path(&primary)))
                        .child(small("crash-copy", if note == Some("copy") { "Copied" } else { "Copy" }).on_click({
                            let text = text.clone();
                            cx.listener(move |w, _, _, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
                                w.crash_note = Some("copy");
                                cx.notify();
                            })
                        }))
                        .children(system.map(|s| small("crash-system", "macOS report").on_click(move |_, _, cx| cx.reveal_path(&s))))
                        .when(can_send, |d| {
                            let label = match note {
                                Some("sending") => "Sending",
                                Some("sent") => "Sent",
                                Some("failed") => "Failed",
                                _ => "Send",
                            };
                            d.child(small("crash-send", label).on_click(cx.listener(move |w, _, _, cx| w.send_crash(text.clone(), cx))))
                        })
                        .child(
                            ui::icon_button(&t, "crash-hide", Icon::Close, 24., 12.).on_click(cx.listener(|w, _, _, cx| {
                                w.crash = None;
                                cx.notify();
                            })),
                        ),
                )
                .into_any_element(),
        )
    }

    fn send_crash(&mut self, text: String, cx: &mut Context<Self>) {
        if matches!(self.crash_note, Some("sending" | "sent")) {
            return;
        }
        let Some(reporter) = crash::reporter(self.store.read(cx).core.settings.get().crash_report_url.as_deref()) else { return };
        self.crash_note = Some("sending");
        cx.notify();
        let task = cx.background_spawn(async move { reporter.send(&text) });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |w, cx| {
                if let Err(e) = &r {
                    log::warn!("crash report not sent: {e:#}");
                }
                w.crash_note = Some(if r.is_ok() { "sent" } else { "failed" });
                cx.notify();
            });
        })
        .detach();
    }
}
