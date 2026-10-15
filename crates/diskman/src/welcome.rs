//! First run (Welcome board): no saved connection yet. The logo, a short menu and the
//! isometric memory blocks.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyBinding, Window, actions, div, img, prelude::*,
};
use crate::theme::px;
use diskman_core::ConnectionProfile;

use crate::theme::{ActiveTheme, PIXEL};
use crate::ui;

actions!(welcome, [Up, Down, Choose, PasteUrl, Localhost, NewConn]);

pub fn bind_keys(cx: &mut App) {
    let c = Some("Welcome");
    cx.bind_keys([
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("enter", Choose, c),
        KeyBinding::new("cmd-v", PasteUrl, c),
        KeyBinding::new("cmd-l", Localhost, c),
    ]);
}

pub enum WelcomeEvent {
    NewConnection,
    /// A new connection prefilled (from a pasted URL or localhost), with its password.
    Prefill(ConnectionProfile, Option<String>),
    OpenFile,
    Shortcuts,
}

const ITEMS: [(&str, &str); 5] = [
    ("New connection", "⌘N"),
    ("Paste a connection URL", "⌘V"),
    ("Connect to localhost:5432", "⌘L"),
    ("Open a .sql file", "⌘O"),
    ("Keyboard shortcuts", "⇧⌘/"),
];

pub struct Welcome {
    focus: FocusHandle,
    selected: usize,
    note: Option<String>,
}

impl EventEmitter<WelcomeEvent> for Welcome {}

impl Focusable for Welcome {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Welcome {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Welcome { focus: cx.focus_handle(), selected: 0, note: None }
    }

    fn choose(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = ix;
        self.note = None;
        match ix {
            0 => cx.emit(WelcomeEvent::NewConnection),
            1 => self.paste_url(cx),
            2 => {
                let mut p = ConnectionProfile::new("localhost");
                p.host = "localhost".into();
                p.user = std::env::var("USER").unwrap_or_else(|_| "postgres".into());
                cx.emit(WelcomeEvent::Prefill(p, None));
            }
            3 => cx.emit(WelcomeEvent::OpenFile),
            _ => cx.emit(WelcomeEvent::Shortcuts),
        }
        cx.notify();
    }

    fn paste_url(&mut self, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default();
        // A postgres:// URL or a libpq key=value string (parsed by the core).
        match diskman_core::connstr::parse_profile(&text) {
            Some((p, pw)) => cx.emit(WelcomeEvent::Prefill(p, pw)),
            None => self.note = Some("The clipboard has no connection string".into()),
        }
        cx.notify();
    }
}

impl Render for Welcome {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let art = if t.is_dark() { "art/welcome-dark.svg" } else { "art/welcome-light.svg" };
        let menu = ITEMS.iter().enumerate().map(|(i, (label, key))| {
            let on = i == self.selected;
            let hov = t.hov;
            div()
                .id(("welcome", i))
                .h(px(40.))
                .px(px(14.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(14.))
                .when(on, |d| d.bg(t.acc_s).border_1().border_color(t.acc).text_color(t.tx))
                .when(!on, |d| d.text_color(t.tx2).hover(move |s| s.bg(hov)))
                .on_click(cx.listener(move |w, _, _, cx| w.choose(i, cx)))
                .child(div().w(px(12.)).text_size(px(10.)).text_color(t.acc).child(if on { "▶" } else { "" }))
                .child(div().flex_1().text_size(px(13.)).child(*label))
                .child(ui::keycap(&t, *key))
        });
        // The whole work surface (transparent: the surface paints its fill).
        div()
            .id("welcome")
            .size_full()
            .flex()
            .flex_col()
            .key_context("Welcome")
            .track_focus(&self.focus)
            .on_action(cx.listener(|w, _: &Up, _, cx| {
                w.selected = w.selected.saturating_sub(1);
                cx.notify();
            }))
            .on_action(cx.listener(|w, _: &Down, _, cx| {
                w.selected = (w.selected + 1).min(ITEMS.len() - 1);
                cx.notify();
            }))
            .on_action(cx.listener(|w, _: &Choose, _, cx| {
                let s = w.selected;
                w.choose(s, cx)
            }))
            .on_action(cx.listener(|w, _: &PasteUrl, _, cx| w.paste_url(cx)))
            .on_action(cx.listener(|w, _: &Localhost, _, cx| w.choose(2, cx)))
            .on_action(cx.listener(|_, _: &NewConn, _, cx| cx.emit(WelcomeEvent::NewConnection)))
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|w, _, window, cx| window.focus(&w.focus, cx)))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .pt(px(96.))
                    .px(px(72.))
                    .flex()
                    .flex_row()
                    .gap(px(56.))
                    .overflow_hidden()
                    .child(
                        div()
                            .w(px(420.))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .child(div().font_family(PIXEL).text_size(px(56.)).line_height(px(56.)).text_color(t.tx).child("DISKMAN"))
                            .child(div().mt(px(40.)).flex().flex_col().gap(px(2.)).children(menu))
                            .children(self.note.clone().map(|n| div().mt(px(10.)).text_size(px(11.)).text_color(t.warn).child(n))),
                    )
                    .child(div().flex_1().min_w_0().child(img(art).w(px(460.)).h(px(236.)).ml(px(-20.)))),
            )
    }
}
