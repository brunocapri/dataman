//! Query history of a connection (side panel), newest first, grouped by day, filterable by
//! text and outcome, and favorite queries (saved across connections, never pruned). ↩ inserts
//! at the cursor, ⌥↩ opens a new tab, ⌘↩ runs; double-click opens a new tab.

use chrono::{Datelike, Local};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, ListAlignment, ListState, SharedString,
    Window, actions, div, list, prelude::*,
};
use crate::theme::px;
use diskman_core::{Favorite, HistoryEntry, Outcome};

use crate::assets::{Icon, icon};
use crate::conns::ConnStore;
use crate::grid::ui_count;
use crate::input::{InputEvent, TextInput};
use crate::theme::{ActiveTheme, Theme};
use crate::ui;

actions!(history_panel, [Prev, Next, Insert, OpenInTab, RunEntry, Close]);

const CONTEXT: &str = "HistoryPanel";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", Prev, c),
        KeyBinding::new("down", Next, c),
        KeyBinding::new("enter", Insert, c),
        KeyBinding::new("alt-enter", OpenInTab, c),
        KeyBinding::new("cmd-enter", RunEntry, c),
        KeyBinding::new("escape", Close, c),
    ]);
}

pub enum HistoryEvent {
    /// Insert the statement at the editor's cursor.
    Insert(String),
    /// A new tab, on the given connection's database (the history entry's).
    OpenInTab(String, Option<String>),
    Run(String),
    /// A favorite, in a new tab on its own connection; `true` also runs it.
    OpenFavorite(Favorite, bool),
    /// Import Favorites…: `.sql` files picked in the panel, a favorite each.
    ImportFavorites,
    /// A favorite was added, renamed or removed here (the tabs linked to one follow).
    FavoritesChanged,
    Close,
}

/// The favorites menu opened on the star toggle (no favorite under it): Import Favorites… only.
const NO_FAVORITE: usize = usize::MAX;

