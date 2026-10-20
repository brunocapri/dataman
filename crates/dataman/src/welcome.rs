//! First run (Welcome board): no saved connection yet. The logo, a short menu and the
//! isometric memory blocks.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyBinding, Window, actions, div, img, prelude::*,
};
use crate::theme::px;
use dataman_core::ConnectionProfile;

use crate::theme::{ActiveTheme, PIXEL};
use crate::ui;

actions!(welcome, [Up, Down, Choose, PasteUrl, Localhost]);

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
    /// Settings › MCP: an agent adds the connection (`create_connection`).
    AgentSetup,
    /// A new connection prefilled (from a pasted URL or localhost), with its password.
    Prefill(ConnectionProfile, Option<String>),
    OpenFile,
    Shortcuts,
}

/// The menu; an empty key marks the quieter entry under the one it goes with.
const ITEMS: [(&str, &str); 6] = [
    ("New connection", "⌘N"),
    ("Add with an agent", ""),
    ("Paste a connection URL", "⌘V"),
    ("Connect to localhost:5432", "⌘L"),
    ("Open a .sql file", "⌘O"),
    ("Keyboard shortcuts", "⇧⌘/"),
];

/// The localhost entry (⌘L).
const LOCALHOST: usize = 3;

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
            1 => cx.emit(WelcomeEvent::AgentSetup),
            2 => self.paste_url(cx),
            LOCALHOST => {
                let mut p = ConnectionProfile::new("localhost");
                p.host = "localhost".into();
                p.user = std::env::var("USER").unwrap_or_else(|_| "postgres".into());
                cx.emit(WelcomeEvent::Prefill(p, None));
            }
            4 => cx.emit(WelcomeEvent::OpenFile),
            _ => cx.emit(WelcomeEvent::Shortcuts),
        }
        cx.notify();
    }

    /// Checks: picks menu entry `ix` as a click would; its label.
    pub(crate) fn demo_choose(&mut self, ix: usize, cx: &mut Context<Self>) -> &'static str {
        self.choose(ix, cx);
        ITEMS[ix].0
    }

    fn paste_url(&mut self, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default();
        // A postgres:// URL or a libpq key=value string (parsed by the core).
        match dataman_core::connstr::parse_profile(&text) {
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
            let quiet = key.is_empty();
            let hov = t.hov;
            div()
                .id(("welcome", i))
                .h(px(if quiet { 30. } else { 40. }))
                .px(px(14.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(14.))
                .when(on, |d| d.bg(t.acc_s).border_1().border_color(t.acc).text_color(t.tx))
                .when(!on, |d| d.text_color(if quiet { t.tx3 } else { t.tx2 }).hover(move |s| s.bg(hov)))
                .on_click(cx.listener(move |w, _, _, cx| w.choose(i, cx)))
                .child(div().w(px(12.)).text_size(px(10.)).text_color(t.acc).child(if on { "▶" } else { "" }))
                .child(div().flex_1().text_size(px(if quiet { 12. } else { 13. })).child(*label))
                .when(!quiet, |d| d.child(ui::keycap(&t, *key)))
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
            .on_action(cx.listener(|w, _: &Localhost, _, cx| w.choose(LOCALHOST, cx)))
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
                            .child(div().font_family(PIXEL).text_size(px(56.)).line_height(px(56.)).text_color(t.tx).child("DATAMAN"))
                            .child(div().mt(px(40.)).flex().flex_col().gap(px(2.)).children(menu))
                            .children(self.note.clone().map(|n| div().mt(px(10.)).text_size(px(11.)).text_color(t.warn).child(n))),
                    )
                    .child(div().flex_1().min_w_0().child(img(art).w(px(460.)).h(px(236.)).ml(px(-20.)))),
            )
    }
}
