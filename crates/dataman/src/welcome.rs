use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Window, actions, div, prelude::*,
};
use crate::theme::px;
use dataman_core::ConnectionProfile;

use crate::conns::{ConnState, ConnStore};
use crate::theme::{ActiveTheme, Theme, WORDMARK};
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

/// Each way by hand: its glyph, its label, its key.
const ITEMS: [(&str, &str, &str); 6] = [
    ("+", "New connection", "⌘N"),
    ("⎘", "Paste a connection URL", "⌘V"),
    ("→", "Connect to localhost:5432", "⌘L"),
    ("↗", "Open a .sql file", "⌘O"),
    ("↗", "Open a database file", ""),
    ("▢", "Open a project folder", ""),
];

const LOCALHOST: usize = 2;
/// Shown while projects are on (Settings › Project).
const PROJECT: usize = 5;

fn item_shown(i: usize, projects: bool) -> bool {
    i != PROJECT || projects
}

/// What ↑↓↩ is on: an item by hand, or a row of FROM YOUR AGENT (by its place there). The items
/// come first, as they read: on the left, or above when the columns wrap.
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
    agents: Option<Agents>,
    looked: bool,
    copied: ui::CopiedFlash,
    scroll: gpui::ScrollHandle,
    /// Where each row of the ↑↓ list was in the last frame: ↑↓ scrolls to the next one at once.
    drawn: std::rc::Rc<std::cell::RefCell<Vec<(Sel, gpui::Bounds<gpui::Pixels>)>>>,
    /// The connections the agent saved while the Welcome tab was open, in the order they came,
    /// listed until it closes.
    arrivals: Vec<Arrival>,
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
        }
    }

    /// The saved connections changed on disk (`now`, `added` the new ones). Those gone leave
    /// FROM YOUR AGENT; those an agent made while the Welcome tab is `open` join it, the first
    /// to come selected.
    pub(crate) fn saved_elsewhere(&mut self, now: &[ConnectionProfile], added: &[String], open: bool, cx: &mut Context<Self>) {
        let had = self.arrivals.len();
        self.arrivals.retain(|a| now.iter().any(|p| p.id == a.id));
        let pruned = self.arrivals.len() != had;
        let store = self.store.read(cx);
        if open {
            for p in now.iter().filter(|p| p.agent_made && added.contains(&p.id)) {
                if !self.arrivals.iter().any(|a| a.id == p.id) {
                    self.arrivals.push(Arrival::of(p, store.states.get(&p.id)));
                }
            }
        }
        if had == 0 && !self.arrivals.is_empty() {
            self.selected = Sel::Arrival(0);
        }
        self.keep_selection();
        if pruned || self.arrivals.len() != had {
            cx.notify();
        }
    }

    /// The Welcome tab closed: what the agent saved while it was open is listed no more.
    pub(crate) fn closed(&mut self, cx: &mut Context<Self>) {
        if !self.arrivals.is_empty() {
            self.arrivals.clear();
            self.keep_selection();
            cx.notify();
        }
    }

    /// The store changed: each row says how its connect went now; one deleted in the app goes.
    fn restate(&mut self, cx: &mut Context<Self>) {
        if self.arrivals.is_empty() {
            return;
        }
        let store = self.store.clone();
        let store = store.read(cx);
        let now: Vec<Arrival> = self.arrivals.iter().filter_map(|a| store.profile(&a.id).map(|p| Arrival::of(p, store.states.get(&a.id)))).collect();
        if now != self.arrivals {
            self.arrivals = now;
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

    fn step(&mut self, down: bool, cx: &mut Context<Self>) {
        let projects = crate::project_ui::projects(cx).enabled;
        let n = self.arrivals.len();
        let next = |i: usize| (i + 1..ITEMS.len()).find(|j| item_shown(*j, projects));
        let prev = |i: usize| (0..i).rev().find(|j| item_shown(*j, projects));
        let last = (0..ITEMS.len()).rev().find(|j| item_shown(*j, projects)).unwrap_or(0);
        self.selected = match (self.selected, down) {
            (Sel::Item(i), true) => next(i).map(Sel::Item).or((n > 0).then_some(Sel::Arrival(0))).unwrap_or(self.selected),
            (Sel::Item(i), false) => prev(i).map_or(self.selected, Sel::Item),
            (Sel::Arrival(i), true) => Sel::Arrival((i + 1).min(n.saturating_sub(1))),
            (Sel::Arrival(0), false) => Sel::Item(last),
            (Sel::Arrival(i), false) => Sel::Arrival(i - 1),
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

    /// The welcome scrolled to its end, as far down as it goes.
    #[cfg(feature = "checks")]
    pub(crate) fn demo_scroll_to_end(&self) {
        self.scroll.scroll_to_bottom();
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

    /// Each agent: its logo, its name, and Copy for what adds Dataman to it (`Row::copy`).
    fn agent_list(&self, t: &Theme, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let agents = self.agents.as_ref()?;
        let rows = agents.listing.rows.iter().zip(&agents.texts).map(|(r, texts)| {
            let a = r.agent;
            let logo = if a.mono { crate::assets::agent_logo_mono(a.logo).text_color(t.tx).into_any_element() } else { crate::assets::sharp(a.logo, 16.).into_any_element() };
            let copy = self.copied.button(t, texts.copy_id.clone(), ui::Btn::Ghost, "Copy", texts.copy.clone(), |w: &mut Self| &mut w.copied, cx).h(px(24.));
            div()
                .h(px(36.))
                .flex_none()
                .px(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(div().flex_none().mr(px(2.)).child(logo))
                .child(div().flex_1().min_w_0().text_size(px(crate::theme::text::BODY)).text_color(t.tx).child(a.label))
                .child(copy)
        });
        Some(div().mx(px(-8.)).flex().flex_col().children(rows))
    }

    fn prompt(&self, t: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let copy = self.copied.button(t, "welcome-prompt-copy".into(), ui::Btn::Ghost, "Copy", PROMPT.into(), |w: &mut Self| &mut w.copied, cx).h(px(24.));
        // Something to copy, not to type in: a dashed outline and no field's fill.
        div()
            .h(px(48.))
            .pl(px(14.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .border_1()
            .border_dashed()
            .border_color(t.line2)
            .child(div().flex_none().text_size(px(crate::theme::text::BODY)).text_color(t.tx4).child("❯"))
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(crate::theme::text::TITLE)).text_color(t.tx).child(PROMPT))
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
                .h(px(32.))
                .px(px(12.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .map(|d| ui::selectable(d, t, on, true))
                .when(!on, |d| d.hover(move |s| s.bg(hov)))
                .cursor_pointer()
                .on_click(cx.listener(move |w, _, _, cx| w.choose(Sel::Arrival(i), cx)))
                .child(self.tracked(Sel::Arrival(i)))
                .children(crate::assets::small_logo_at(a.logo, 14.))
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
        div().mx(px(-12.)).flex().flex_col().children(rows)
    }
}

/// The wordmark's size, and its font's capital height in ems.
const WORDMARK_PX: f32 = 36.;
const WORDMARK_CAPS: f32 = 0.7;

/// What an address keeps when its row is short of room.
const ADDRESS_KEEPS: f32 = 96.;

/// The agents Dataman can be added to, as this machine has them, with what each row shows made
/// once when they are found: a row is drawn on every frame (the Copied flash animates).
struct Agents {
    listing: std::sync::Arc<dataman_mcp::agents::Listing>,
    /// One per `listing.rows`, in its order.
    texts: Vec<AgentTexts>,
}

struct AgentTexts {
    copy_id: SharedString,
    copy: SharedString,
}

impl Agents {
    /// Looks at the disk (nothing runs): off the UI thread.
    fn here() -> Agents {
        let listing = dataman_mcp::agents::Listing::here();
        let texts = listing
            .rows
            .iter()
            .map(|r| AgentTexts { copy_id: format!("welcome-agent-copy-{}", r.agent.id).into(), copy: r.copy.clone().into() })
            .collect();
        Agents { listing, texts }
    }
}

/// A part of the welcome: its caption over what it holds.
fn part(t: &Theme, caption: &'static str, body: impl IntoElement) -> gpui::Div {
    div().flex().flex_col().child(div().h(px(22.)).mb(px(6.)).flex().items_center().child(ui::caps(t, caption))).child(body)
}

/// One of the two panels, opaque over the pattern; the row they are in gives both the taller
/// one's height.
fn panel(t: &Theme) -> gpui::Div {
    div().relative().flex_grow(1.).flex_basis(px(380.)).min_w_0().flex().flex_col().gap(px(22.)).px(px(24.)).py(px(20.)).bg(t.bg).border_1().border_color(t.line)
}

/// The dotted squares' tile, and its side: a multiple of the squares (16) and of the dots' period
/// (3), so tiles meet without a seam.
const PATTERN: &str = "brand/dots.svg";
const PATTERN_TILE: f32 = 192.;

/// The pattern behind the panels, tiled over what shows of it and no further. Its outermost pixel
/// is left bare all round: a line of the grid there would double the border it meets.
fn pattern(color: gpui::Hsla) -> gpui::Canvas<()> {
    gpui::canvas(
        |_, _, _| {},
        move |b, _, window, cx| {
            let tile = px(PATTERN_TILE);
            let inner = gpui::Bounds::new(gpui::point(b.left() + px(1.), b.top() + px(1.)), gpui::size(b.size.width - px(2.), b.size.height - px(2.)));
            window.with_content_mask(Some(gpui::ContentMask { bounds: inner }), |window| {
                let mut y = b.top();
                while y < b.bottom() {
                    let mut x = b.left();
                    while x < b.right() {
                        if let Err(e) = window.paint_svg(gpui::Bounds::new(gpui::point(x, y), gpui::size(tile, tile)), PATTERN.into(), None, gpui::TransformationMatrix::unit(), color, cx) {
                            log::warn!("the welcome's pattern: {e:#}");
                            return;
                        }
                        x += tile;
                    }
                    y += tile;
                }
            });
        },
    )
}

impl Render for Welcome {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.drawn.borrow_mut().clear();
        if !self.looked {
            self.looked = true;
            cx.spawn(async move |w, cx| {
                let found = cx.background_executor().spawn(async { Agents::here() }).await;
                // The welcome lives as long as the window: gone, nothing is shown.
                let _ = w.update(cx, |w, cx| {
                    w.agents = Some(found);
                    cx.notify();
                });
            })
            .detach();
        }
        let t = cx.theme();
        let projects = crate::project_ui::projects(cx).enabled;
        let menu = (0..ITEMS.len()).filter(|i| item_shown(*i, projects)).map(|i| {
            let (glyph, label, key) = ITEMS[i];
            let on = self.selected == Sel::Item(i);
            let quiet = key.is_empty();
            let hov = t.hov;
            div()
                .children(crate::ui::here(&("welcome", i).into()))
                .id(("welcome", i))
                .h(px(34.))
                .px(px(12.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .map(|d| ui::selectable(d, &t, on, true))
                .when(on, |d| d.text_color(t.tx))
                .when(!on, |d| d.text_color(if quiet { t.tx3 } else { t.tx2 }).hover(move |s| s.bg(hov)))
                .cursor_pointer()
                .on_click(cx.listener(move |w, _, _, cx| w.choose(Sel::Item(i), cx)))
                .child(self.tracked(Sel::Item(i)))
                .child(div().flex_none().w(px(16.)).flex().justify_center().text_size(px(crate::theme::text::TITLE)).text_color(t.tx3).child(glyph))
                .child(div().flex_1().text_size(px(crate::theme::text::BODY)).child(label))
                .when(!quiet, |d| d.child(ui::keycap(&t, key)))
        });
        let menu = div().mx(px(-12.)).flex().flex_col().gap(px(2.)).children(menu);
        // Two panels side by side, as tall as each other; narrower, the agent's wraps under.
        // What the agent saved goes under the items: one ↑↓ list, read top to bottom.
        let by_hand = panel(&t)
            .children(ui::here(&"welcome-hand".into()))
            .child(div().child(part(&t, "CONNECT", menu)).children(self.note.clone().map(|n| div().mt(px(10.)).text_size(px(crate::theme::text::SMALL)).text_color(t.tx2).child(n))))
            .when(!self.arrivals.is_empty(), |d| d.child(part(&t, "FROM YOUR AGENT", self.arrival_rows(&t, cx))));
        // Dataman added to the agent first, then what to ask it.
        let agent = panel(&t).children(ui::here(&"welcome-agent".into())).children(self.agent_list(&t, cx).map(|l| part(&t, "ADD MCP", l))).child(part(&t, "ASK YOUR AGENT", self.prompt(&t, cx)));
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
            // The disc as tall as the capitals, from the baseline to the top of the D. A line as
            // tall as the type puts the capitals' middle on its own (Host Grotesk: ascent 1.015,
            // descent 0.315, capitals 0.7 em), so centring the disc on the line lines it up with them.
            .child(
                div()
                    .h(px(84.))
                    .flex_none()
                    .px(px(56.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(WORDMARK_PX * 0.3))
                    .border_b_1()
                    .border_color(t.pane_line)
                    .child(crate::assets::sharp("brand/dataman-disc.png", WORDMARK_PX * WORDMARK_CAPS))
                    .child(div().font_family(WORDMARK).font_weight(gpui::FontWeight::NORMAL).text_size(px(WORDMARK_PX)).line_height(px(WORDMARK_PX)).text_color(t.tx).child("Dataman")),
            )
            .child(
                div().flex_1().min_h_0().relative().overflow_hidden().child(pattern(t.pattern).absolute().inset_0()).child(
                    div()
                        .id("welcome-body")
                        .track_scroll(&self.scroll)
                        .absolute()
                        .inset_0()
                        .pt(px(32.))
                        .px(px(56.))
                        .pb(px(40.))
                        .overflow_y_scroll()
                        .child(div().flex().flex_row().flex_wrap().items_stretch().gap(px(24.)).child(by_hand).child(agent)),
                ),
            )
    }
}
