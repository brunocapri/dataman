use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Window, actions, div, prelude::*,
};
use crate::theme::px;
use dataman_core::ConnectionProfile;

use crate::conns::{ConnState, ConnStore};
use crate::theme::{ActiveTheme, BANNER, Theme};
use crate::ui::{self, Tipped as _};

actions!(welcome, [Up, Down, Choose, PasteUrl, Localhost]);

pub fn bind_keys(cx: &mut App) {
    let c = Some("Welcome");
    cx.bind_keys([
        crate::keymap::kb("up", Up, c),
        crate::keymap::kb("down", Down, c),
        crate::keymap::kb("enter", Choose, c),
        crate::keymap::kb("cmd-v", PasteUrl, c),
        crate::keymap::kb("cmd-l", Localhost, c),
    ]);
}

pub enum WelcomeEvent {
    NewConnection,
    /// A sheet filled in; with the connection string it came from, when it did.
    Prefill(ConnectionProfile, Option<String>, Option<String>),
    OpenFile,
    OpenDatabase,
    OpenProject,
    /// A connection under FROM YOUR AGENT, picked.
    Open(String),
}

/// What the user's agent is asked, word for word.
pub const PROMPT: &str = "Set up Dataman for this project.";

const ITEMS: [(&str, &str); 6] = [
    ("New connection", "⌘N"),
    ("Paste a connection URL", "⌘V"),
    ("Connect to localhost:5432", "⌘L"),
    ("Open a .sql file", "⌘O"),
    ("Open a database file", ""),
    ("Open a project folder", ""),
];

const LOCALHOST: usize = 2;
/// Shown while projects are on (Settings › Project).
const PROJECT: usize = 5;

fn item_shown(i: usize, projects: bool) -> bool {
    i != PROJECT || projects
}

/// What ↑↓↩ is on: a row of FROM YOUR AGENT (by its place there), or an item by hand. The
/// agent's rows come first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sel {
    Arrival(usize),
    Item(usize),
}

/// A connection the agent saved while the welcome showed, as its row shows it: made again when
/// the list or the store changes, never on a frame.
#[derive(PartialEq)]
struct Arrival {
    id: String,
    logo: &'static str,
    name: SharedString,
    /// Its host:port/database, or its file.
    at: SharedString,
    /// How its connect went, when there is something to say; its whole text for the tooltip
    /// (a failure's every line), and the width it keeps when the row is short of room.
    state: Option<SharedString>,
    tip: Option<(SharedString, bool)>,
    state_min: f32,
}

/// What a state keeps when its row is short of room: "Needs your password" in full.
const STATE_KEEPS: usize = 19;

impl Arrival {
    fn of(p: &ConnectionProfile, state: Option<&ConnState>) -> Arrival {
        let mut at = crate::agents_rules::place(p);
        if !p.engine.is_file() && !p.database.is_empty() {
            at.push('/');
            at.push_str(&p.database);
        }
        let (state, tip): (Option<SharedString>, _) = match state {
            Some(ConnState::Connecting) => (Some("Connecting…".into()), None),
            // None tried yet: it waits for the user's. One tried and refused says why.
            Some(ConnState::Failed { needs_password: true, password_tried: false, .. }) => (Some("Needs your password".into()), None),
            Some(ConnState::Failed { title, body, .. }) => {
                let shown: SharedString = if title.trim().is_empty() { "Could not connect".into() } else { title.clone().into() };
                let whole = if body.is_empty() { shown.clone() } else { format!("{shown}\n{body}").into() };
                (Some(shown), Some((whole, true)))
            }
            Some(ConnState::Connected(_)) | None => (None, None),
        };
        let tip = tip.or_else(|| state.clone().map(|s| (s, false)));
        // The font is monospaced: so many characters are so wide.
        let state_min = state.as_ref().map_or(0., |s| s.chars().count().min(STATE_KEEPS) as f32 * crate::theme::ADVANCE * crate::theme::text::UI);
        Arrival { id: p.id.clone(), logo: p.logo(), name: p.name.clone().into(), at: at.into(), state, tip, state_min }
    }
}