/// A statement's first line, flattened and capped, for a favorite's default name.
pub(crate) fn fav_name(sql: &str) -> String {
    let flat: String = sql.trim_end_matches(';').split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 60 { format!("{}…", flat.chars().take(60).collect::<String>()) } else { flat }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Chip {
    All,
    Ok,
    Error,
    Cancelled,
}

enum Item {
    Day(SharedString),
    Entry(usize),
    Fav(usize),
}

pub struct HistoryPanel {
    store: Entity<ConnStore>,
    focus: FocusHandle,
    conn_id: Option<String>,
    entries: Vec<HistoryEntry>,
    favorites: Vec<Favorite>,
    items: Vec<Item>,
    shown: usize,
    selected: Option<usize>,
    chip: Chip,
    /// Favorites instead of history: a separate toggle, always next to the status dropdown
    /// (never inside it).
    show_favorites: bool,
    /// The status dropdown (All / OK / Error / Cancelled): open, and its button's bounds
    /// (window coordinates), to place the popover under it.
    chip_menu: bool,
    chip_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    /// The favorites toggle's own bounds (window coordinates), for the same reason.
    fav_toggle_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    /// A favorite's context menu (right-click, ⌃-click): its index into `favorites`, and
    /// where to anchor the popover.
    fav_menu: Option<(usize, gpui::Point<gpui::Pixels>)>,
    /// A favorite being renamed inline: its index into `favorites`, and the field.
    renaming: Option<(usize, Entity<TextInput>)>,
    filter: Entity<TextInput>,
    list: ListState,
}

impl EventEmitter<HistoryEvent> for HistoryPanel {}

impl Focusable for HistoryPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl HistoryPanel {
    pub fn new(store: Entity<ConnStore>, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| TextInput::new(cx, "search statements").bare());
        cx.subscribe(&filter, |h: &mut HistoryPanel, _, ev: &InputEvent, cx| match ev {
            InputEvent::Changed => h.apply_filter(cx),
            InputEvent::Submit => h.emit_selected(HistoryEvent::Insert, cx),
        })
        .detach();
        HistoryPanel {
            store,
            focus: cx.focus_handle(),
            conn_id: None,
            entries: vec![],
            favorites: vec![],
            items: vec![],
            shown: 0,
            selected: None,
            chip: Chip::All,
            show_favorites: false,
            chip_menu: false,
            chip_bounds: Default::default(),
            fav_toggle_bounds: Default::default(),
            fav_menu: None,
            renaming: None,
            filter,
            list: ListState::new(0, ListAlignment::Top, px(200.)),
        }
    }

    /// The connection whose history is listed.
    pub fn conn_id(&self) -> Option<&str> {
        self.conn_id.as_deref()
    }

    pub fn filter_focus(&self, cx: &App) -> FocusHandle {
        self.filter.focus_handle(cx)
    }

    /// Puts text in the filter (the palette's "Search history").
    pub fn set_filter(&mut self, text: &str, cx: &mut Context<Self>) {
        self.filter.update(cx, |f, cx| f.set_text(text, cx));
        self.apply_filter(cx);
    }

    /// Shows (and re-reads) the history and favorites of a connection.
    pub fn show(&mut self, conn_id: Option<String>, cx: &mut Context<Self>) {
        self.conn_id = conn_id;
        self.entries = match &self.conn_id {
            Some(id) => self.store.read(cx).core.history.load(id, 1000).unwrap_or_default(),
            None => vec![],
        };
        self.favorites = self.store.read(cx).core.favorites.list();
        self.apply_filter(cx);
    }

    /// Re-reads favorites only (after a toggle), keeping the history as is.
    pub(crate) fn reload_favorites(&mut self, cx: &mut Context<Self>) {
        self.favorites = self.store.read(cx).core.favorites.list();
        self.apply_filter(cx);
    }

    fn is_favorited(&self, sql: &str) -> bool {
        self.favorites.iter().any(|f| f.sql.trim() == sql.trim())
    }

    fn toggle_favorite(&mut self, entry_ix: usize, cx: &mut Context<Self>) {
        let e = &self.entries[entry_ix];
        let (sql, database, conn_id) = (e.sql.clone(), e.database.clone(), self.conn_id.clone());
        let store = self.store.read(cx).core.favorites.clone();
        if let Some(existing) = store.find_by_sql(&sql) {
            let _ = store.remove(&existing.id);
        } else {
            let mut fav = Favorite::new(fav_name(&sql), sql);
            fav.connection_id = conn_id;
            fav.database = Some(database);
            let _ = store.add(fav);
        }
        self.reload_favorites(cx);
        cx.emit(HistoryEvent::FavoritesChanged);
    }

    fn remove_favorite(&mut self, fav_ix: usize, cx: &mut Context<Self>) {
        let id = self.favorites[fav_ix].id.clone();
        let _ = self.store.read(cx).core.favorites.remove(&id);
        self.reload_favorites(cx);
        cx.emit(HistoryEvent::FavoritesChanged);
    }

    /// The favorites view, with favorite `id` selected and scrolled into view, the panel
    /// focused (↑↓ ↩ work at once): the palette's pick, the "Saved as favorite" link. The
    /// search is cleared so the favorite is listed. False when it is not saved (any more).
    pub fn focus_favorite(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.favorites = self.store.read(cx).core.favorites.list();
        self.show_favorites = true;
        self.chip_menu = false;
        self.fav_menu = None;
        self.renaming = None;
        self.filter.update(cx, |f, cx| f.set_text("", cx));
        self.apply_filter(cx);
        let Some(ix) = self.items.iter().position(|it| matches!(it, Item::Fav(i) if self.favorites[*i].id == id)) else { return false };
        self.selected = Some(ix);
        // Item heights are not measured yet after the reset: put the favorite near the top (the
        // list itself keeps the last page full), then reveal it exactly once it is laid out.
        self.list.scroll_to(gpui::ListOffset { item_ix: ix.saturating_sub(2), offset_in_item: px(0.) });
        let list = self.list.clone();
        window.on_next_frame(move |_, _| list.scroll_to_reveal_item(ix));
        window.focus(&self.focus, cx);
        cx.notify();
        true
    }

    /// Starts editing a favorite's name in place: ↩ confirms (`commit_rename`), esc cancels
    /// (the `Close` action, intercepted while renaming), an empty name reverts to the old one.
    fn start_rename(&mut self, fav_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.favorites[fav_ix].name.clone();
        let input = cx.new(|cx| TextInput::new(cx, "name").bare());
        input.update(cx, |i, cx| i.set_text(name, cx));
        cx.subscribe(&input, |h: &mut HistoryPanel, _, ev: &InputEvent, cx| {
            if let InputEvent::Submit = ev {
                h.commit_rename(cx);
            }
        })
        .detach();
        window.focus(&input.focus_handle(cx), cx);
        self.renaming = Some((fav_ix, input));
        // The row (or the panel's own track_focus) takes the focus back on this same click
        // after this handler runs: the field reclaims it once the event is done.
        cx.defer_in(window, |h, window, cx| {
            if let Some((_, input)) = &h.renaming {
                window.focus(&input.focus_handle(cx), cx);
            }
        });
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some((fav_ix, input)) = self.renaming.take() else { return };
        let name = input.read(cx).text().trim().to_string();
        if !name.is_empty()
            && let Some(f) = self.favorites.get(fav_ix)
        {
            let _ = self.store.read(cx).core.favorites.rename(&f.id, &name);
        }
        self.reload_favorites(cx);
        cx.emit(HistoryEvent::FavoritesChanged);
    }

    fn matches(&self, e: &HistoryEntry, q: &str) -> bool {
        let chip = match self.chip {
            Chip::All => true,
            Chip::Ok => e.outcome == Outcome::Ok,
            Chip::Error => e.outcome == Outcome::Error,
            Chip::Cancelled => e.outcome == Outcome::Cancelled,
        };
        chip && (q.is_empty() || e.sql.to_lowercase().contains(q))
    }

    fn apply_filter(&mut self, cx: &mut Context<Self>) {
        let q = self.filter.read(cx).text().to_lowercase();
        let mut items = Vec::new();
        let mut shown = 0;
        if self.show_favorites {
            for (i, f) in self.favorites.iter().enumerate() {
                if q.is_empty() || f.name.to_lowercase().contains(&q) || f.sql.to_lowercase().contains(&q) {
                    items.push(Item::Fav(i));
                    shown += 1;
                }
            }
        } else {
            let today = Local::now().date_naive();
            let mut day = None;
            for (i, e) in self.entries.iter().enumerate() {
                if !self.matches(e, &q) {
                    continue;
                }
                let d = e.ts.with_timezone(&Local).date_naive();
                if day != Some(d) {
                    day = Some(d);
                    let label = if d == today {
                        "TODAY".to_string()
                    } else if Some(d) == today.pred_opt() {
                        "YESTERDAY".to_string()
                    } else if d.year() == today.year() {
                        d.format("%a %d %b").to_string().to_uppercase()
                    } else {
                        d.format("%d %b %Y").to_string().to_uppercase()
                    };
                    items.push(Item::Day(label.into()));
                }
                items.push(Item::Entry(i));
                shown += 1;
            }
        }
        self.items = items;
        self.shown = shown;
        self.selected = self.items.iter().position(|i| !matches!(i, Item::Day(_)));
        self.list.reset(self.items.len());
        cx.notify();
    }

    fn move_sel(&mut self, d: isize, cx: &mut Context<Self>) {
        let picks: Vec<usize> = (0..self.items.len()).filter(|&i| !matches!(self.items[i], Item::Day(_))).collect();
        if picks.is_empty() {
            return;
        }
        let cur = self.selected.and_then(|s| picks.iter().position(|&e| e == s)).unwrap_or(0);
        let next = (cur as isize + d).clamp(0, picks.len() as isize - 1) as usize;
        self.selected = Some(picks[next]);
        self.list.scroll_to_reveal_item(picks[next]);
        cx.notify();
    }

    fn emit_selected(&mut self, f: fn(String) -> HistoryEvent, cx: &mut Context<Self>) {
        let sql = match self.selected.and_then(|s| self.items.get(s)) {
            Some(Item::Entry(i)) => Some(self.entries[*i].sql.clone()),
            Some(Item::Fav(i)) => Some(self.favorites[*i].sql.clone()),
            _ => None,
        };
        if let Some(sql) = sql {
            cx.emit(f(sql));
        }
    }

    /// ⌥↩ and double-click: always a new tab.
    fn open_selected_in_tab(&mut self, cx: &mut Context<Self>) {
        match self.selected.and_then(|s| self.items.get(s)) {
            Some(Item::Entry(i)) => {
                let (sql, database) = (self.entries[*i].sql.clone(), self.entries[*i].database.clone());
                cx.emit(HistoryEvent::OpenInTab(sql, Some(database)));
            }
            Some(Item::Fav(i)) => cx.emit(HistoryEvent::OpenFavorite(self.favorites[*i].clone(), false)),
            _ => {}
        }
    }

    fn run_selected(&mut self, cx: &mut Context<Self>) {
        match self.selected.and_then(|s| self.items.get(s)) {
            Some(Item::Entry(i)) => cx.emit(HistoryEvent::Run(self.entries[*i].sql.clone())),
            Some(Item::Fav(i)) => cx.emit(HistoryEvent::OpenFavorite(self.favorites[*i].clone(), true)),
            _ => {}
        }
    }

    fn render_item(&self, ix: usize, t: &Theme, focused: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        match &self.items[ix] {
            Item::Day(label) => div()
                .h(px(28.))
                .px(px(8.))
                .pb(px(4.))
                .flex()
                .items_end()
                .text_size(px(10.))
                .text_color(t.tx3)
                .child(label.clone())
                .into_any_element(),
            Item::Entry(i) => {
                let entry_i = *i;
                let e = &self.entries[entry_i];
                let sel = self.selected == Some(ix);
                let is_fav = self.is_favorited(&e.sql);
                let (glyph, gcolor) = match e.outcome {
                    Outcome::Ok => ("✓", t.ok),
                    Outcome::Error => ("✕", t.err),
                    Outcome::Cancelled => ("⊘", t.warn),
                };
                let result = match e.outcome {
                    Outcome::Error => error_code(e.error.as_deref()).unwrap_or_else(|| "error".into()),
                    Outcome::Cancelled => match e.rows {
                        Some(n) => format!("cancelled · {}", ui_count(n as usize)),
                        None => "cancelled".into(),
                    },
                    Outcome::Ok => match e.rows {
                        Some(1) => "1 row".into(),
                        Some(n) => format!("{} rows", ui_count(n as usize)),
                        None => "done".into(),
                    },
                };
                let rcolor = match e.outcome {
                    Outcome::Error => t.err,
                    Outcome::Cancelled => t.warn,
                    Outcome::Ok => t.tx3,
                };
                // Leading comment lines are not the statement.
                let body: Vec<&str> = e.sql.lines().skip_while(|l| l.trim().is_empty() || l.trim_start().starts_with("--")).collect();
                let src = if body.is_empty() { e.sql.clone() } else { body.join("\n") };
                let flat: String = src.split_whitespace().collect::<Vec<_>>().join(" ");
                let (verb, rest) = match flat.split_once(' ') {
                    Some((v, r)) => (v.to_string(), format!(" {r}")),
                    None => (flat.clone(), String::new()),
                };
                let time = e.ts.with_timezone(&Local).format("%H:%M:%S").to_string();
                let hov = t.hov;
                div()
                    .px(px(6.))
                    .child(
                        div()
                            .id(("hist", ix))
                            .px(px(8.))
                            .py(px(7.))
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            // Neutral: blue is the explorer's selection.
                            .when(sel && focused, |d| d.bg(t.mark).border_1().border_color(t.edge))
                            .when(sel && !focused, |d| d.bg(t.prs))
                            .when(!sel, |d| d.hover(move |s| s.bg(hov)))
                            .on_click(cx.listener(move |h, ev: &gpui::ClickEvent, window, cx| {
                                h.selected = Some(ix);
                                window.focus(&h.focus, cx);
                                if ev.click_count() >= 2 {
                                    h.open_selected_in_tab(cx);
                                }
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.))
                                    .text_size(px(11.))
                                    .child(div().w(px(12.)).text_center().text_color(gcolor).child(glyph))
                                    .child(div().text_color(t.tx).child(time))
                                    .child(div().text_color(t.tx3).child(fmt_ms(e.duration_ms)))
                                    .when(e.source.as_deref() == Some(diskman_core::history::AGENT), |d| {
                                        d.child(crate::assets::icon(crate::assets::Icon::Agent).size(px(11.)).text_color(t.tx3))
                                    })
                                    .child(div().flex_1())
                                    .child(div().text_color(rcolor).child(result))
                                    .child(fav_button(t, ("fav-toggle", ix), is_fav).on_click(cx.listener(
                                        move |h, _: &gpui::ClickEvent, _, cx| {
                                            cx.stop_propagation();
                                            h.toggle_favorite(entry_i, cx);
                                        },
                                    ))),
                            )
                            .child(
                                div()
                                    .pl(px(20.))
                                    .flex()
                                    .flex_row()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_color(t.tx2)
                                    .child(div().flex_none().text_color(t.kw).child(verb))
                                    .child(div().min_w_0().overflow_hidden().text_ellipsis().child(rest)),
                            ),
                    )
                    .into_any_element()
            }
            Item::Fav(i) => {
                let fav_i = *i;
                let f = &self.favorites[fav_i];
                let sel = self.selected == Some(ix);
                let conn_name = f.connection_id.as_ref().and_then(|id| self.store.read(cx).profile(id)).map(|p| p.name.clone());
                let flat: String = f.sql.split_whitespace().collect::<Vec<_>>().join(" ");
                let hov = t.hov;
                let renaming = self.renaming.as_ref().filter(|(i, _)| *i == fav_i).map(|(_, input)| input.clone());
                // Renaming: the field in the accent, ↩ at its end (it confirms).
                let name: gpui::AnyElement = match renaming {
                    Some(input) => div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.))
                        .border_1()
                        .border_color(t.acc)
                        .pl(px(4.))
                        .pr(px(2.))
                        .text_color(t.tx)
                        .child(div().flex_1().min_w_0().child(input))
                        .child(ui::keycap_sm(t.key, t.tx2, "↩").h(px(14.)).line_height(px(12.)).text_size(px(10.)))
                        .into_any_element(),
                    None => div().text_color(t.tx).child(f.name.clone()).into_any_element(),
                };
                div()
                    .px(px(6.))
                    .child(
                        div()
                            .id(("fav", ix))
                            .px(px(8.))
                            .py(px(7.))
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .when(sel && focused, |d| d.bg(t.mark).border_1().border_color(t.edge))
                            .when(sel && !focused, |d| d.bg(t.prs))
                            .when(!sel, |d| d.hover(move |s| s.bg(hov)))
                            .on_click(cx.listener(move |h, ev: &gpui::ClickEvent, window, cx| {
                                // A click in the rename field belongs to the field: taking the
                                // focus back to the panel here would swallow what is typed next.
                                if h.renaming.as_ref().is_some_and(|(i, _)| *i == fav_i) {
                                    return;
                                }
                                h.selected = Some(ix);
                                window.focus(&h.focus, cx);
                                if ev.click_count() >= 2 {
                                    h.open_selected_in_tab(cx);
                                }
                                cx.notify();
                            }))
                            // Right-click, and ⌃-click (the Mac convention): the favorite's
                            // context menu (Rename, Remove) at the pointer.
                            .on_mouse_down(gpui::MouseButton::Right, cx.listener(move |h, ev: &gpui::MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                h.fav_menu = Some((fav_i, ev.position));
                                cx.notify();
                            }))
                            .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |h, ev: &gpui::MouseDownEvent, _, cx| {
                                if ev.modifiers.control {
                                    cx.stop_propagation();
                                    h.fav_menu = Some((fav_i, ev.position));
                                    cx.notify();
                                }
                            }))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.))
                                    .text_size(px(11.))
                                    .child(name)
                                    .child(div().flex_1())
                                    .children(conn_name.map(|n| div().text_color(t.tx3).child(n)))
                                    .child(fav_button(t, ("fav-remove", ix), true).on_click(cx.listener(
                                        move |h, _: &gpui::ClickEvent, _, cx| {
                                            cx.stop_propagation();
                                            h.remove_favorite(fav_i, cx);
                                        },
                                    ))),
                            )
                            .child(
                                div()
                                    .pl(px(20.))
                                    .flex()
                                    .flex_row()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_color(t.tx2)
                                    .child(flat),
                            ),
                    )
                    .into_any_element()
            }
        }
    }

    /// A favorite's context menu (right-click, ⌃-click): Rename, Remove, Import favorites…; on
    /// the star toggle, Import favorites… alone (sentence case here; the menu bar's File ▸
    /// Import Favorites… keeps the macOS title case).
    fn render_fav_menu(&self, t: &Theme, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let (fav_ix, at) = self.fav_menu?;
        let item = |key: &'static str, label: &'static str, f: fn(&mut Self, usize, &mut Window, &mut Context<Self>)| {
            ui::menu_item(t, key, label, None, true).on_click(cx.listener(move |h, _, window, cx| {
                h.fav_menu = None;
                f(h, fav_ix, window, cx);
                cx.notify();
            }))
        };
        let menu = ui::popover(t)
            .w(px(160.))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|h, _, _, cx| {
                h.fav_menu = None;
                cx.notify();
            }))
            .when(fav_ix != NO_FAVORITE, |d| {
                d.child(item("fm-rename", "Rename", |h, ix, window, cx| h.start_rename(ix, window, cx)))
                    .child(item("fm-remove", "Remove", |h, ix, _, cx| h.remove_favorite(ix, cx)))
                    .child(ui::menu_sep(t))
            })
            .child(item("fm-import", "Import favorites…", |_, _, _, cx| cx.emit(HistoryEvent::ImportFavorites)));
        Some(gpui::deferred(gpui::anchored().position(at).snap_to_window_with_margin(px(8.)).child(menu)).priority(3))
    }

    /// The status filter (All / OK / Error / Cancelled), compact, in the app's picker style
    /// (`chrome::render_conn_picker`). Favorites is a separate, always-visible toggle beside
    /// it (`render_favorites_toggle`), never one of its rows.
    fn render_chip_dropdown(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let label = match self.chip {
            Chip::All => "All",
            Chip::Ok => "OK",
            Chip::Error => "Error",
            Chip::Cancelled => "Cancelled",
        };
        let open = self.chip_menu;
        let (hov, prs) = (t.hov, t.prs);
        let button = div()
            .id("history-chip")
            .h(px(24.))
            .px(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .text_size(px(11.))
            .border_1()
            .when(open, |d| d.bg(prs).border_color(t.line2).text_color(t.tx))
            .when(!open, |d| d.text_color(t.tx3).border_color(t.line).hover(move |s| s.bg(hov)))
            .child(label)
            .child(icon(Icon::ChevronDown).flex_none().size(px(8.)).text_color(t.tx3))
            .on_click(cx.listener(|h, _, _, cx| {
                h.chip_menu = !h.chip_menu;
                cx.notify();
            }));
        let menu = open.then(|| {
            let options = [(Chip::All, "All"), (Chip::Ok, "OK"), (Chip::Error, "Error"), (Chip::Cancelled, "Cancelled")];
            let rows: Vec<_> = options
                .into_iter()
                .map(|(c, lbl)| {
                    let on = self.chip == c && !self.show_favorites;
                    ui::menu_item(t, lbl, lbl, on.then_some("✓"), true)
                        .on_click(cx.listener(move |h, _, _, cx| {
                            // A status filter is the history again: favorites switch off.
                            h.chip = c;
                            h.show_favorites = false;
                            h.chip_menu = false;
                            h.apply_filter(cx);
                        }))
                        .into_any_element()
                })
                .collect();
            div().absolute().left_0().top(px(26.)).child(
                gpui::deferred(
                    gpui::anchored()
                        .snap_to_window_with_margin(px(8.))
                        .child(ui::popover(t).w(px(140.)).on_mouse_down_out(cx.listener(|h, _, _, cx| {
                            h.chip_menu = false;
                            cx.notify();
                        })).children(rows)),
                )
                .priority(3),
            )
        });
        let bounds = self.chip_bounds.clone();
        div().flex().flex_row().relative().on_children_prepainted(move |b, _, _| bounds.set(b.first().copied())).child(button).children(menu)
    }

    /// Favorites instead of history: a star, filled while on. Always next to the status
    /// dropdown, never inside it.
    fn render_favorites_toggle(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let on = self.show_favorites;
        let (hov, prs) = (t.hov, t.prs);
        let button = div()
            .id("history-fav-toggle")
            .h(px(24.))
            .w(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .border_1()
            .when(on, |d| d.bg(prs).border_color(t.line2))
            .when(!on, |d| d.border_color(t.line).hover(move |s| s.bg(hov)))
            .child(icon(if on { Icon::StarFill } else { Icon::Star }).size(px(12.)).text_color(if on { t.warn } else { t.tx3 }))
            .on_click(cx.listener(|h, ev: &gpui::ClickEvent, _, cx| {
                // ⌃-click is the Mac's right click: the favorites menu.
                if ev.modifiers().control {
                    h.fav_menu = Some((NO_FAVORITE, ev.position()));
                    cx.notify();
                    return;
                }
                h.show_favorites = !h.show_favorites;
                h.apply_filter(cx);
            }))
            .on_mouse_down(gpui::MouseButton::Right, cx.listener(|h, ev: &gpui::MouseDownEvent, _, cx| {
                h.fav_menu = Some((NO_FAVORITE, ev.position));
                cx.stop_propagation();
                cx.notify();
            }));
        let bounds = self.fav_toggle_bounds.clone();
        div().flex().flex_row().on_children_prepainted(move |b, _, _| bounds.set(b.first().copied())).child(button)
    }

    // ---------------------------------------------------------------------------------------
    // `history_check` (`--demo`): real mouse and key events need window-coordinate anchors and
    // a way to read state a click cannot report back on its own.

    pub(crate) fn demo_entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Favorites an entry directly (setup for a check: the star click itself is not what
    /// `history-check` is testing).
    pub(crate) fn demo_favorite_entry(&mut self, entry_ix: usize, cx: &mut Context<Self>) {
        self.toggle_favorite(entry_ix, cx);
    }

    /// Screenshots (`marks`, `marks-history`): the history (`favorites` false) with its first
    /// entry selected, or two favorites with the first selected and the second being renamed
    /// (the panel focused).
    pub(crate) fn demo_marks(&mut self, favorites: bool, window: &mut Window, cx: &mut Context<Self>) {
        if favorites {
            let core = self.store.read(cx).core.clone();
            for (name, sql) in [("Orders by day", "SELECT date_trunc('day', p.created) AS day, count(*) FROM orders p GROUP BY 1 ORDER BY 1;"), ("Top customers", "SELECT c.name, count(*) FROM orders p JOIN customer c ON c.id = p.customer GROUP BY 1 ORDER BY 2 DESC LIMIT 10;")] {
                let mut f = diskman_core::Favorite::new(name, sql);
                f.connection_id = self.conn_id.clone();
                let _ = core.favorites.add(f);
            }
            self.reload_favorites(cx);
            self.show_favorites = true;
            self.apply_filter(cx);
        }
        self.selected = self.items.iter().position(|i| matches!(i, Item::Entry(_) | Item::Fav(_)));
        if favorites && let Some(second) = self.items.iter().filter_map(|i| if let Item::Fav(f) = i { Some(*f) } else { None }).nth(1) {
            self.start_rename(second, window, cx);
        } else if favorites {
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }

    /// Screenshots (`marks-search`): the search field focused, holding `text`.
    pub(crate) fn demo_search(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.filter.update(cx, |f, cx| f.set_text(text, cx));
        self.apply_filter(cx);
        window.focus(&self.filter.focus_handle(cx), cx);
    }

    pub(crate) fn demo_favorites(&self) -> Vec<(String, String)> {
        self.favorites.iter().map(|f| (f.id.clone(), f.name.clone())).collect()
    }

    pub(crate) fn demo_showing_favorites(&self) -> bool {
        self.show_favorites
    }

    /// The selected favorite's id, its index among the listed items, and whether its row is
    /// painted whole inside the list's viewport.
    pub(crate) fn demo_selected_favorite(&self) -> Option<(String, usize, bool)> {
        let ix = self.selected?;
        let Item::Fav(i) = self.items.get(ix)? else { return None };
        let view = self.list.viewport_bounds();
        let inside = self.list.bounds_for_item(ix).is_some_and(|b| b.top() >= view.top() - px(0.5) && b.bottom() <= view.bottom() + px(0.5));
        Some((self.favorites[*i].id.clone(), ix, inside))
    }

    /// Screenshots (`fav-menu`): the favorites listed.
    pub(crate) fn demo_show_favorites(&mut self, cx: &mut Context<Self>) {
        self.favorites = self.store.read(cx).core.favorites.list();
        self.show_favorites = true;
        self.apply_filter(cx);
    }

    /// Screenshots (`fav-menu`): the context menu of listed item `item_ix`, as a right-click in
    /// its row opens it.
    pub(crate) fn demo_open_fav_menu(&mut self, item_ix: usize, cx: &mut Context<Self>) {
        if let (Some(Item::Fav(i)), Some(p)) = (self.items.get(item_ix), self.demo_item_point(item_ix)) {
            self.selected = Some(item_ix);
            self.fav_menu = Some((*i, gpui::point(p.x + px(60.), p.y)));
            cx.notify();
        }
    }

    /// Renames favorite `fav_ix` the way the inline field does (Rename, the text, ↩).
    pub(crate) fn demo_rename(&mut self, fav_ix: usize, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.start_rename(fav_ix, window, cx);
        if let Some((_, input)) = &self.renaming {
            input.update(cx, |i, cx| i.set_text(name, cx));
        }
        self.commit_rename(cx);
    }

    pub(crate) fn demo_has_focus(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    pub(crate) fn demo_dropdown_open(&self) -> bool {
        self.chip_menu
    }

    /// The status dropdown button's bounds, in window coordinates (`None` before it is first
    /// painted).
    pub(crate) fn demo_dropdown_bounds(&self) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.chip_bounds.get()
    }

    pub(crate) fn demo_favorites_toggle_bounds(&self) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.fav_toggle_bounds.get()
    }

    pub(crate) fn demo_fav_menu_open(&self) -> bool {
        self.fav_menu.is_some()
    }

    pub(crate) fn demo_renaming_index(&self) -> Option<usize> {
        self.renaming.as_ref().map(|(i, _)| *i)
    }

    /// Whether the rename field itself (not the panel, not the row) actually holds the
    /// keyboard focus: real typing only reaches it when this is true.
    pub(crate) fn demo_renaming_focused(&self, window: &Window, cx: &App) -> bool {
        self.renaming.as_ref().is_some_and(|(_, input)| input.focus_handle(cx).is_focused(window))
    }

    /// A point inside the rename field, as last painted.
    pub(crate) fn demo_rename_field_point(&self, cx: &App) -> Option<gpui::Point<gpui::Pixels>> {
        let (x, y, w, h) = self.renaming.as_ref()?.1.read(cx).demo_bounds()?;
        Some(gpui::point(px(x + w * 0.5), px(y + h * 0.5)))
    }

    /// A point inside item `item_ix` of the visible list (a day header, a history entry or a
    /// favorite), in window coordinates; `None` before it is painted.
    pub(crate) fn demo_item_point(&self, item_ix: usize) -> Option<gpui::Point<gpui::Pixels>> {
        let b = self.list.bounds_for_item(item_ix)?;
        Some(gpui::point(b.origin.x + px(20.), b.origin.y + b.size.height * 0.5))
    }
}

/// A small square star toggle: outline unfavorited, filled (warn color) favorited.
fn fav_button(t: &Theme, id: impl Into<gpui::ElementId>, favorited: bool) -> gpui::Stateful<gpui::Div> {
    let (hov, prs) = (t.hov, t.prs);
    let color = if favorited { t.warn } else { t.tx3 };
    div()
        .id(id)
        .flex_none()
        .size(px(20.))
        .flex()
        .items_center()
        .justify_center()
        .hover(move |s| s.bg(hov))
        .active(move |s| s.bg(prs))
        .child(icon(if favorited { Icon::StarFill } else { Icon::Star }).size(px(12.)).text_color(color))
}

fn fmt_ms(ms: f64) -> String {
    if ms < 1000. { format!("{ms:.0} ms") } else { format!("{:.1} s", ms / 1000.) }
}

/// `[42703] column … does not exist` → `42703`.
fn error_code(e: Option<&str>) -> Option<String> {
    let e = e?;
    let rest = e.strip_prefix('[')?;
    Some(rest.split(']').next()?.to_string())
}

impl Render for HistoryPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let focused = self.focus.contains_focused(window, cx);
        let filter_focused = self.filter.focus_handle(cx).is_focused(window);
        let profile = self.conn_id.as_ref().and_then(|id| self.store.read(cx).profile(id).cloned());
        let name: SharedString = profile.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| "no connection".into()).into();
        let color = profile.as_ref().map(|p| t.profile_color(p));
        let total = if self.show_favorites { self.favorites.len() } else { self.entries.len() };
        let body: gpui::AnyElement = if self.items.is_empty() {
            div()
                .flex_1()
                .px(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .child(ui::empty_state(&t, "[ CLEAN SLATE ]", 20., None))
                .into_any_element()
        } else {
            let theme = t.clone();
            list(self.list.clone(), cx.processor(move |h, ix: usize, _window, cx| h.render_item(ix, &theme, focused, cx)))
                .flex_1()
                .into_any_element()
        };
        // Transparent: the work surface paints the fill once.
        div()
            .size_full()
            .flex()
            .flex_col()
            .text_size(px(12.))
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|h, _: &Prev, _, cx| h.move_sel(-1, cx)))
            .on_action(cx.listener(|h, _: &Next, _, cx| h.move_sel(1, cx)))
            .on_action(cx.listener(|h, _: &Insert, _, cx| h.emit_selected(HistoryEvent::Insert, cx)))
            .on_action(cx.listener(|h, _: &OpenInTab, _, cx| h.open_selected_in_tab(cx)))
            .on_action(cx.listener(|h, _: &RunEntry, _, cx| h.run_selected(cx)))
            .on_action(cx.listener(|h, _: &Close, _, cx| {
                // esc while renaming cancels the rename (the old name stays); otherwise it
                // closes the panel, as usual.
                if h.renaming.take().is_some() {
                    cx.notify();
                } else {
                    cx.emit(HistoryEvent::Close);
                }
            }))
            .child(
                div()
                    .h(px(40.))
                    .flex_none()
                    .pl(px(14.))
                    .pr(px(8.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .border_b_1()
                    .border_color(t.line)
                    .child(ui::caps(&t, "HISTORY"))
                    .children(color.map(|c| ui::chip(c, 8.)))
                    .child(div().flex_1().text_color(t.tx).overflow_hidden().whitespace_nowrap().text_ellipsis().child(name))
                    .child(
                        ui::icon_button(&t, "close-history", Icon::Close, 24., 13.)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(HistoryEvent::Close))),
                    ),
            )
            .child(
                div()
                    .px(px(12.))
                    .pt(px(10.))
                    .pb(px(8.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .h(px(28.))
                            .px(px(8.))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .relative()
                            .border_1()
                            // A search field's focus is neutral, like the explorer filter's: the
                            // border and its halo, the fill unchanged.
                            .border_color(if filter_focused { t.mark_b } else { t.field_b })
                            .when(filter_focused, |d| d.child(ui::halo_ring(t.mark)))
                            .child(icon(Icon::Search).size(px(13.)).text_color(t.tx3))
                            .child(div().flex_1().min_w_0().text_color(t.tx).child(self.filter.clone()))
                            .child(div().text_size(px(11.)).text_color(t.tx3).child(format!("{} of {}", ui_count(self.shown), ui_count(total)))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(4.))
                            .child(self.render_chip_dropdown(&t, cx))
                            .child(self.render_favorites_toggle(&t, cx)),
                    ),
            )
            .child(body)
            .children(self.render_fav_menu(&t, cx))
    }
}
