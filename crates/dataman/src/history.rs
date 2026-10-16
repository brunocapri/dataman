//! Query history of a connection (side panel), newest first, grouped by day, filterable by
//! text and outcome, and favorite queries (saved across connections, never pruned), which the
//! panel opens on. ↩ inserts at the cursor, ⌥↩ opens a new tab, ⌘↩ runs; double-click opens a
//! new tab.
//!
//! Favorites can be filed in folders, drawn as the explorer draws its tree (disclosure
//! chevrons, a folder filled while shut, children indented under it): the header's "+ folder"
//! makes one (after the others) and names it in place, its menu's Rename names it again. A
//! favorite dragged onto a folder (or a favorite in it) is filed there, the folder tinted on
//! the way, and dragged anywhere else in the list it goes back to the top level; a folder
//! dragged moves among the folders, as the explorer's connections do (a line marks the gap),
//! and the store keeps that order. A search opens the folders holding a match. Deleting a
//! folder keeps its favorites: they move to the top level.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use chrono::{Datelike, Local};
use gpui::{
    AnyElement, App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, ListAlignment, ListState,
    Pixels, Point, SharedString, Window, actions, div, list, point, prelude::*,
};
use crate::theme::px;
use dataman_core::favorites::Folder;
use dataman_core::{Favorite, HistoryEntry, Outcome};

use crate::assets::{Icon, icon};
use crate::conns::ConnStore;
use crate::grid::ui_count;
use crate::input::{InputEvent, TextInput};
use crate::theme::{ActiveTheme, Theme};
use crate::tree_menu::{ITEM_H, PAD, SEP_H};
use crate::ui;

actions!(history_panel, [Prev, Next, Insert, OpenInTab, RunEntry, Close, Collapse, Expand]);

const CONTEXT: &str = "HistoryPanel";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", Prev, c),
        KeyBinding::new("down", Next, c),
        KeyBinding::new("left", Collapse, c),
        KeyBinding::new("right", Expand, c),
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

/// A statement's first line, flattened and capped, for a favorite's default name.
pub(crate) fn fav_name(sql: &str) -> String {
    let flat: String = sql.trim_end_matches(';').split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 60 { format!("{}…", flat.chars().take(60).collect::<String>()) } else { flat }
}

/// How far a favorite in a folder sits in from the folder's row: past its chevron, so the
/// favorite's name lines up with the folder's icon (the explorer steps 12 pt a level).
const NEST: f32 = 15.;
/// The favorites' context menu, and its Move to folder submenu.
const MENU_W: f32 = 220.;
const SUB_W: f32 = 200.;

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
    /// A folder of favorites: its index into `folders`, and whether its favorites are listed
    /// under it (open, or a search found some).
    Folder { ix: usize, open: bool },
    /// A favorite (index into `favorites`): indented under its folder when it is in one.
    Fav(usize),
}

/// A favorite on the move (dragged onto a folder, or out to the top level): its id.
pub(crate) struct DraggedFav(pub(crate) String);

/// A folder on the move among the folders: its id.
pub(crate) struct DraggedFolder(pub(crate) String);

/// Where a dragged favorite would land.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DropAt {
    Folder(String),
    Top,
}

/// What is being named in place: a favorite (index into `favorites`) or a folder (its id).
#[derive(Clone, PartialEq)]
enum Naming {
    Fav(usize),
    Folder(String),
}

/// What a context menu is about: a favorite (index into `favorites`), a folder (its id), or
/// the favorites as a whole (the star toggle).
#[derive(Clone, PartialEq)]
enum MenuOn {
    Fav(usize),
    Folder(String),
    Panel,
}

#[derive(Clone, Debug, PartialEq)]
enum Act {
    Run,
    Open,
    Insert,
    Rename,
    /// To a folder (its id), or the top level.
    Move(Option<String>),
    Remove,
    NewFolder,
    DeleteFolder,
    Import,
}

enum MenuEntry {
    Item { act: Act, icon: Icon, label: SharedString, detail: Option<SharedString>, key: Option<&'static str> },
    Sep,
    /// Move to folder ▸: where it can go, the folder it is in checked (and inert).
    Sub { icon: Icon, label: SharedString, items: Vec<(Act, Icon, SharedString, bool)> },
}

impl MenuEntry {
    fn item(act: Act, icon: Icon, label: &'static str, key: Option<&'static str>) -> MenuEntry {
        MenuEntry::Item { act, icon, label: label.into(), detail: None, key }
    }

    fn h(&self) -> f32 {
        if matches!(self, MenuEntry::Sep) { SEP_H } else { ITEM_H }
    }

    fn label(&self) -> Option<&str> {
        match self {
            MenuEntry::Item { label, .. } | MenuEntry::Sub { label, .. } => Some(label),
            MenuEntry::Sep => None,
        }
    }
}

/// A context menu (right-click, ⌃-click) open at `at`: the row under the pointer (`hi`), and
/// the submenu shown (the index of its entry) with its own highlighted row.
struct Menu {
    on: MenuOn,
    at: Point<Pixels>,
    hi: Option<usize>,
    sub: Option<usize>,
    sub_hi: Option<usize>,
}

type BoundsCell = Rc<Cell<Option<Bounds<Pixels>>>>;

pub struct HistoryPanel {
    store: Entity<ConnStore>,
    focus: FocusHandle,
    conn_id: Option<String>,
    entries: Vec<HistoryEntry>,
    favorites: Vec<Favorite>,
    /// The favorites' folders, in the user's order (the store's).
    folders: Vec<Folder>,
    /// The folders open (by id); a folder starts shut, like the explorer's.
    open_folders: HashSet<String>,
    items: Vec<Item>,
    shown: usize,
    selected: Option<usize>,
    chip: Chip,
    /// Favorites instead of history (the view the panel opens on): a separate toggle, always
    /// next to the status dropdown (never inside it).
    show_favorites: bool,
    /// The status dropdown (All / OK / Error / Cancelled): open, and its button's bounds
    /// (window coordinates), to place the popover under it.
    chip_menu: bool,
    chip_bounds: BoundsCell,
    /// The favorites toggle's own bounds (window coordinates), for the same reason.
    fav_toggle_bounds: BoundsCell,
    /// The header's "+ folder" button (the checks click it).
    new_folder_bounds: BoundsCell,
    /// The context menu, and where its popover and submenu were last painted (a click in the
    /// submenu is not "outside" the menu).
    menu: Option<Menu>,
    menu_bounds: BoundsCell,
    sub_bounds: BoundsCell,
    /// A favorite or folder being renamed inline, and the field.
    renaming: Option<(Naming, Entity<TextInput>)>,
    /// While a favorite is dragged: its id, and where it would land.
    dragging: Option<String>,
    drop_at: Option<DropAt>,
    /// While a folder is dragged: its id, and the gap among the folders it would drop into
    /// (`dataman_core::order`'s slot: 0 before the first, `folders.len()` after the last).
    dragging_folder: Option<String>,
    folder_drop: Option<usize>,
    filter: Entity<TextInput>,
    list: ListState,
    /// The panel shares the explorer's dock (`dock`): it shows and hides with the explorer, so
    /// it has no × of its own.
    pub(crate) shares_dock: bool,
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
            folders: vec![],
            open_folders: HashSet::new(),
            items: vec![],
            shown: 0,
            selected: None,
            chip: Chip::All,
            show_favorites: true,
            chip_menu: false,
            chip_bounds: Default::default(),
            fav_toggle_bounds: Default::default(),
            new_folder_bounds: Default::default(),
            menu: None,
            menu_bounds: Default::default(),
            sub_bounds: Default::default(),
            renaming: None,
            dragging: None,
            drop_at: None,
            dragging_folder: None,
            folder_drop: None,
            filter,
            list: ListState::new(0, ListAlignment::Top, px(200.)),
            shares_dock: false,
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
        self.read_favorites(cx);
        self.apply_filter(cx);
    }