pub struct Welcome {
    focus: FocusHandle,
    selected: Sel,
    note: Option<String>,
    store: Entity<ConnStore>,
    /// None until the welcome first shows: only then is the disk looked at.
    agents: Option<ui::Agents>,
    looked: bool,
    copied: ui::CopiedFlash,
    scroll: gpui::ScrollHandle,
    /// Where each row of the ↑↓ list was in the last frame: ↑↓ scrolls to the next one at once.
    drawn: std::rc::Rc<std::cell::RefCell<Vec<(Sel, gpui::Bounds<gpui::Pixels>)>>>,
    /// The connections the agent saved while the welcome showed, in the order they came. While
    /// they are all there is and none has been picked, the welcome stays (`holds`).
    arrivals: Vec<Arrival>,
    /// The listed connection whose sheet the user saved: connected, it was picked.
    saved: Option<String>,
}

impl EventEmitter<WelcomeEvent> for Welcome {}

impl Focusable for Welcome {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Welcome {
    pub fn new(store: Entity<ConnStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |w, _, cx| w.restate(cx)).detach();
        Welcome {
            focus: cx.focus_handle(),
            selected: Sel::Item(0),
            note: None,
            store,
            agents: None,
            looked: false,
            copied: Default::default(),
            scroll: gpui::ScrollHandle::new(),
            drawn: Default::default(),
            arrivals: Vec::new(),
            saved: None,
        }
    }

    /// The saved connections changed on disk (`now`, `added` the new ones). Those gone leave
    /// FROM YOUR AGENT; those an agent made while the welcome was `showing` join it, the first
    /// to come selected.
    pub(crate) fn saved_elsewhere(&mut self, now: &[ConnectionProfile], added: &[String], showing: bool, cx: &mut Context<Self>) {
        let had = self.arrivals.len();
        self.arrivals.retain(|a| now.iter().any(|p| p.id == a.id));
        let pruned = self.arrivals.len() != had;
        let store = self.store.read(cx);
        if showing {
            for p in now.iter().filter(|p| p.agent_made && added.contains(&p.id)) {
                if !self.arrivals.iter().any(|a| a.id == p.id) {
                    self.arrivals.push(Arrival::of(p, store.states.get(&p.id)));
                }
            }
        }
        if had == 0 && !self.arrivals.is_empty() {
            self.selected = Sel::Arrival(0);
        }
        self.drop_if_not_alone(now);
        self.keep_selection();
        if pruned || self.arrivals.len() != had {
            cx.notify();
        }
    }

    /// A connection saved that is not the agent's: the welcome is not kept any more, and its
    /// list goes with it (deleting that one later must not bring old rows back).
    fn drop_if_not_alone(&mut self, saved: &[ConnectionProfile]) {
        if saved.iter().any(|p| !self.lists(&p.id)) {
            self.clear();
        }
    }

    /// Nothing listed, nothing waited for: the welcome kept for the agent no more.
    fn clear(&mut self) {
        self.arrivals.clear();
        self.saved = None;
    }

    /// The store changed: each row says how its connect went now; one deleted in the app goes.
    fn restate(&mut self, cx: &mut Context<Self>) {
        if self.arrivals.is_empty() {
            return;
        }
        let store = self.store.clone();
        let store = store.read(cx);
        let now: Vec<Arrival> = self.arrivals.iter().filter_map(|a| store.profile(&a.id).map(|p| Arrival::of(p, store.states.get(&a.id)))).collect();
        let changed = now != self.arrivals;
        self.arrivals = now;
        let had = self.arrivals.len();
        self.drop_if_not_alone(&store.profiles);
        if changed || self.arrivals.len() != had {
            self.keep_selection();
            cx.notify();
        }
    }

    fn keep_selection(&mut self) {
        if let Sel::Arrival(i) = self.selected
            && i >= self.arrivals.len()
        {
            self.selected = self.arrivals.len().checked_sub(1).map_or(Sel::Item(0), Sel::Arrival);
        }
    }

    pub(crate) fn lists(&self, id: &str) -> bool {
        self.arrivals.iter().any(|a| a.id == id)
    }

    /// Whether it stays with `profiles` saved: each came from the agent while it showed, and the
    /// user has picked none yet.
    pub(crate) fn holds(&self, profiles: &[ConnectionProfile]) -> bool {
        !self.arrivals.is_empty() && profiles.iter().all(|p| self.lists(&p.id))
    }

