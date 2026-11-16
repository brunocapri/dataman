// pgpass passwords stay in the file (looked up on connect); a service's own password, and
// DBeaver's and TablePlus', go to the Keychain.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Window, actions, div, prelude::*,
};
use crate::theme::px;
use dataman_core::Importable;

use crate::conns::ConnStore;
use crate::theme::ActiveTheme;
use crate::ui::{self, Btn};

actions!(import_sheet, [ImportUp, ImportDown, ImportToggle, ImportConfirm, ImportCancel]);

pub fn bind_keys(cx: &mut App) {
    let c = Some("ImportSheet");
    cx.bind_keys([
        crate::keymap::kb("up", ImportUp, c),
        crate::keymap::kb("down", ImportDown, c),
        crate::keymap::kb("space", ImportToggle, c),
        crate::keymap::kb("enter", ImportToggle, c),
        crate::keymap::kb("cmd-enter", ImportConfirm, c),
        crate::keymap::kb("escape", ImportCancel, c),
    ]);
}

pub enum ImportEvent {
    Close,
    /// The connections saved, and the passwords another app's Keychain did not give (the
    /// connection's name, and why).
    Imported(Vec<String>, Vec<(String, String)>),
}

pub struct ImportView {
    store: Entity<ConnStore>,
    focus: FocusHandle,
    items: Option<Vec<Importable>>,
    picked: Vec<bool>,
    hi: usize,
    error: Option<String>,
    /// Reading another app's Keychain: macOS is asking the user.
    busy: bool,
}

impl EventEmitter<ImportEvent> for ImportView {}