    /// Re-reads favorites only (after a toggle), keeping the history as is.
    pub(crate) fn reload_favorites(&mut self, cx: &mut Context<Self>) {
        self.read_favorites(cx);
        self.apply_filter(cx);
    }

    /// The favorites and their folders from the store, the folders in the user's order.
    fn read_favorites(&mut self, cx: &App) {
        let core = &self.store.read(cx).core;
        self.favorites = core.favorites.list();
        self.folders = core.favorites.folders();
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
        self.refresh_keeping(cx);
        cx.emit(HistoryEvent::FavoritesChanged);
    }

    /// The favorites view, with favorite `id` selected and scrolled into view, the panel
    /// focused (↑↓ ↩ work at once): the palette's pick, the "Saved as favorite" link. The
    /// search is cleared so the favorite is listed, and its folder opened. False when it is not
    /// saved (any more).
    pub fn focus_favorite(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.read_favorites(cx);
        self.show_favorites = true;
        self.chip_menu = false;
        self.menu = None;
        self.renaming = None;
        if let Some(d) = self.favorites.iter().find(|f| f.id == id).and_then(|f| f.folder.clone()) {
            self.open_folders.insert(d);
        }
        self.filter.update(cx, |f, cx| f.set_text("", cx));
        self.apply_filter(cx);
        let Some(ix) = self.fav_item(id) else { return false };
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

    /// The listed item of favorite `id`, if it is listed.
    fn fav_item(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|it| matches!(it, Item::Fav(i) if self.favorites[*i].id == id))
    }