    /// The user picked a connection or went elsewhere: the welcome gives way, and comes back
    /// only when nothing is saved.
    pub(crate) fn release(&mut self, cx: &mut Context<Self>) {
        if !self.arrivals.is_empty() {
            self.clear();
            self.keep_selection();
            cx.notify();
        }
    }

    /// The user saved the sheet of `id` (its password, say): its connect is the user's pick.
    pub(crate) fn sheet_saved(&mut self, id: &str) {
        if self.lists(id) {
            self.saved = Some(id.to_string());
        }
    }

    /// `id` connected: when that was the pick of a sheet saved here, the welcome gives way.
    pub(crate) fn connected(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.saved.as_deref() == Some(id) {
            self.release(cx);
        }
    }

    fn step(&mut self, down: bool, cx: &mut Context<Self>) {
        let projects = crate::project_ui::projects(cx).enabled;
        let n = self.arrivals.len();
        let next = |i: usize| (i + 1..ITEMS.len()).find(|j| item_shown(*j, projects));
        let prev = |i: usize| (0..i).rev().find(|j| item_shown(*j, projects));
        self.selected = match (self.selected, down) {
            (Sel::Arrival(i), true) if i + 1 < n => Sel::Arrival(i + 1),
            (Sel::Arrival(_), true) => Sel::Item(0),
            (Sel::Arrival(i), false) => Sel::Arrival(i.saturating_sub(1)),
            (Sel::Item(i), true) => next(i).map_or(self.selected, Sel::Item),
            (Sel::Item(i), false) => prev(i).map(Sel::Item).or(n.checked_sub(1).map(Sel::Arrival)).unwrap_or(self.selected),
        };
        // From where the row was last drawn: the frame this notify asks for already shows it.
        let to = self.drawn.borrow().iter().find(|(s, _)| *s == self.selected).map(|(_, b)| *b);
        if let Some(at) = to.and_then(|b| ui::scrolled_to(&self.scroll, b, px(16.), px(16.))) {
            self.scroll.set_offset(at);
        }
        cx.notify();
    }