impl Focusable for ImportView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ImportView {
    pub fn new(store: Entity<ConnStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let task = store.read(cx).core.importable();
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |v, cx| {
                match r {
                    Ok(items) if v.items.is_none() => v.set_items(items),
                    Ok(_) => {}
                    Err(e) => v.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        ImportView { store, focus, items: None, picked: vec![], hi: 0, error: None, busy: false }
    }

    pub fn set_items(&mut self, items: Vec<Importable>) {
        self.picked = vec![false; items.len()];
        self.hi = items.iter().position(Importable::pickable).unwrap_or(0);
        self.items = Some(items);
    }

    #[cfg(feature = "checks")]
    pub fn demo_items(&self) -> Option<Vec<(String, dataman_core::ImportSource, bool, bool)>> {
        Some(self.items.as_ref()?.iter().zip(&self.picked).map(|(i, p)| (i.profile.name.clone(), i.source, !i.pickable(), *p)).collect())
    }

    pub fn pick(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(items) = &self.items else { return };
        if items.get(ix).is_some_and(Importable::pickable) {
            self.picked[ix] = !self.picked[ix];
        }
        self.hi = ix;
        cx.notify();
    }

    fn count(&self) -> usize {
        self.picked.iter().filter(|p| **p).count()
    }

    fn step(&mut self, d: isize, cx: &mut Context<Self>) {
        let n = self.items.as_ref().map_or(0, Vec::len);
        if n == 0 {
            return;
        }
        self.hi = (self.hi as isize + d).clamp(0, n as isize - 1) as usize;
        cx.notify();
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let Some(items) = &self.items else { return };
        let chosen: Vec<Importable> = items.iter().zip(&self.picked).filter(|(_, p)| **p).map(|(i, _)| i.clone()).collect();
        if chosen.is_empty() || self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        let core = self.store.read(cx).core.clone();
        let read = core.read_keychain(chosen);
        cx.spawn(async move |this, cx| {
            let r = read.await;
            // The sheet closed meanwhile: nothing was picked any more.
            let _ = this.update(cx, |v, cx| {
                v.busy = false;
                match r.and_then(|(items, unread)| Ok((core.import(items)?, unread))) {
                    Ok((ids, unread)) => {
                        v.store.update(cx, |s, cx| s.reload(cx));
                        cx.emit(ImportEvent::Imported(ids, unread));
                    }
                    Err(e) => v.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for ImportView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let n = self.count();
        let body = match &self.items {
            None => div().h(px(64.)).into_any_element(),
            Some(items) if items.is_empty() => div()
                .h(px(96.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(crate::theme::text::BODY))
                .text_color(t.tx3)
                .child("Nothing to import")
                .into_any_element(),
            Some(items) => div()
                .id("import-list")
                .max_h(px(360.))
                .overflow_y_scroll()
                .py(px(6.))
                .flex()
                .flex_col()
                .children(items.iter().enumerate().map(|(i, it)| {
                    let on = self.picked[i];
                    let hi = i == self.hi;
                    let p = &it.profile;
                    let hov = t.hov;
                    let notes = (!it.notes.is_empty()).then(|| it.notes.join(" · "));
                    div()
                        .id(("import-row", i))
                        .mx(px(8.))
                        .min_h(px(30.))
                        .py(px(5.))
                        .px(px(10.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.))
                        .map(|d| ui::selectable(d, &t, hi, true))
                        .when(!hi, |d| d.hover(move |s| s.bg(hov)))
                        .when(!it.pickable(), |d| d.opacity(0.5))
                        .when(it.pickable(), |d| d.cursor_pointer())
                        .on_click(cx.listener(move |v, _, _, cx| v.pick(i, cx)))
                        .child(ui::checkbox(&t, on))
                        .child(div().flex_none().max_w(px(200.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(p.name.clone()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx3).child(it.refused.clone().unwrap_or_else(|| p.summary())))
                                .children(notes.map(|n| div().text_size(px(crate::theme::text::HINT)).text_color(t.tx4).child(n))),
                        )
                        .child(div().flex_none().text_size(px(crate::theme::text::HINT)).text_color(t.tx4).child(if it.exists { "saved" } else { it.source.label() }))
                }))
                .into_any_element(),
        };
        let card = ui::modal(&t, "import-sheet")
            .w(px(600.))
            .child(ui::modal_header(
                &t,
                None,
                "Import connections",
                ui::modal_close(&t, "imp-close").on_click(cx.listener(|_, _, _, cx| cx.emit(ImportEvent::Close))),
            ))
            .child(body)
            .children(self.error.clone().map(|e| ui::error_block(&t, "imp-error", e).px(px(20.)).pb(px(8.)).text_size(px(crate::theme::text::UI))))
            .child(
                ui::modal_footer(&t)
                    .child(div().flex_1())
                    .child(
                        ui::button(&t, "imp-cancel", Btn::Ghost)
                            .h(px(28.))
                            .px(px(12.))
                            .child("Cancel")
                            .child(div().text_size(px(crate::theme::text::HINT)).opacity(0.6).child("esc"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ImportEvent::Close))),
                    )
                    .child(
                        ui::button(&t, "imp-go", if n > 0 { Btn::Primary } else { Btn::Ghost })
                            .h(px(28.))
                            .px(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .when(n == 0, |d| d.bg(t.prs).text_color(t.tx3))
                            .child(if n > 0 { format!("Import {n}") } else { "Import".into() })
                            .child(div().text_size(px(crate::theme::text::HINT)).font_weight(FontWeight::NORMAL).opacity(0.75).child(crate::ui::keys_text("⌘↩")))
                            .on_click(cx.listener(|v, _, _, cx| v.confirm(cx))),
                    ),
            );
        div()
            .id("import-backdrop")
            .absolute()
            .top(px(crate::theme::metrics::TITLE_H))
            .left_0()
            .right_0()
            .bottom_0()
            .bg(t.scrim)
            .flex()
            .items_center()
            .justify_center()
            .key_context("ImportSheet Modal")
            .track_focus(&self.focus)
            .on_action(cx.listener(|v, _: &ImportUp, _, cx| v.step(-1, cx)))
            .on_action(cx.listener(|v, _: &ImportDown, _, cx| v.step(1, cx)))
            .on_action(cx.listener(|v, _: &ImportToggle, _, cx| {
                let i = v.hi;
                v.pick(i, cx)
            }))
            .on_action(cx.listener(|v, _: &ImportConfirm, _, cx| v.confirm(cx)))
            .on_action(cx.listener(|_, _: &ImportCancel, _, cx| cx.emit(ImportEvent::Close)))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(ui::esc_closes(Some("ImportSheet"), cx.listener(|_, _: &ImportCancel, _, cx| cx.emit(ImportEvent::Close))))
            .child(card)
    }
}