    /// The listed item of folder `id`.
    fn folder_item(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|it| matches!(it, Item::Folder { ix, .. } if self.folders[*ix].id == id))
    }

    /// Starts editing a favorite's or a folder's name in place: ↩ confirms (`commit_rename`),
    /// esc cancels (the `Close` action, intercepted while renaming), an empty name reverts to
    /// the old one.
    fn start_naming(&mut self, naming: Naming, window: &mut Window, cx: &mut Context<Self>) {
        let name = match &naming {
            Naming::Fav(i) => self.favorites[*i].name.clone(),
            Naming::Folder(id) => match self.folders.iter().find(|d| d.id == *id) {
                Some(d) => d.name.clone(),
                None => return,
            },
        };
        let input = cx.new(|cx| TextInput::new(cx, "name").bare());
        input.update(cx, |i, cx| {
            i.set_text(name, cx);
            i.select_all(cx);
        });
        cx.subscribe(&input, |h: &mut HistoryPanel, _, ev: &InputEvent, cx| {
            if let InputEvent::Submit = ev {
                h.commit_rename(cx);
            }
        })
        .detach();
        window.focus(&input.focus_handle(cx), cx);
        self.renaming = Some((naming, input));
        // The row (or the panel's own track_focus) takes the focus back on this same click
        // after this handler runs: the field reclaims it once the event is done.
        cx.defer_in(window, |h, window, cx| {
            if let Some((_, input)) = &h.renaming {
                window.focus(&input.focus_handle(cx), cx);
            }
        });
        cx.notify();
    }

    fn start_rename(&mut self, fav_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.start_naming(Naming::Fav(fav_ix), window, cx);
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some((naming, input)) = self.renaming.take() else { return };
        let name = input.read(cx).text().trim().to_string();
        let favorites = self.store.read(cx).core.favorites.clone();
        if !name.is_empty() {
            match &naming {
                Naming::Fav(i) => {
                    if let Some(f) = self.favorites.get(*i) {
                        let _ = favorites.rename(&f.id, &name);
                    }
                }
                Naming::Folder(id) => {
                    let _ = favorites.rename_folder(id, &name);
                }
            }
        }
        // Back to the panel: ↑↓ go on from the row just named.
        self.refresh_keeping(cx);
        if let Naming::Fav(_) = naming {
            cx.emit(HistoryEvent::FavoritesChanged);
        }
    }

    // ---------------------------------------------------------------------------------------
    // Folders

    /// The header's "+ folder": a folder named "New folder" (made unique), listed (the search
    /// cleared, the favorites shown), selected, and its name in an inline field to type over.
    fn new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(d) = self.store.read(cx).core.favorites.add_folder("New folder") else { return };
        self.show_favorites = true;
        self.chip_menu = false;
        self.menu = None;
        self.renaming = None;
        self.open_folders.insert(d.id.clone());
        self.filter.update(cx, |f, cx| f.set_text("", cx));
        self.read_favorites(cx);
        self.apply_filter(cx);
        if let Some(ix) = self.folder_item(&d.id) {
            self.selected = Some(ix);
            self.list.scroll_to_reveal_item(ix);
        }
        self.start_naming(Naming::Folder(d.id), window, cx);
    }

    /// Opens or shuts a folder; shutting the one holding the selection selects the folder.
    fn toggle_folder(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.open_folders.remove(id) {
            self.open_folders.insert(id.to_string());
        } else if let Some(Item::Fav(i)) = self.selected.and_then(|s| self.items.get(s))
            && self.favorites[*i].folder.as_deref() == Some(id)
        {
            self.selected = self.folder_item(id);
        }
        self.refresh_keeping(cx);
    }

    /// Deletes a folder, never its favorites: they move to the top level.
    fn delete_folder(&mut self, id: &str, cx: &mut Context<Self>) {
        let _ = self.store.read(cx).core.favorites.remove_folder(id);
        self.open_folders.remove(id);
        self.refresh_keeping(cx);
    }

    /// Files favorite `id` in a folder (`None`: the top level), opens that folder and selects
    /// the favorite where it now is.
    fn move_favorite(&mut self, id: &str, to: Option<String>, cx: &mut Context<Self>) {
        let _ = self.store.read(cx).core.favorites.move_to_folder(id, to.as_deref());
        if let Some(d) = &to {
            self.open_folders.insert(d.clone());
        }
        self.read_favorites(cx);
        self.apply_filter(cx);
        if let Some(ix) = self.fav_item(id) {
            self.selected = Some(ix);
            self.list.scroll_to_reveal_item(ix);
        }
        cx.notify();
    }

    /// Where favorite `id` dragged to `pos` would land: the folder under the pointer (its row,
    /// or a favorite in it), else the top level; `None` where it already is, or over the
    /// history.
    fn drop_target(&self, id: &str, pos: Point<Pixels>) -> Option<DropAt> {
        let from = self.favorites.iter().find(|f| f.id == id)?.folder.clone();
        let mut at = DropAt::Top;
        for (ix, it) in self.items.iter().enumerate() {
            if !self.list.bounds_for_item(ix).is_some_and(|b| b.contains(&pos)) {
                continue;
            }
            at = match it {
                Item::Folder { ix, .. } => DropAt::Folder(self.folders[*ix].id.clone()),
                Item::Fav(i) => self.favorites[*i].folder.clone().map_or(DropAt::Top, DropAt::Folder),
                Item::Day(_) | Item::Entry(_) => return None,
            };
            break;
        }
        let already = match &at {
            DropAt::Top => from.is_none(),
            DropAt::Folder(d) => from.as_deref() == Some(d.as_str()),
        };
        (!already).then_some(at)
    }

    fn drop_favorite(&mut self, id: &str, cx: &mut Context<Self>) {
        self.dragging = None;
        match self.drop_at.take() {
            Some(DropAt::Folder(d)) => self.move_favorite(id, Some(d), cx),
            Some(DropAt::Top) => self.move_favorite(id, None, cx),
            None => cx.notify(),
        }
    }

    /// Folders are reordered only while every one is listed (no search narrowing them).
    fn can_reorder(&self, cx: &App) -> bool {
        self.show_favorites && self.filter.read(cx).text().is_empty()
    }

    /// The gap among the folders folder `id` dragged to `pos` points at, the explorer's way:
    /// before or after a folder's row by its middle, after the folder anywhere over its
    /// favorites, after the last folder over the top level; `None` where it would not move.
    fn folder_slot(&self, id: &str, pos: Point<Pixels>) -> Option<usize> {
        let from = self.folders.iter().position(|d| d.id == id)?;
        let at = |id: &str| self.folders.iter().position(|d| d.id == id);
        let mut slot = self.folders.len();
        for (ix, it) in self.items.iter().enumerate() {
            let Some(b) = self.list.bounds_for_item(ix).filter(|b| b.contains(&pos)) else { continue };
            slot = match it {
                Item::Folder { ix: k, .. } => crate::dnd::slot_at(*k, &b, pos, false)?,
                Item::Fav(i) => match &self.favorites[*i].folder {
                    Some(d) => at(d)? + 1,
                    None => self.folders.len(),
                },
                Item::Day(_) | Item::Entry(_) => return None,
            };
            break;
        }
        dataman_core::order::moves(from, slot).then_some(slot)
    }

    /// A folder dropped moves to the gap the line marks (and the store keeps the order).
    fn drop_folder(&mut self, id: &str, cx: &mut Context<Self>) {
        self.dragging_folder = None;
        let Some(slot) = self.folder_drop.take() else { return cx.notify() };
        let mut ids: Vec<String> = self.folders.iter().map(|d| d.id.clone()).collect();
        let Some(from) = ids.iter().position(|d| d == id) else { return };
        if dataman_core::order::move_to(&mut ids, from, slot).is_some() {
            let _ = self.store.read(cx).core.favorites.reorder_folders(&ids);
        }
        self.refresh_keeping(cx);
    }

    /// The drop line on listed item `ix` while a folder is dragged: before a folder's row
    /// (`false`), or after the last row of the last folder (`true`, its favorites included).
    fn folder_line(&self, ix: usize) -> Option<bool> {
        let slot = self.folder_drop?;
        let in_folders = |it: &Item| match it {
            Item::Folder { .. } => true,
            Item::Fav(i) => self.favorites[*i].folder.is_some(),
            _ => false,
        };
        match self.items.get(ix)? {
            Item::Folder { ix: k, .. } if *k == slot => Some(false),
            it if slot == self.folders.len() && in_folders(it) && !self.items.get(ix + 1).is_some_and(in_folders) => Some(true),
            _ => None,
        }
    }

    // ---------------------------------------------------------------------------------------
    // The list

    fn matches(&self, e: &HistoryEntry, q: &str) -> bool {
        let chip = match self.chip {
            Chip::All => true,
            Chip::Ok => e.outcome == Outcome::Ok,
            Chip::Error => e.outcome == Outcome::Error,
            Chip::Cancelled => e.outcome == Outcome::Cancelled,
        };
        chip && (q.is_empty() || e.sql.to_lowercase().contains(q))
    }

    /// Rebuilds the list, the selection back on the first row (the search's top match).
    fn apply_filter(&mut self, cx: &mut Context<Self>) {
        let q = self.filter.read(cx).text().to_lowercase();
        let mut items = Vec::new();
        let mut shown = 0;
        if self.show_favorites {
            let hit = |f: &Favorite| q.is_empty() || f.name.to_lowercase().contains(&q) || f.sql.to_lowercase().contains(&q);
            // The folders first, as the explorer lists them; a search lists those holding a
            // match (or named like it), open.
            for (ix, d) in self.folders.iter().enumerate() {
                let named = !q.is_empty() && d.name.to_lowercase().contains(&q);
                let found: Vec<usize> =
                    (0..self.favorites.len()).filter(|&i| self.favorites[i].folder.as_deref() == Some(d.id.as_str()) && (named || hit(&self.favorites[i]))).collect();
                if !q.is_empty() && !named && found.is_empty() {
                    continue;
                }
                let open = !q.is_empty() || self.open_folders.contains(&d.id);
                items.push(Item::Folder { ix, open });
                shown += found.len();
                if open {
                    items.extend(found.into_iter().map(Item::Fav));
                }
            }
            for (i, f) in self.favorites.iter().enumerate() {
                if f.folder.is_none() && hit(f) {
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

    /// Re-reads the favorites and rebuilds the list, the same favorite or folder selected (a
    /// folder opened or shut, a favorite moved or renamed), else the one now in its place.
    fn refresh_keeping(&mut self, cx: &mut Context<Self>) {
        let was = self.selected;
        // (id, a favorite's rather than a folder's)
        let keep = match was.and_then(|s| self.items.get(s)) {
            Some(Item::Fav(i)) => self.favorites.get(*i).map(|f| (f.id.clone(), true)),
            Some(Item::Folder { ix, .. }) => self.folders.get(*ix).map(|d| (d.id.clone(), false)),
            _ => None,
        };
        let top = self.list.logical_scroll_top();
        self.read_favorites(cx);
        self.apply_filter(cx);
        self.list.scroll_to(top);
        let found = keep.and_then(|(id, fav)| if fav { self.fav_item(&id) } else { self.folder_item(&id) });
        let picks = |i: &Item| !matches!(i, Item::Day(_));
        self.selected = found.or_else(|| {
            let w = was?.min(self.items.len().checked_sub(1)?);
            (w..self.items.len()).chain((0..w).rev()).find(|&i| picks(&self.items[i]))
        });
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

    /// ← and →, as in the explorer: → opens the selected folder, ← shuts it, or, on a
    /// favorite in a folder, goes up to the folder.
    fn fold(&mut self, open: bool, cx: &mut Context<Self>) {
        match self.selected.and_then(|s| self.items.get(s)) {
            Some(&Item::Folder { ix, open: is_open, .. }) if is_open != open => {
                let id = self.folders[ix].id.clone();
                self.toggle_folder(&id, cx);
            }
            Some(Item::Fav(i)) if !open => {
                if let Some(ix) = self.favorites[*i].folder.clone().and_then(|d| self.folder_item(&d)) {
                    self.selected = Some(ix);
                    self.list.scroll_to_reveal_item(ix);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    fn emit_selected(&mut self, f: fn(String) -> HistoryEvent, cx: &mut Context<Self>) {
        let sql = match self.selected.and_then(|s| self.items.get(s)) {
            Some(Item::Entry(i)) => Some(self.entries[*i].sql.clone()),
            Some(Item::Fav(i)) => Some(self.favorites[*i].sql.clone()),
            // ↩ on a folder opens or shuts it.
            Some(Item::Folder { ix, .. }) => {
                let id = self.folders[*ix].id.clone();
                self.toggle_folder(&id, cx);
                None
            }
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

    // ---------------------------------------------------------------------------------------
    // Rows

    fn render_item(&self, ix: usize, t: &Theme, focused: bool, cx: &mut Context<Self>) -> AnyElement {
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
            Item::Entry(i) => self.render_entry(ix, *i, t, focused, cx),
            &Item::Folder { ix: folder_ix, open } => self.render_folder(ix, folder_ix, open, t, focused, cx),
            Item::Fav(i) => self.render_fav(ix, *i, t, focused, cx),
        }
    }

    fn render_entry(&self, ix: usize, entry_i: usize, t: &Theme, focused: bool, cx: &mut Context<Self>) -> AnyElement {
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
                            .when(e.source.as_deref() == Some(dataman_core::history::AGENT), |d| {
                                d.child(icon(Icon::Agent).size(px(11.)).text_color(t.tx3))
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

    /// The inline name field: the accent border, ↩ at its end (it confirms).
    fn name_field(input: Entity<TextInput>, t: &Theme) -> AnyElement {
        div()
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
            .into_any_element()
    }

    /// A folder's row, the explorer's way: the chevron, the folder (filled while shut), the
    /// name; tinted in the accent while a favorite dragged over it would drop in. Dragged
    /// itself, it moves among the folders (a line marks the gap, as among the connections).
    fn render_folder(&self, ix: usize, folder_ix: usize, open: bool, t: &Theme, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let d = &self.folders[folder_ix];
        let id = d.id.clone();
        let sel = self.selected == Some(ix);
        let dropping = cx.has_active_drag() && self.drop_at.as_ref() == Some(&DropAt::Folder(id.clone()));
        let dragged = cx.has_active_drag() && self.dragging_folder.as_deref() == Some(id.as_str());
        let line = self.folder_line(ix).filter(|_| cx.has_active_drag());
        let reorder = self.can_reorder(cx);
        let renaming = match &self.renaming {
            Some((Naming::Folder(r), input)) if *r == id => Some(input.clone()),
            _ => None,
        };
        let is_renaming = renaming.is_some();
        let name: AnyElement = match renaming {
            Some(input) => Self::name_field(input, t),
            None => div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(d.name.clone()).into_any_element(),
        };
        let hov = t.hov;
        let chevron = icon(if open { Icon::ChevronDown } else { Icon::ChevronRight }).size(px(10.)).text_color(t.tx4);
        let (click_id, menu_id, ctrl_id, ghost) = (id.clone(), id.clone(), id.clone(), d.name.clone());
        div()
            .px(px(6.))
            .child(
                div()
                    .id(("fav-folder", ix))
                    .relative()
                    .h(px(28.))
                    .pl(px(8.))
                    .pr(px(8.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(5.))
                    .text_size(px(12.))
                    .when(dropping, |d| d.bg(t.acc_s).border_1().border_color(t.acc.opacity(0.45)))
                    .when(!dropping && sel && focused, |d| d.bg(t.mark).border_1().border_color(t.edge))
                    .when(!dropping && sel && !focused, |d| d.bg(t.prs))
                    .when(!dropping && !sel, |d| d.hover(move |s| s.bg(hov)))
                    .when(dragged, |d| d.opacity(0.45))
                    .on_click(cx.listener(move |h, ev: &gpui::ClickEvent, window, cx| {
                        // The rename field's clicks are its own; ⌃-click opened the menu.
                        if is_renaming || ev.modifiers().control {
                            return;
                        }
                        h.selected = Some(ix);
                        window.focus(&h.focus, cx);
                        // A click opens or shuts it (the explorer's way); a double-click is
                        // nothing more than its first click (Rename is in its menu).
                        if ev.click_count() >= 2 {
                            return cx.notify();
                        }
                        h.toggle_folder(&click_id, cx);
                    }))
                    // Among the folders; not while its name is edited, nor while a search
                    // lists only some of them.
                    .when(!is_renaming && reorder, |d| d.on_drag(DraggedFolder(id), move |_, _, _, cx| crate::dnd::ghost(ghost.clone(), None, cx)))
                    .on_mouse_down(gpui::MouseButton::Right, cx.listener(move |h, ev: &gpui::MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        h.selected = Some(ix);
                        h.open_menu(MenuOn::Folder(menu_id.clone()), ev.position, cx);
                    }))
                    .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |h, ev: &gpui::MouseDownEvent, _, cx| {
                        if ev.modifiers.control {
                            cx.stop_propagation();
                            h.selected = Some(ix);
                            h.open_menu(MenuOn::Folder(ctrl_id.clone()), ev.position, cx);
                        }
                    }))
                    .child(div().w(px(10.)).flex_none().flex().justify_center().child(chevron))
                    .child(icon(if open { Icon::Schema } else { Icon::SchemaFilled }).size(px(14.)).text_color(if dropping { t.acc_t } else { t.tx3 }))
                    .child(name)
                    .children(line.map(|after| crate::dnd::drop_line(t, false, after))),
            )
            .into_any_element()
    }

    fn render_fav(&self, ix: usize, fav_i: usize, t: &Theme, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let f = &self.favorites[fav_i];
        let sel = self.selected == Some(ix);
        let profile = f.connection_id.as_ref().and_then(|id| self.store.read(cx).profile(id));
        let conn_name = profile.map(|p| p.name.clone());
        let chip = profile.map(|p| t.profile_color(p));
        let flat: String = f.sql.split_whitespace().collect::<Vec<_>>().join(" ");
        let hov = t.hov;
        let nested = f.folder.is_some();
        let dragged = cx.has_active_drag() && self.dragging.as_deref() == Some(f.id.as_str());
        let renaming = match &self.renaming {
            Some((Naming::Fav(i), input)) if *i == fav_i => Some(input.clone()),
            _ => None,
        };
        let is_renaming = renaming.is_some();
        let name: AnyElement = match renaming {
            Some(input) => Self::name_field(input, t),
            None => div().text_color(t.tx).child(f.name.clone()).into_any_element(),
        };
        let (fav_id, ghost) = (f.id.clone(), f.name.clone());
        // A folder dragged after the last one: the line under its last favorite, as wide as a
        // folder's row.
        let line = self.folder_line(ix).filter(|_| cx.has_active_drag());
        div()
            .relative()
            .pl(px(if nested { 6. + NEST } else { 6. }))
            .pr(px(6.))
            // In a folder: a hairline under its chevron, down the folder's favorites (their
            // cards are tall: the indent alone hardly tells where the folder ends).
            .when(nested, |d| d.child(div().absolute().top_0().bottom_0().left(px(18.5)).w(gpui::px(1.)).bg(t.line)))
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
                    // Its place stays marked, dimmed, while its ghost is on the move.
                    .when(dragged, |d| d.opacity(0.45))
                    .on_click(cx.listener(move |h, ev: &gpui::ClickEvent, window, cx| {
                        // A click in the rename field belongs to the field: taking the
                        // focus back to the panel here would swallow what is typed next.
                        if is_renaming || ev.modifiers().control {
                            return;
                        }
                        h.selected = Some(ix);
                        window.focus(&h.focus, cx);
                        if ev.click_count() >= 2 {
                            h.open_selected_in_tab(cx);
                        }
                        cx.notify();
                    }))
                    // Dragged onto a folder (or out of one); not while its name is edited
                    // (a drag there selects text).
                    .when(!is_renaming, |d| d.on_drag(DraggedFav(fav_id), move |_, _, _, cx| crate::dnd::ghost(ghost.clone(), chip, cx)))
                    // Right-click, and ⌃-click (the Mac convention): the favorite's
                    // context menu (Run, Open, Rename, Move to folder, Remove…) at the pointer.
                    .on_mouse_down(gpui::MouseButton::Right, cx.listener(move |h, ev: &gpui::MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        h.selected = Some(ix);
                        h.open_menu(MenuOn::Fav(fav_i), ev.position, cx);
                    }))
                    .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |h, ev: &gpui::MouseDownEvent, _, cx| {
                        if ev.modifiers.control {
                            cx.stop_propagation();
                            h.selected = Some(ix);
                            h.open_menu(MenuOn::Fav(fav_i), ev.position, cx);
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
            .children(line.map(|after| crate::dnd::drop_line(t, false, after).mx(px(6.))))
            .into_any_element()
    }

    // ---------------------------------------------------------------------------------------
    // The context menu

    fn open_menu(&mut self, on: MenuOn, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.chip_menu = false;
        self.sub_bounds.set(None);
        self.menu = Some(Menu { on, at, hi: None, sub: None, sub_hi: None });
        cx.notify();
    }

    /// What a menu offers. A favorite: Run, Open in new tab, Insert at cursor (their keys on
    /// the selected row), Rename, Move to folder ▸ (once there are folders), Remove, Import
    /// favorites…; a folder: Rename, New folder, Delete folder (it says where its favorites
    /// go); the star toggle: New folder, Import favorites…. Sentence case here; the menu bar's
    /// File ▸ Import Favorites… keeps the macOS title case.
    fn menu_entries(&self, on: &MenuOn) -> Vec<MenuEntry> {
        use MenuEntry as E;
        let import = E::item(Act::Import, Icon::Import, "Import favorites…", None);
        let new_folder = E::item(Act::NewFolder, Icon::Plus, "New folder", None);
        match on {
            MenuOn::Fav(i) => {
                let Some(f) = self.favorites.get(*i) else { return vec![] };
                let mut v = vec![
                    E::item(Act::Run, Icon::Run, "Run", Some("⌘↩")),
                    E::item(Act::Open, Icon::Tab, "Open in new tab", Some("⌥↩")),
                    E::item(Act::Insert, Icon::Cursor, "Insert at cursor", Some("↩")),
                    E::Sep,
                    E::item(Act::Rename, Icon::Edit, "Rename", None),
                ];
                if !self.folders.is_empty() {
                    let mut items = Vec::new();
                    if f.folder.is_some() {
                        items.push((Act::Move(None), Icon::ChevronUp, SharedString::from("Top level"), false));
                    }
                    for d in &self.folders {
                        let here = f.folder.as_deref() == Some(d.id.as_str());
                        items.push((Act::Move(Some(d.id.clone())), Icon::Schema, d.name.clone().into(), here));
                    }
                    v.push(E::Sub { icon: Icon::Schema, label: "Move to folder".into(), items });
                }
                v.extend([E::Sep, E::item(Act::Remove, Icon::Trash, "Remove", None), E::Sep, import]);
                v
            }
            MenuOn::Folder(id) => {
                let n = self.favorites.iter().filter(|f| f.folder.as_deref() == Some(id.as_str())).count();
                let delete = E::Item {
                    act: Act::DeleteFolder,
                    icon: Icon::Trash,
                    label: "Delete folder".into(),
                    // Its favorites are kept: say where they go.
                    detail: (n > 0).then(|| if n == 1 { "1 favorite to top level".into() } else { format!("{n} favorites to top level").into() }),
                    key: None,
                };
                vec![E::item(Act::Rename, Icon::Edit, "Rename", None), new_folder, E::Sep, delete]
            }
            MenuOn::Panel => vec![new_folder, E::Sep, import],
        }
    }

    fn run_menu(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.menu.take() else { return };
        let fav = match m.on {
            MenuOn::Fav(i) => self.favorites.get(i).cloned().map(|f| (i, f)),
            _ => None,
        };
        match (act, fav) {
            (Act::Run, Some((_, f))) => cx.emit(HistoryEvent::OpenFavorite(f, true)),
            (Act::Open, Some((_, f))) => cx.emit(HistoryEvent::OpenFavorite(f, false)),
            (Act::Insert, Some((_, f))) => cx.emit(HistoryEvent::Insert(f.sql)),
            (Act::Rename, Some((i, _))) => self.start_rename(i, window, cx),
            (Act::Rename, None) => {
                if let MenuOn::Folder(id) = m.on {
                    self.start_naming(Naming::Folder(id), window, cx);
                }
            }
            (Act::Move(to), Some((_, f))) => self.move_favorite(&f.id, to, cx),
            (Act::Remove, Some((i, _))) => self.remove_favorite(i, cx),
            (Act::NewFolder, _) => self.new_folder(window, cx),
            (Act::DeleteFolder, _) => {
                if let MenuOn::Folder(id) = m.on {
                    self.delete_folder(&id, cx);
                }
            }
            (Act::Import, _) => cx.emit(HistoryEvent::ImportFavorites),
            _ => {}
        }
        cx.notify();
    }

    /// The menu at the pointer, in the explorer's look (`grid_menu::row`: icon, label, a muted
    /// detail, the key), and Move to folder's submenu beside it (to the left when the window
    /// ends to its right: the panel is at the window's right edge).
    fn render_menu(&self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(m) = &self.menu else { return vec![] };
        let entries = self.menu_entries(&m.on);
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut sub = None;
        for (i, e) in entries.iter().enumerate() {
            match e {
                MenuEntry::Sep => rows.push(ui::menu_sep(t).into_any_element()),
                MenuEntry::Item { act, icon: ic, label, detail, key } => {
                    let act = act.clone();
                    let row = crate::grid_menu::row(t, ("fav-menu-item", i), *ic, label.clone(), detail.clone(), key.map(SharedString::from), m.hi == Some(i))
                        .on_hover(cx.listener(move |h, on: &bool, _, cx| {
                            if let (true, Some(m)) = (*on, h.menu.as_mut())
                                && (m.hi != Some(i) || m.sub.is_some())
                            {
                                m.hi = Some(i);
                                m.sub = None;
                                m.sub_hi = None;
                                cx.notify();
                            }
                        }))
                        .on_click(cx.listener(move |h, _: &gpui::ClickEvent, window, cx| h.run_menu(act.clone(), window, cx)));
                    rows.push(row.into_any_element());
                }
                MenuEntry::Sub { icon: ic, label, items } => {
                    let open = m.sub == Some(i);
                    if open {
                        sub = Some((i, items));
                    }
                    let fg = if m.hi == Some(i) || open { t.on_acc } else { t.tx3 };
                    let row = crate::grid_menu::row(t, ("fav-menu-item", i), *ic, label.clone(), None, None, m.hi == Some(i) || open)
                        .child(icon(Icon::ChevronRight).size(px(10.)).text_color(fg))
                        .on_hover(cx.listener(move |h, on: &bool, _, cx| {
                            if let (true, Some(m)) = (*on, h.menu.as_mut())
                                && m.sub != Some(i)
                            {
                                m.hi = Some(i);
                                m.sub = Some(i);
                                m.sub_hi = None;
                                cx.notify();
                            }
                        }))
                        .on_click(cx.listener(move |h, _: &gpui::ClickEvent, _, cx| {
                            if let Some(m) = h.menu.as_mut() {
                                m.hi = Some(i);
                                m.sub = Some(i);
                                cx.notify();
                            }
                        }));
                    rows.push(row.into_any_element());
                }
            }
        }
        let sub_bounds = self.sub_bounds.clone();
        let main = ui::popover(t)
            .id("fav-menu")
            .w(px(MENU_W))
            .text_size(px(12.))
            .font_family(crate::theme::mono())
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(move |h, ev: &gpui::MouseDownEvent, _, cx| {
                // A click in the submenu is a click in the menu.
                if h.menu.as_ref().is_some_and(|m| m.sub.is_some()) && sub_bounds.get().is_some_and(|b| b.contains(&ev.position)) {
                    return;
                }
                h.menu = None;
                cx.notify();
            }))
            .children(rows);
        let bounds = self.menu_bounds.clone();
        let mut out = vec![
            gpui::deferred(
                gpui::anchored()
                    .position(m.at)
                    .snap_to_window_with_margin(px(8.))
                    .child(div().on_children_prepainted(move |b, _, _| bounds.set(b.first().copied())).child(main)),
            )
            .priority(3)
            .into_any_element(),
        ];
        if let (Some((i, items)), Some(mb)) = (sub, self.menu_bounds.get()) {
            let top = mb.top() + px(entries[..i].iter().map(MenuEntry::h).sum::<f32>());
            let right = mb.right() + px(SUB_W + 8.) <= window.viewport_size().width;
            let (at, corner) = if right { (point(mb.right() - px(2.), top), gpui::Anchor::TopLeft) } else { (point(mb.left() + px(2.), top), gpui::Anchor::TopRight) };
            let sub_rows: Vec<AnyElement> = items
                .iter()
                .enumerate()
                .map(|(j, (act, ic, label, here))| {
                    let hi = m.sub_hi == Some(j) && !here;
                    let row = crate::grid_menu::row(t, ("fav-submenu-item", j), *ic, label.clone(), None, None, hi)
                        .when(*here, |d| d.text_color(t.tx3).child(icon(Icon::Check).size(px(12.)).text_color(t.tx2)))
                        .on_hover(cx.listener(move |h, on: &bool, _, cx| {
                            if let (true, Some(m)) = (*on, h.menu.as_mut())
                                && m.sub_hi != Some(j)
                            {
                                m.sub_hi = Some(j);
                                cx.notify();
                            }
                        }));
                    if *here {
                        row.into_any_element()
                    } else {
                        let act = act.clone();
                        row.on_click(cx.listener(move |h, _: &gpui::ClickEvent, window, cx| h.run_menu(act.clone(), window, cx))).into_any_element()
                    }
                })
                .collect();
            let pop = ui::popover(t)
                .id("fav-submenu")
                .w(px(SUB_W))
                .text_size(px(12.))
                .font_family(crate::theme::mono())
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .children(sub_rows);
            let bounds = self.sub_bounds.clone();
            out.push(
                gpui::deferred(
                    gpui::anchored()
                        .position(at)
                        .anchor(corner)
                        .snap_to_window_with_margin(px(8.))
                        .child(div().on_children_prepainted(move |b, _, _| bounds.set(b.first().copied())).child(pop)),
                )
                .priority(4)
                .into_any_element(),
            );
        }
        out
    }

    // ---------------------------------------------------------------------------------------
    // The header's controls

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
                    h.open_menu(MenuOn::Panel, ev.position(), cx);
                    return;
                }
                h.show_favorites = !h.show_favorites;
                h.apply_filter(cx);
            }))
            .on_mouse_down(gpui::MouseButton::Right, cx.listener(|h, ev: &gpui::MouseDownEvent, _, cx| {
                h.open_menu(MenuOn::Panel, ev.position, cx);
                cx.stop_propagation();
            }));
        let bounds = self.fav_toggle_bounds.clone();
        div().flex().flex_row().on_children_prepainted(move |b, _, _| bounds.set(b.first().copied())).child(button)
    }

    /// "+ folder" (the favorites only): a new folder, named in place.
    fn render_new_folder(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (hov, prs) = (t.hov, t.prs);
        let button = div()
            .id("new-fav-folder")
            .flex_none()
            .h(px(24.))
            .px(px(5.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(1.))
            .hover(move |s| s.bg(hov))
            .active(move |s| s.bg(prs))
            .child(icon(Icon::Plus).size(px(10.)).text_color(t.tx3))
            .child(icon(Icon::Schema).size(px(14.)).text_color(t.tx3))
            .on_click(cx.listener(|h, _: &gpui::ClickEvent, window, cx| h.new_folder(window, cx)));
        let bounds = self.new_folder_bounds.clone();
        div().flex().flex_row().on_children_prepainted(move |b, _, _| bounds.set(b.first().copied())).child(button)
    }

    // ---------------------------------------------------------------------------------------
    // `history_check` (`--demo`): real mouse and key events need window-coordinate anchors and
    // a way to read state a click cannot report back on its own.

    pub(crate) fn demo_entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Whether the history listed holds `sql` (a favorite run from its menu landed there).
    pub(crate) fn demo_has_entry(&self, sql: &str) -> bool {
        self.entries.iter().any(|e| e.sql.trim().trim_end_matches(';') == sql.trim().trim_end_matches(';'))
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
        self.show_favorites = favorites;
        if favorites {
            let core = self.store.read(cx).core.clone();
            for (name, sql) in [("Orders by day", "SELECT date_trunc('day', p.created) AS day, count(*) FROM orders p GROUP BY 1 ORDER BY 1;"), ("Top customers", "SELECT c.name, count(*) FROM orders p JOIN customer c ON c.id = p.customer GROUP BY 1 ORDER BY 2 DESC LIMIT 10;")] {
                let mut f = dataman_core::Favorite::new(name, sql);
                f.connection_id = self.conn_id.clone();
                let _ = core.favorites.add(f);
            }
            self.read_favorites(cx);
        }
        self.apply_filter(cx);
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

    /// Each favorite's name and the name of the folder it is in.
    pub(crate) fn demo_filed(&self) -> Vec<(String, Option<String>)> {
        let folder = |id: &str| self.folders.iter().find(|d| d.id == id).map(|d| d.name.clone());
        self.favorites.iter().map(|f| (f.name.clone(), f.folder.as_deref().and_then(folder))).collect()
    }

    pub(crate) fn demo_folders(&self) -> Vec<(String, String)> {
        self.folders.iter().map(|d| (d.id.clone(), d.name.clone())).collect()
    }

    /// The listed rows, as `▸ name` / `▾ name` for a folder, `name` for a favorite (indented
    /// two spaces in a folder), `entry` for the history.
    pub(crate) fn demo_rows(&self) -> Vec<String> {
        self.items
            .iter()
            .map(|it| match it {
                Item::Day(d) => d.to_string(),
                Item::Entry(_) => "entry".into(),
                Item::Folder { ix, open } => format!("{} {}", if *open { "▾" } else { "▸" }, self.folders[*ix].name),
                Item::Fav(i) => format!("{}{}", if self.favorites[*i].folder.is_some() { "  " } else { "" }, self.favorites[*i].name),
            })
            .collect()
    }

    /// The listed item of the favorite or folder named `name`.
    pub(crate) fn demo_item_named(&self, name: &str) -> Option<usize> {
        self.items.iter().position(|it| match it {
            Item::Folder { ix, .. } => self.folders[*ix].name == name,
            Item::Fav(i) => self.favorites[*i].name == name,
            _ => false,
        })
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
        self.read_favorites(cx);
        self.show_favorites = true;
        self.apply_filter(cx);
    }

    /// Screenshots (`fav-menu`): the context menu of listed item `item_ix`, as a right-click in
    /// its row opens it.
    pub(crate) fn demo_open_fav_menu(&mut self, item_ix: usize, cx: &mut Context<Self>) {
        if let (Some(Item::Fav(i)), Some(p)) = (self.items.get(item_ix), self.demo_item_point(item_ix)) {
            self.selected = Some(item_ix);
            let i = *i;
            self.open_menu(MenuOn::Fav(i), point(p.x + px(60.), p.y), cx);
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
    pub(crate) fn demo_dropdown_bounds(&self) -> Option<Bounds<Pixels>> {
        self.chip_bounds.get()
    }

    pub(crate) fn demo_favorites_toggle_bounds(&self) -> Option<Bounds<Pixels>> {
        self.fav_toggle_bounds.get()
    }

    pub(crate) fn demo_new_folder_bounds(&self) -> Option<Bounds<Pixels>> {
        self.new_folder_bounds.get()
    }

    pub(crate) fn demo_fav_menu_open(&self) -> bool {
        self.menu.is_some()
    }

    /// The middle of the open menu's row labelled `label` (`sub`: the submenu's), as last
    /// painted.
    pub(crate) fn demo_menu_point(&self, label: &str, sub: bool) -> Option<Point<Pixels>> {
        let m = self.menu.as_ref()?;
        let entries = self.menu_entries(&m.on);
        let (b, top) = if sub {
            let MenuEntry::Sub { items, .. } = entries.get(m.sub?)? else { return None };
            let j = items.iter().position(|(_, _, l, _)| l.as_ref() == label)?;
            (self.sub_bounds.get()?, PAD + j as f32 * ITEM_H)
        } else {
            let i = entries.iter().position(|e| e.label() == Some(label))?;
            (self.menu_bounds.get()?, PAD + entries[..i].iter().map(MenuEntry::h).sum::<f32>())
        };
        Some(point(b.left() + px(30.), b.top() + px(top + ITEM_H / 2.)))
    }

    /// Where a favorite being dragged would land: `"top"`, a folder's name, or `None`.
    pub(crate) fn demo_drop_at(&self) -> Option<String> {
        match self.drop_at.as_ref()? {
            DropAt::Top => Some("top".into()),
            DropAt::Folder(id) => self.folders.iter().find(|d| d.id == *id).map(|d| d.name.clone()),
        }
    }

    /// The gap among the folders a folder being dragged would drop into, and the listed items
    /// drawing the line that marks it (with `true` for a line under the item).
    pub(crate) fn demo_folder_drop(&self) -> Option<(usize, Vec<(usize, bool)>)> {
        let slot = self.folder_drop?;
        Some((slot, (0..self.items.len()).filter_map(|ix| self.folder_line(ix).map(|after| (ix, after))).collect()))
    }

    pub(crate) fn demo_renaming_index(&self) -> Option<usize> {
        match &self.renaming {
            Some((Naming::Fav(i), _)) => Some(*i),
            _ => None,
        }
    }

    /// The folder (its name) being renamed in place.
    pub(crate) fn demo_renaming_folder(&self) -> Option<String> {
        match &self.renaming {
            Some((Naming::Folder(id), _)) => self.folders.iter().find(|d| d.id == *id).map(|d| d.name.clone()),
            _ => None,
        }
    }

    /// Whether the rename field itself (not the panel, not the row) actually holds the
    /// keyboard focus: real typing only reaches it when this is true.
    pub(crate) fn demo_renaming_focused(&self, window: &Window, cx: &App) -> bool {
        self.renaming.as_ref().is_some_and(|(_, input)| input.focus_handle(cx).is_focused(window))
    }

    /// A point inside the rename field, as last painted.
    pub(crate) fn demo_rename_field_point(&self, cx: &App) -> Option<Point<Pixels>> {
        let (x, y, w, h) = self.renaming.as_ref()?.1.read(cx).demo_bounds()?;
        Some(point(px(x + w * 0.5), px(y + h * 0.5)))
    }

    /// A point inside item `item_ix` of the visible list (a day header, a history entry, a
    /// folder or a favorite), in window coordinates; `None` before it is painted.
    pub(crate) fn demo_item_point(&self, item_ix: usize) -> Option<Point<Pixels>> {
        let b = self.list.bounds_for_item(item_ix)?;
        // Past a nested favorite's indent.
        Some(point(b.origin.x + px(40.), b.origin.y + b.size.height * 0.5))
    }

    /// The list's own bounds (the checks drop below its last row).
    pub(crate) fn demo_list_bounds(&self) -> Bounds<Pixels> {
        self.list.viewport_bounds()
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
        let favs = self.show_favorites;
        let total = if favs { self.favorites.len() } else { self.entries.len() };
        // A drag that ended outside the panel leaves no drop behind.
        if !cx.has_active_drag() {
            self.drop_at = None;
            self.dragging = None;
            self.folder_drop = None;
            self.dragging_folder = None;
        }
        let top_drop = self.drop_at == Some(DropAt::Top);
        let body: AnyElement = if self.items.is_empty() {
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
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .flex()
                .flex_col()
                // A favorite dragged over the list: the folder it would drop into (its row, or
                // a favorite in it), else the top level.
                .on_drag_move(cx.listener(|h, ev: &gpui::DragMoveEvent<DraggedFav>, _, cx| {
                    let pos = ev.event.position;
                    let id = ev.drag(cx).0.clone();
                    let at = if ev.bounds.contains(&pos) { h.drop_target(&id, pos) } else { None };
                    if at != h.drop_at || h.dragging.as_ref() != Some(&id) {
                        h.drop_at = at;
                        h.dragging = Some(id);
                        cx.notify();
                    }
                }))
                .on_drop(cx.listener(|h, d: &DraggedFav, _, cx| h.drop_favorite(&d.0, cx)))
                // A folder dragged over the list: the gap among the folders it would drop into.
                .on_drag_move(cx.listener(|h, ev: &gpui::DragMoveEvent<DraggedFolder>, _, cx| {
                    let pos = ev.event.position;
                    let id = ev.drag(cx).0.clone();
                    let slot = if ev.bounds.contains(&pos) && h.can_reorder(cx) { h.folder_slot(&id, pos) } else { None };
                    if slot != h.folder_drop || h.dragging_folder.as_ref() != Some(&id) {
                        h.folder_drop = slot;
                        h.dragging_folder = Some(id);
                        cx.notify();
                    }
                }))
                .on_drop(cx.listener(|h, d: &DraggedFolder, _, cx| h.drop_folder(&d.0, cx)))
                .child(list(self.list.clone(), cx.processor(move |h, ix: usize, _window, cx| h.render_item(ix, &theme, focused, cx))).flex_1())
                // Out of its folder: the whole list is the drop target.
                .when(top_drop, |d| d.child(div().absolute().inset_0().mx(px(6.)).border_1().border_color(t.acc.opacity(0.45))))
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
            .on_action(cx.listener(|h, _: &Collapse, _, cx| h.fold(false, cx)))
            .on_action(cx.listener(|h, _: &Expand, _, cx| h.fold(true, cx)))
            .on_action(cx.listener(|h, _: &Insert, _, cx| h.emit_selected(HistoryEvent::Insert, cx)))
            .on_action(cx.listener(|h, _: &OpenInTab, _, cx| h.open_selected_in_tab(cx)))
            .on_action(cx.listener(|h, _: &RunEntry, _, cx| h.run_selected(cx)))
            .on_action(cx.listener(|h, _: &Close, _, cx| {
                // esc while renaming cancels the rename (the old name stays), with a menu open
                // it shuts the menu; otherwise it closes the panel, as usual.
                if h.renaming.take().is_some() || h.menu.take().is_some() {
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
                    .group("history-head")
                    .child(ui::caps(&t, if favs { "FAVORITES" } else { "HISTORY" }))
                    .children(color.filter(|_| !favs).map(|c| ui::chip(c, 8.)))
                    .child(div().flex_1().text_color(t.tx).overflow_hidden().whitespace_nowrap().text_ellipsis().when(!favs, |d| d.child(name)))
                    .when(favs, |d| d.child(self.render_new_folder(&t, cx)))
                    // Stacked with the explorer it shows and hides with the explorer: no ×.
                    .when(!self.shares_dock, |d| {
                        d.child(ui::icon_button(&t, "close-history", Icon::Close, 24., 13.).on_click(cx.listener(|_, _, _, cx| cx.emit(HistoryEvent::Close))))
                    }),
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
            .children(self.render_menu(&t, window, cx))
    }
}