    fn choose(&mut self, sel: Sel, cx: &mut Context<Self>) {
        self.selected = sel;
        self.note = None;
        match sel {
            Sel::Arrival(i) => {
                if let Some(a) = self.arrivals.get(i) {
                    cx.emit(WelcomeEvent::Open(a.id.clone()));
                }
            }
            Sel::Item(0) => cx.emit(WelcomeEvent::NewConnection),
            Sel::Item(1) => self.paste_url(cx),
            Sel::Item(LOCALHOST) => {
                let mut p = ConnectionProfile::new("localhost");
                p.host = "localhost".into();
                p.user = std::env::var("USER").unwrap_or_else(|_| "postgres".into());
                cx.emit(WelcomeEvent::Prefill(p, None, None));
            }
            Sel::Item(3) => cx.emit(WelcomeEvent::OpenFile),
            Sel::Item(4) => cx.emit(WelcomeEvent::OpenDatabase),
            Sel::Item(_) => cx.emit(WelcomeEvent::OpenProject),
        }
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_agents(&self) -> Option<std::sync::Arc<dataman_mcp::agents::Listing>> {
        self.agents.as_ref().map(|a| a.listing.clone())
    }

    /// FROM YOUR AGENT as it reads: each row's connection and what it says of its connect.
    #[cfg(feature = "checks")]
    pub(crate) fn demo_arrivals(&self) -> Vec<(String, Option<String>)> {
        self.arrivals.iter().map(|a| (a.id.clone(), a.state.as_ref().map(|s| s.to_string()))).collect()
    }

    /// Where the welcome's scrolled body shows.
    #[cfg(feature = "checks")]
    pub(crate) fn demo_view(&self) -> gpui::Bounds<gpui::Pixels> {
        self.scroll.bounds()
    }

    /// The row ↑↓ is on under FROM YOUR AGENT, if one is.
    #[cfg(feature = "checks")]
    pub(crate) fn demo_selected_arrival(&self) -> Option<usize> {
        match self.selected {
            Sel::Arrival(i) => Some(i),
            Sel::Item(_) => None,
        }
    }

    fn paste_url(&mut self, cx: &mut Context<Self>) {
        let text = cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default();
        match dataman_core::connstr::parse_profile(&text) {
            Some((p, pw)) => cx.emit(WelcomeEvent::Prefill(p, pw, Some(text.trim().to_string()))),
            None => self.note = Some("The clipboard has no connection string".into()),
        }
        cx.notify();
    }

    fn agent_list(&self, t: &Theme, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let agents = self.agents.as_ref()?;
        let rows = (0..agents.listing.rows.len()).map(|at| ui::agent_row(t, agents, at, [], &self.copied, |w: &mut Self| &mut w.copied, cx));
        Some(div().mx(px(-8.)).flex().flex_col().children(rows))
    }

    fn prompt(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let copy = self.copied.button(t, "welcome-prompt-copy".into(), ui::Btn::Ghost, "Copy", PROMPT.into(), |w: &mut Self| &mut w.copied, cx).h(px(24.));
        ui::field_frame(div(), t, false)
            .h(px(40.))
            .pl(px(12.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .bg(t.field)
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(crate::theme::text::BODY)).text_color(t.tx).child(PROMPT))
            .child(copy)
    }

    /// Records where the row of `sel` is drawn (`drawn`), for ↑↓.
    fn tracked(&self, sel: Sel) -> gpui::AnyElement {
        let drawn = self.drawn.clone();
        gpui::canvas(move |b, _, _| drawn.borrow_mut().push((sel, b)), |_, _, _, _| {}).absolute().top_0().left_0().size_full().into_any_element()
    }

    /// FROM YOUR AGENT: one row per connection that came.
    fn arrival_rows(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let hov = t.hov;
        let rows = self.arrivals.iter().enumerate().map(|(i, a)| {
            let on = self.selected == Sel::Arrival(i);
            div()
                .children(ui::here(&("welcome-from", i).into()))
                .id(("welcome-from", i))
                .h(px(36.))
                .px(px(14.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .map(|d| ui::selectable(d, t, on, true))
                .when(!on, |d| d.hover(move |s| s.bg(hov)))
                .cursor_pointer()
                .on_click(cx.listener(move |w, _, _, cx| w.choose(Sel::Arrival(i), cx)))
                .child(self.tracked(Sel::Arrival(i)))
                .child(marker(t, on).mr(px(4.)))
                .children(crate::assets::small_logo_at(a.logo, 16.))
                .child(div().flex_none().text_size(px(crate::theme::text::BODY)).text_color(t.tx).child(a.name.clone()))
                // Short of room the address gives way first, down to a stub, then the state, down
                // to what it keeps; the name stays. (A flex item shrinks by its factor times its
                // width: 1000 to 1 is the address first.)
                .child(
                    div()
                        .children(ui::here(&("welcome-from-at", i).into()))
                        .id(("welcome-from-at", i))
                        .min_w(px(ADDRESS_KEEPS))
                        .flex_shrink(1000.)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(crate::theme::text::UI))
                        .text_color(t.tx3)
                        .child(a.at.clone()),
                )
                .child(div().flex_1())
                .children(a.state.clone().map(|s| {
                    div()
                        .children(ui::here(&("welcome-from-state", i).into()))
                        .id(("welcome-from-state", i))
                        .min_w(px(a.state_min))
                        .flex_shrink(1.)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(crate::theme::text::UI))
                        .text_color(t.tx3)
                        .child(s)
                        .when_some(a.tip.clone(), |d, (whole, failed)| if failed { d.tip(ui::error_tip(whole)) } else { d.tip(ui::text_tooltip(whole)) })
                }))
        });
        div().mx(px(-14.)).flex().flex_col().gap(px(2.)).children(rows)
    }
}

/// What an address keeps when its row is short of room.
const ADDRESS_KEEPS: f32 = 96.;

/// ▶ on the row ↑↓ is on, in both lists (they are one).
fn marker(t: &Theme, on: bool) -> gpui::Div {
    div().flex_none().w(px(12.)).text_size(px(crate::theme::text::TINY)).text_color(t.acc).child(if on { "▶" } else { "" })
}

/// A part of the welcome: its caption over what it holds.
fn part(t: &Theme, caption: &'static str, body: impl IntoElement) -> gpui::Div {
    div().flex().flex_col().child(div().h(px(26.)).mb(px(6.)).flex().items_center().child(ui::caps(t, caption))).child(body)
}

impl Render for Welcome {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.drawn.borrow_mut().clear();
        if !self.looked {
            self.looked = true;
            ui::find_agents(cx, "welcome-agent-copy", |w: &mut Self, found| w.agents = Some(found));
        }
        let t = cx.theme();
        let projects = crate::project_ui::projects(cx).enabled;
        let menu = (0..ITEMS.len()).filter(|i| item_shown(*i, projects)).map(|i| {
            let (label, key) = ITEMS[i];
            let on = self.selected == Sel::Item(i);
            let quiet = key.is_empty();
            let hov = t.hov;
            div()
                .children(crate::ui::here(&("welcome", i).into()))
                .id(("welcome", i))
                .h(px(if quiet { 30. } else { 40. }))
                .px(px(14.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(14.))
                .map(|d| ui::selectable(d, &t, on, true))
                .when(on, |d| d.text_color(t.tx))
                .when(!on, |d| d.text_color(if quiet { t.tx3 } else { t.tx2 }).hover(move |s| s.bg(hov)))
                .cursor_pointer()
                .on_click(cx.listener(move |w, _, _, cx| w.choose(Sel::Item(i), cx)))
                .child(self.tracked(Sel::Item(i)))
                .child(marker(&t, on))
                .child(div().flex_1().text_size(px(if quiet { 12. } else { 13. })).child(label))
                .when(!quiet, |d| d.child(ui::keycap(&t, key)))
        });
        let menu = div().mx(px(-14.)).flex().flex_col().gap(px(2.)).children(menu);
        // Side by side while both fit at their widths, the agent's way growing 1.25 to the hand's
        // 1 (the prototype's); narrower, BY HAND wraps under it.
        let agent = div()
            .flex()
            .flex_col()
            .gap(px(26.))
            .flex_grow(1.25)
            .flex_basis(px(400.))
            .min_w_0()
            .child(part(&t, "YOUR AGENT", div().children(self.agent_list(&t, cx))))
            .child(part(&t, "ASK YOUR AGENT", self.prompt(&t, cx)))
            .when(!self.arrivals.is_empty(), |d| d.child(part(&t, "FROM YOUR AGENT", self.arrival_rows(&t, cx))));
        let by_hand = div()
            .flex_grow(1.)
            .flex_basis(px(320.))
            .min_w_0()
            .child(part(&t, "BY HAND", menu))
            .children(self.note.clone().map(|n| div().mt(px(10.)).text_size(px(crate::theme::text::SMALL)).text_color(t.tx2).child(n)));
        // The whole work surface (transparent: the surface paints its fill).
        div()
            .id("welcome")
            .size_full()
            .flex()
            .flex_col()
            .key_context("Welcome")
            .track_focus(&self.focus)
            .on_action(cx.listener(|w, _: &Up, _, cx| w.step(false, cx)))
            .on_action(cx.listener(|w, _: &Down, _, cx| w.step(true, cx)))
            .on_action(cx.listener(|w, _: &Choose, _, cx| {
                let s = w.selected;
                w.choose(s, cx)
            }))
            .on_action(cx.listener(|w, _: &PasteUrl, _, cx| w.paste_url(cx)))
            .on_action(cx.listener(|w, _: &Localhost, _, cx| w.choose(Sel::Item(LOCALHOST), cx)))
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|w, _, window, cx| window.focus(&w.focus, cx)))
            .child(
                div()
                    .id("welcome-body")
                    .track_scroll(&self.scroll)
                    .flex_1()
                    .min_h_0()
                    .pt(px(96.))
                    .px(px(72.))
                    .pb(px(48.))
                    .overflow_y_scroll()
                    .child(div().font_family(BANNER).text_size(px(56.)).line_height(px(56.)).text_color(t.tx).child("DATAMAN"))
                    .child(div().mt(px(40.)).flex().flex_row().flex_wrap().items_start().gap_x(px(72.)).gap_y(px(40.)).child(agent).child(by_hand)),
            )
    }
}
