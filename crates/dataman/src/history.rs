use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use chrono::{Datelike, Local};
use gpui::{
    AnyElement, App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, ListAlignment, ListState,
    Pixels, Point, SharedString, Window, actions, div, list, point, prelude::*,
};
use crate::theme::px;
use dataman_core::favorites::{Folder, RemovedFolder};
use dataman_core::{Favorite, HistoryEntry, Outcome};

use crate::assets::{Icon, icon};
use crate::conns::ConnStore;
use crate::grid::ui_count;
use crate::input::{InputEvent, TextInput};
use crate::theme::{ActiveTheme, Theme};
use crate::tree_menu::{ITEM_H, PAD, SEP_H};
use crate::ui;
use crate::undo::{Entry, Op};

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
    Insert(String),
    OpenInTab(String, String, String),
    Run(String, String, String),
    OpenFavorite(Favorite, bool),
    ImportFavorites,
    FavoritesChanged,
    Did(Entry),
    Close,
}

const HISTORY_MAX: usize = 2000;
const NAME_MAX: usize = 40;
const SQL_MAX: usize = 90;

fn excerpt(flat: &str, q: &str, max: usize) -> String {
    const LEAD: usize = 12;
    match ui::find_ci(flat, q).map(|h| flat[..h.start].chars().count()) {
        Some(at) if at > LEAD + 8 => clip(&format!("…{}", flat.chars().skip(at - LEAD).collect::<String>()), max),
        _ => clip(flat, max),
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() > max { format!("{}…", s.chars().take(max).collect::<String>().trim_end()) } else { s.to_string() }
}

pub(crate) fn fav_name(sql: &str) -> String {
    let flat: String = sql.trim_end_matches(';').split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 60 { format!("{}…", flat.chars().take(60).collect::<String>()) } else { flat }
}

const FAV_ROW_H: f32 = 56.;

const UNFOLD_STAGGERED: usize = 8;

const NEST: f32 = 15.;
const MENU_W: f32 = 220.;
const SUB_W: f32 = 200.;
const DELETE_ASK: Duration = Duration::from_secs(3);

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
    Folder { ix: usize, open: bool },
    Fav(usize),
}

pub(crate) struct DraggedFav(pub(crate) String);

pub(crate) struct DraggedFolder(pub(crate) String);

#[derive(Clone, Debug, PartialEq, Eq)]
enum DropAt {
    Folder(String),
    Top,
}

#[derive(Clone, PartialEq)]
enum Naming {
    Fav(usize),
    Folder(String),
}

#[derive(Clone, PartialEq)]
enum MenuOn {
    Entry(usize),
    Fav(usize),
    Folder(String),
    Panel,
}

#[derive(Clone, Debug, PartialEq)]
enum Act {
    Run,
    Open,
    Rename,
    Move(Option<String>),
    Remove,
    NewFolder,
    DeleteFolder,
    Import,
    ToggleFavorite,
    RemoveEntry,
}

enum MenuEntry {
    Item { act: Act, icon: Icon, label: SharedString, detail: Option<SharedString>, key: Option<&'static str> },
    Sep,
    Sub { icon: Icon, label: SharedString, items: Vec<(Act, Icon, SharedString, bool)> },
}

impl MenuEntry {
    fn item(act: Act, icon: Icon, label: &'static str, key: Option<&'static str>) -> MenuEntry {
        MenuEntry::Item { act, icon, label: label.into(), detail: None, key }
    }

    fn h(&self) -> f32 {
        if matches!(self, MenuEntry::Sep) { SEP_H } else { ITEM_H }
    }

    #[cfg(feature = "checks")]
    fn label(&self) -> Option<&str> {
        match self {
            MenuEntry::Item { label, .. } | MenuEntry::Sub { label, .. } => Some(label),
            MenuEntry::Sep => None,
        }
    }
}

struct Menu {
    on: MenuOn,
    at: Point<Pixels>,
    hi: Option<usize>,
    sub: Option<usize>,
    sub_hi: Option<usize>,
    ask: Option<DeleteAsk>,
}

struct DeleteAsk {
    _timeout: gpui::Task<()>,
    _esc: gpui::Subscription,
}

type BoundsCell = Rc<Cell<Option<Bounds<Pixels>>>>;

pub struct HistoryPanel {
    store: Entity<ConnStore>,
    focus: FocusHandle,
    entries: Vec<(String, HistoryEntry)>,
    favorites: Vec<Favorite>,
    folders: Vec<Folder>,
    open_folders: HashSet<String>,
    folding: Option<(String, std::time::Instant, bool)>,
    shut_task: Option<gpui::Task<()>>,
    unfold_h: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, f32>>>,
    items: Vec<Item>,
    shown: usize,
    selected: Option<usize>,
    chip: Chip,
    show_favorites: bool,
    other_filter: String,
    chip_menu: bool,
    chip_bounds: BoundsCell,
    new_folder_bounds: BoundsCell,
    clear_ask: Option<gpui::Task<()>>,
    clear_bounds: BoundsCell,
    menu: Option<Menu>,
    menu_bounds: BoundsCell,
    sub_bounds: BoundsCell,
    renaming: Option<(Naming, Entity<TextInput>)>,
    dragging: Option<String>,
    drop_at: Option<DropAt>,
    dragging_folder: Option<String>,
    folder_drop: Option<usize>,
    filter: Entity<TextInput>,
    query: String,
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
        let filter = cx.new(|cx| TextInput::new(cx, "Search").bare());
        cx.subscribe(&filter, |h: &mut HistoryPanel, _, ev: &InputEvent, cx| match ev {
            InputEvent::Changed => h.apply_filter(cx),
            InputEvent::Submit => h.emit_selected(HistoryEvent::Insert, cx),
        })
        .detach();
        HistoryPanel {
            store,
            focus: cx.focus_handle(),
            entries: vec![],
            favorites: vec![],
            folders: vec![],
            open_folders: HashSet::new(),
            folding: None,
            shut_task: None,
            unfold_h: Default::default(),
            items: vec![],
            shown: 0,
            selected: None,
            chip: Chip::All,
            show_favorites: true,
            other_filter: String::new(),
            chip_menu: false,
            chip_bounds: Default::default(),
            new_folder_bounds: Default::default(),
            clear_ask: None,
            clear_bounds: Default::default(),
            menu: None,
            menu_bounds: Default::default(),
            sub_bounds: Default::default(),
            renaming: None,
            dragging: None,
            drop_at: None,
            dragging_folder: None,
            folder_drop: None,
            filter,
            query: String::new(),
            list: ListState::new(0, ListAlignment::Top, px(200.)),
        }
    }

    pub fn set_favorites(&mut self, on: bool, cx: &mut Context<Self>) {
        if on == self.show_favorites {
            return;
        }
        self.show_favorites = on;
        self.chip_menu = false;
        self.menu = None;
        self.renaming = None;
        let text = std::mem::take(&mut self.other_filter);
        self.other_filter = self.filter.read(cx).text().to_string();
        self.filter.update(cx, |f, cx| f.set_text(text, cx));
        self.apply_filter(cx);
    }

    pub fn showing_favorites(&self) -> bool {
        self.show_favorites
    }

    pub fn set_filter(&mut self, text: &str, cx: &mut Context<Self>) {
        self.filter.update(cx, |f, cx| f.set_text(text, cx));
        self.apply_filter(cx);
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let ids: Vec<String> = store.profiles.iter().map(|p| p.id.clone()).collect();
        self.entries = store.core.history.load_all(&ids, HISTORY_MAX).unwrap_or_default();
        self.read_favorites(cx);
        self.apply_filter(cx);
    }

    pub(crate) fn reload_favorites(&mut self, cx: &mut Context<Self>) {
        self.read_favorites(cx);
        self.apply_filter(cx);
    }

    fn read_favorites(&mut self, cx: &App) {
        let core = &self.store.read(cx).core;
        self.favorites = core.favorites.list();
        self.folders = core.favorites.folders();
    }

    fn is_favorited(&self, sql: &str) -> bool {
        self.favorites.iter().any(|f| f.sql.trim() == sql.trim())
    }

    fn toggle_favorite(&mut self, entry_ix: usize, cx: &mut Context<Self>) {
        let (conn, e) = &self.entries[entry_ix];
        let (sql, database, conn_id) = (e.sql.clone(), e.database.clone(), Some(conn.clone()));
        let store = self.store.read(cx).core.favorites.clone();
        if let Some(existing) = store.find_by_sql(&sql) {
            if let Ok(Some((ix, fav))) = store.take(&existing.id) {
                cx.emit(HistoryEvent::Did(Entry::new(format!("Remove favorite '{}'", fav.name), Op::Favorite { fav, ix, added: false })));
            }
        } else {
            let mut fav = Favorite::new(fav_name(&sql), sql);
            fav.connection_id = conn_id;
            fav.database = Some(database);
            if let Ok(fav) = store.add(fav) {
                let ix = store.list().iter().position(|f| f.id == fav.id).unwrap_or(0);
                cx.emit(HistoryEvent::Did(Entry::new(format!("Add favorite '{}'", fav.name), Op::Favorite { fav, ix, added: true })));
            }
        }
        self.reload_favorites(cx);
        cx.emit(HistoryEvent::FavoritesChanged);
    }

    fn remove_entry(&mut self, entry_ix: usize, cx: &mut Context<Self>) {
        let Some((conn, e)) = self.entries.get(entry_ix).cloned() else { return };
        if let Ok(Some(line)) = self.store.read(cx).core.history.remove(&conn, &e) {
            let op = Op::History { entry: Some((conn.clone(), e)), taken: vec![(conn, line)] };
            cx.emit(HistoryEvent::Did(Entry::new("Remove from history", op)));
        }
        let keep = self.selected;
        self.reload(cx);
        self.selected = keep.filter(|&s| s < self.items.len() && !matches!(self.items[s], Item::Day(_))).or(self.selected);
        cx.notify();
    }

    fn clear_clicked(&mut self, cx: &mut Context<Self>) {
        if self.clear_ask.take().is_none() {
            self.clear_ask = Some(cx.spawn(async move |h, cx| {
                cx.background_executor().timer(DELETE_ASK).await;
                let _ = h.update(cx, |h, cx| {
                    h.clear_ask = None;
                    cx.notify();
                });
            }));
            return cx.notify();
        }
        let store = self.store.read(cx);
        let mut taken = Vec::new();
        for p in &store.profiles {
            if let Ok(Some(bytes)) = store.core.history.take(&p.id) {
                taken.push((p.id.clone(), bytes));
            }
        }
        if !taken.is_empty() {
            cx.emit(HistoryEvent::Did(Entry::new("Clear history", Op::History { entry: None, taken })));
        }
        self.reload(cx);
    }

    fn remove_favorite(&mut self, fav_ix: usize, cx: &mut Context<Self>) {
        let id = self.favorites[fav_ix].id.clone();
        if let Ok(Some((ix, fav))) = self.store.read(cx).core.favorites.take(&id) {
            cx.emit(HistoryEvent::Did(Entry::new(format!("Remove favorite '{}'", fav.name), Op::Favorite { fav, ix, added: false })));
        }
        self.refresh_keeping(cx);
        cx.emit(HistoryEvent::FavoritesChanged);
    }

    pub fn focus_favorite(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.read_favorites(cx);
        self.set_favorites(true, cx);
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

    fn fav_item(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|it| matches!(it, Item::Fav(i) if self.favorites[*i].id == id))
    }

    fn folder_item(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|it| matches!(it, Item::Folder { ix, .. } if self.folders[*ix].id == id))
    }

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
                    if let Some(f) = self.favorites.get(*i)
                        && f.name != name
                        && let Ok(Some(_)) = favorites.rename(&f.id, &name)
                    {
                        let op = Op::FavoriteName { id: f.id.clone(), from: f.name.clone(), to: name.clone() };
                        cx.emit(HistoryEvent::Did(Entry::new(format!("Rename favorite '{}'", f.name), op)));
                    }
                }
                Naming::Folder(id) => {
                    let from = self.folders.iter().find(|d| d.id == *id).map(|d| d.name.clone());
                    if let Some(from) = from.filter(|f| *f != name)
                        && let Ok(Some(_)) = favorites.rename_folder(id, &name)
                    {
                        let op = Op::FolderName { id: id.clone(), from: from.clone(), to: name.clone() };
                        cx.emit(HistoryEvent::Did(Entry::new(format!("Rename folder '{from}'"), op)));
                    }
                }
            }
        }
        self.refresh_keeping(cx);
        if let Naming::Fav(_) = naming {
            cx.emit(HistoryEvent::FavoritesChanged);
        }
    }

    fn new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(d) = self.store.read(cx).core.favorites.add_folder("New folder") else { return };
        let index = self.store.read(cx).core.favorites.folders().iter().position(|f| f.id == d.id).unwrap_or(0);
        let removed = RemovedFolder { index, folder: d.clone(), favorites: Vec::new() };
        cx.emit(HistoryEvent::Did(Entry::new(format!("New folder '{}'", d.name), Op::Folder { removed, added: true })));
        self.set_favorites(true, cx);
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

    fn toggle_folder(&mut self, id: &str, cx: &mut Context<Self>) {
        let now = std::time::Instant::now();
        let animate = !cx.reduce_motion();
        if self.shutting(id) {
            self.shut_task = None;
            self.folding = Some((id.to_string(), now, true));
            return cx.notify();
        }
        if !self.open_folders.contains(id) {
            self.open_folders.insert(id.to_string());
            if animate {
                self.folding = Some((id.to_string(), now, true));
                self.unfold_h.borrow_mut().clear();
            }
            return self.refresh_keeping(cx);
        }
        if let Some(Item::Fav(i)) = self.selected.and_then(|s| self.items.get(s))
            && self.favorites[*i].folder.as_deref() == Some(id)
        {
            self.selected = self.folder_item(id);
        }
        if !animate {
            return self.shut_now(id, cx);
        }
        self.folding = Some((id.to_string(), now, false));
        let wait = self.fold_time(id, false);
        let id = id.to_string();
        self.shut_task = Some(cx.spawn(async move |h, cx| {
            cx.background_executor().timer(wait).await;
            let _ = h.update(cx, |h, cx| h.shut_now(&id, cx));
        }));
        cx.notify();
    }

    fn shut_now(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.shutting(id) {
            self.folding = None;
        }
        self.shut_task = None;
        self.open_folders.remove(id);
        self.refresh_keeping(cx);
    }

    fn shutting(&self, id: &str) -> bool {
        self.folding.as_ref().is_some_and(|(f, _, opening)| f == id && !opening)
    }

    fn delete_folder(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Ok(Some(removed)) = self.store.read(cx).core.favorites.remove_folder(id) {
            let label = format!("Delete folder '{}'", removed.folder.name);
            cx.emit(HistoryEvent::Did(Entry::new(label, Op::Folder { removed, added: false })));
            cx.emit(HistoryEvent::FavoritesChanged);
        }
        self.open_folders.remove(id);
        self.refresh_keeping(cx);
    }

    fn ask_delete(&mut self, cx: &mut Context<Self>) -> DeleteAsk {
        let timeout = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DELETE_ASK).await;
            let _ = this.update(cx, |h, cx| h.let_go_delete(cx));
        });
        // Esc lets it go and does nothing else (the menu stays): an interceptor runs before
        // any binding, wherever the keys are.
        let panel = cx.entity().downgrade();
        let esc = cx.intercept_keystrokes(move |ev, _, cx| {
            if ev.keystroke.key == "escape" {
                cx.stop_propagation();
                let _ = panel.update(cx, |h, cx| h.let_go_delete(cx));
            }
        });
        DeleteAsk { _timeout: timeout, _esc: esc }
    }

    fn let_go_delete(&mut self, cx: &mut Context<Self>) {
        if self.menu.as_mut().and_then(|m| m.ask.take()).is_some() {
            cx.notify();
        }
    }

    fn move_favorite(&mut self, id: &str, to: Option<String>, cx: &mut Context<Self>) {
        let from = self.favorites.iter().find(|f| f.id == id).map(|f| (f.name.clone(), f.folder.clone()));
        if let (Some((name, from)), Ok(Some(_))) = (from, self.store.read(cx).core.favorites.move_to_folder(id, to.as_deref())) {
            let place = match to.as_ref().and_then(|d| self.folders.iter().find(|f| f.id == *d)) {
                Some(d) => format!("'{}'", d.name),
                None => "the top level".into(),
            };
            let op = Op::FavoriteFolder { id: id.to_string(), from, to: to.clone() };
            cx.emit(HistoryEvent::Did(Entry::new(format!("Move '{name}' to {place}"), op)));
        }
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

    fn can_reorder(&self, cx: &App) -> bool {
        self.show_favorites && self.filter.read(cx).text().is_empty()
    }

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

    fn drop_folder(&mut self, id: &str, cx: &mut Context<Self>) {
        self.dragging_folder = None;
        let Some(slot) = self.folder_drop.take() else { return cx.notify() };
        let mut ids: Vec<String> = self.folders.iter().map(|d| d.id.clone()).collect();
        let before = ids.clone();
        let Some(from) = ids.iter().position(|d| d == id) else { return };
        if dataman_core::order::move_to(&mut ids, from, slot).is_some() && self.store.read(cx).core.favorites.reorder_folders(&ids).is_ok() {
            let name = self.folders[from].name.clone();
            cx.emit(HistoryEvent::Did(Entry::new(format!("Move folder '{name}'"), Op::FolderOrder { from: before, to: ids })));
        }
        self.refresh_keeping(cx);
    }

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

    fn matches(&self, (conn, e): &(String, HistoryEntry), q: &str, names: &std::collections::HashMap<String, String>) -> bool {
        let chip = match self.chip {
            Chip::All => true,
            Chip::Ok => e.outcome == Outcome::Ok,
            Chip::Error => e.outcome == Outcome::Error,
            Chip::Cancelled => e.outcome == Outcome::Cancelled,
        };
        chip && (q.is_empty() || e.sql.to_lowercase().contains(q) || names.get(conn).is_some_and(|n| n.contains(q)) || e.database.to_lowercase().contains(q))
    }

    fn apply_filter(&mut self, cx: &mut Context<Self>) {
        let q = self.filter.read(cx).text().trim().to_lowercase();
        self.query = q.clone();
        let mut items = Vec::new();
        let mut shown = 0;
        if self.show_favorites {
            let hit = |f: &Favorite| q.is_empty() || f.name.to_lowercase().contains(&q) || f.sql.to_lowercase().contains(&q);
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
            let names = self.store.read(cx).profiles.iter().map(|p| (p.id.clone(), p.name.to_lowercase())).collect();
            for (i, e) in self.entries.iter().enumerate() {
                if !self.matches(e, &q, &names) {
                    continue;
                }
                let d = e.1.ts.with_timezone(&Local).date_naive();
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

    fn refresh_keeping(&mut self, cx: &mut Context<Self>) {
        let was = self.selected;
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

    fn fold(&mut self, open: bool, cx: &mut Context<Self>) {
        match self.selected.and_then(|s| self.items.get(s)) {
            Some(&Item::Folder { ix, open: is_open, .. }) if (is_open && !self.shutting(&self.folders[ix].id)) != open => {
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
            Some(Item::Entry(i)) => Some(self.entries[*i].1.sql.clone()),
            Some(Item::Fav(i)) => Some(self.favorites[*i].sql.clone()),
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

    fn open_selected_in_tab(&mut self, cx: &mut Context<Self>) {
        match self.selected.and_then(|s| self.items.get(s)) {
            Some(Item::Entry(i)) => {
                let (conn, e) = &self.entries[*i];
                cx.emit(HistoryEvent::OpenInTab(e.sql.clone(), conn.clone(), e.database.clone()));
            }
            Some(Item::Fav(i)) => cx.emit(HistoryEvent::OpenFavorite(self.favorites[*i].clone(), false)),
            _ => {}
        }
    }

    fn run_selected(&mut self, cx: &mut Context<Self>) {
        match self.selected.and_then(|s| self.items.get(s)) {
            Some(Item::Entry(i)) => {
                let (conn, e) = &self.entries[*i];
                cx.emit(HistoryEvent::Run(e.sql.clone(), conn.clone(), e.database.clone()));
            }
            Some(Item::Fav(i)) => cx.emit(HistoryEvent::OpenFavorite(self.favorites[*i].clone(), true)),
            _ => {}
        }
    }

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
            Item::Fav(i) => {
                let row = self.render_fav(ix, *i, t, focused, cx);
                self.unfolding(ix, *i, row)
            }
        }
    }

    fn fold_time(&self, id: &str, opening: bool) -> std::time::Duration {
        let rows = self.favorites.iter().filter(|f| f.folder.as_deref() == Some(id)).count().saturating_sub(1).min(UNFOLD_STAGGERED) as u32;
        if opening {
            crate::motion::FLOOD.max(crate::motion::FOLDER_STAGGER * rows + crate::motion::FOLDER_ROW)
        } else {
            crate::motion::FLOOD.max(crate::motion::FOLDER_SHUT_STAGGER * rows + crate::motion::FOLDER_SHUT)
        }
    }

    fn folding_moves(&self) -> bool {
        self.folding.as_ref().is_some_and(|(id, at, opening)| at.elapsed() < self.fold_time(id, *opening))
    }

    fn unfolding(&self, ix: usize, fav_i: usize, row: AnyElement) -> AnyElement {
        let f = &self.favorites[fav_i];
        let Some((_, at, opening)) = self.folding.as_ref().filter(|(d, ..)| f.folder.as_deref() == Some(d.as_str())) else { return row };
        let Some(first) = self.items[..ix].iter().rposition(|it| matches!(it, Item::Folder { .. })) else { return row };
        let ms = at.elapsed().as_secs_f32() * 1e3;
        let nth = ix - first - 1;
        let k = if *opening {
            let delay = nth.min(UNFOLD_STAGGERED) as f32 * crate::motion::FOLDER_STAGGER.as_secs_f32() * 1e3;
            let raw = (ms - delay) / (crate::motion::FOLDER_ROW.as_secs_f32() * 1e3);
            if raw >= 1. {
                return row;
            }
            crate::motion::EASE.eval(raw.max(0.))
        } else {
            let n = self.items[first + 1..].iter().take_while(|it| matches!(it, Item::Fav(i) if self.favorites[*i].folder == f.folder)).count();
            let from_end = (n - 1 - nth).min(UNFOLD_STAGGERED) as f32;
            let delay = from_end * crate::motion::FOLDER_SHUT_STAGGER.as_secs_f32() * 1e3;
            let raw = (ms - delay) / (crate::motion::FOLDER_SHUT.as_secs_f32() * 1e3);
            1. - crate::motion::EASE.eval(raw.clamp(0., 1.))
        };
        let heights = self.unfold_h.clone();
        let id = f.id.clone();
        let h = heights.borrow().get(&id).copied().unwrap_or(if *opening { 0. } else { FAV_ROW_H });
        div()
            .w_full()
            .flex()
            .flex_col()
            .h(px(h * k))
            .overflow_hidden()
            .opacity(k)
            .on_children_prepainted(move |b, _, _| {
                if let Some(b) = b.first() {
                    heights.borrow_mut().insert(id.clone(), crate::theme::Pt::pt(b.size.height));
                }
            })
            .child(div().w_full().flex_none().child(row))
            .into_any_element()
    }

    fn render_entry(&self, ix: usize, entry_i: usize, t: &Theme, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let (conn, e) = &self.entries[entry_i];
        let sel = self.selected == Some(ix);
        let profile = self.store.read(cx).profile(conn);
        let (conn_name, chip) = (profile.map(|p| p.name.clone()), profile.map(|p| t.profile_color(p)));
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
        let body: Vec<&str> = e.sql.lines().skip_while(|l| l.trim().is_empty() || l.trim_start().starts_with("--")).collect();
        let src = if body.is_empty() { e.sql.clone() } else { body.join("\n") };
        let flat = excerpt(&src.split_whitespace().collect::<Vec<_>>().join(" "), &self.query, SQL_MAX);
        let (verb, rest) = match flat.split_once(' ') {
            Some((v, r)) if !v.starts_with('…') && ui::find_ci(&flat, &self.query).is_none_or(|h| h.start >= v.len()) => (Some(v.to_string()), format!(" {r}")),
            _ => (None, flat.clone()),
        };
        let time = e.ts.with_timezone(&Local).format("%H:%M:%S").to_string();
        let hov = t.hov;
        let q = self.query.clone();
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
                    .when(sel && focused, |d| d.bg(t.mark).border_1().border_color(t.edge))
                    .when(sel && !focused, |d| d.bg(t.prs))
                    .when(!sel, |d| d.hover(move |s| s.bg(hov)))
                    .on_click(cx.listener(move |h, ev: &gpui::ClickEvent, window, cx| {
                        if ev.modifiers().control {
                            return;
                        }
                        h.selected = Some(ix);
                        window.focus(&h.focus, cx);
                        if ev.click_count() >= 2 {
                            h.open_selected_in_tab(cx);
                        }
                        cx.notify();
                    }))
                    .on_mouse_down(gpui::MouseButton::Right, cx.listener(move |h, ev: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        h.selected = Some(ix);
                        window.focus(&h.focus, cx);
                        h.open_menu(MenuOn::Entry(entry_i), ev.position, cx);
                    }))
                    .on_mouse_down(gpui::MouseButton::Left, cx.listener(move |h, ev: &gpui::MouseDownEvent, window, cx| {
                        if ev.modifiers.control {
                            cx.stop_propagation();
                            h.selected = Some(ix);
                            window.focus(&h.focus, cx);
                            h.open_menu(MenuOn::Entry(entry_i), ev.position, cx);
                        }
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .text_size(px(11.))
                            .child(div().w(px(12.)).flex_none().text_center().text_color(gcolor).child(glyph))
                            .child(div().flex_none().text_color(t.tx).child(time))
                            .child(div().flex_none().text_color(t.tx3).child(fmt_ms(e.duration_ms)))
                            .when(e.source.as_deref() == Some(dataman_core::history::AGENT), |d| {
                                d.child(icon(Icon::Agent).size(px(11.)).text_color(t.tx3))
                            })
                            .child(div().flex_1())
                            .child(div().flex_none().text_color(rcolor).child(result)),
                    )
                    .child(
                        div()
                            .pl(px(20.))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.))
                            .text_color(t.tx3)
                            .children(chip.map(|c| ui::chip(c, 7.)))
                            .child(ui::marked(t, clip(&format!("{} · {}", conn_name.unwrap_or_else(|| conn.clone()), e.database), NAME_MAX), &self.query)),
                    )
                    .child(
                        div()
                            .pl(px(20.))
                            .min_w_0()
                            .flex()
                            .flex_row()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(t.tx2)
                            .children(verb.map(|v| div().flex_none().text_color(t.kw).child(v)))
                            .child(ui::marked(t, rest, &q)),
                    ),
            )
            .into_any_element()
    }

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

    fn render_folder(&self, ix: usize, folder_ix: usize, open: bool, t: &Theme, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let d = &self.folders[folder_ix];
        let id = d.id.clone();
        let open = open && !self.shutting(&id);
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
            None => div().flex_1().min_w_0().flex().flex_row().text_color(t.tx).child(ui::marked(t, d.name.clone(), &self.query)).into_any_element(),
        };
        let hov = t.hov;
        let chevron = ui::disclosure(gpui::ElementId::Name(format!("fav-chev-{id}").into()), open, 10., t.tx4);
        let (click_id, menu_id, ctrl_id, ghost, id_for_icon) = (id.clone(), id.clone(), id.clone(), d.name.clone(), id.clone());
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
                        if is_renaming || ev.modifiers().control {
                            return;
                        }
                        h.selected = Some(ix);
                        window.focus(&h.focus, cx);
                        if ev.click_count() >= 2 {
                            return cx.notify();
                        }
                        h.toggle_folder(&click_id, cx);
                    }))
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
                    .child(self.folder_icon(&id_for_icon, open, dropping, t))
                    .child(name)
                    .children(line.map(|after| crate::dnd::drop_line(t, false, after))),
            )
            .into_any_element()
    }

    fn folder_icon(&self, id: &str, open: bool, dropping: bool, t: &Theme) -> AnyElement {
        let fill = self.folding.as_ref().filter(|(f, ..)| f == id).and_then(|(_, at, opening)| crate::motion::folder_fill(*at, *opening));
        ui::folder_icon(open, fill, 14., if dropping { t.acc_t } else { t.tx3 })
    }

    fn render_fav(&self, ix: usize, fav_i: usize, t: &Theme, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let f = &self.favorites[fav_i];
        let sel = self.selected == Some(ix);
        let profile = f.connection_id.as_ref().and_then(|id| self.store.read(cx).profile(id));
        let conn_name = profile.map(|p| p.name.clone());
        let chip = profile.map(|p| t.profile_color(p));
        let flat = excerpt(&f.sql.split_whitespace().collect::<Vec<_>>().join(" "), &self.query, SQL_MAX);
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
            None => ui::marked(t, clip(&f.name, NAME_MAX), &self.query).text_color(t.tx).into_any_element(),
        };
        let (fav_id, ghost) = (f.id.clone(), f.name.clone());
        let line = self.folder_line(ix).filter(|_| cx.has_active_drag());
        div()
            .relative()
            .pl(px(if nested { 6. + NEST } else { 6. }))
            .pr(px(6.))
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
                    // Not while its name is edited: a drag there selects text.
                    .when(!is_renaming, |d| d.on_drag(DraggedFav(fav_id), move |_, _, _, cx| crate::dnd::ghost(ghost.clone(), chip, cx)))
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
                            .child(div().flex_1().min_w_0().flex().flex_row().child(name))
                            .children(conn_name.map(|n| div().flex_none().text_color(t.tx3).child(clip(&n, 20))))
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
                            .min_w_0()
                            .flex()
                            .flex_row()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(t.tx2)
                            .child(ui::marked(t, flat, &self.query)),
                    ),
            )
            .children(line.map(|after| crate::dnd::drop_line(t, false, after).mx(px(6.))))
            .into_any_element()
    }

    fn open_menu(&mut self, on: MenuOn, at: Point<Pixels>, cx: &mut Context<Self>) {
        self.chip_menu = false;
        self.sub_bounds.set(None);
        self.menu = Some(Menu { on, at, hi: None, sub: None, sub_hi: None, ask: None });
        cx.notify();
    }

    fn menu_entries(&self, on: &MenuOn) -> Vec<MenuEntry> {
        use MenuEntry as E;
        let import = E::item(Act::Import, Icon::Import, "Import favorites…", None);
        let new_folder = E::item(Act::NewFolder, Icon::Plus, "New folder", None);
        match on {
            MenuOn::Entry(i) => {
                let Some((_, e)) = self.entries.get(*i) else { return vec![] };
                let saved = self.is_favorited(&e.sql);
                vec![
                    E::item(Act::Run, Icon::Run, "Run", Some("⌘↩")),
                    E::item(Act::Open, Icon::Tab, "Open in new tab", Some("⌥↩")),
                    E::Sep,
                    if saved { E::item(Act::ToggleFavorite, Icon::StarFill, "Remove from favorites", None) } else { E::item(Act::ToggleFavorite, Icon::Star, "Add to favorites", None) },
                    E::Sep,
                    E::item(Act::RemoveEntry, Icon::Trash, "Remove from history", None),
                ]
            }
            MenuOn::Fav(i) => {
                let Some(f) = self.favorites.get(*i) else { return vec![] };
                let mut v = vec![
                    E::item(Act::Run, Icon::Run, "Run", Some("⌘↩")),
                    E::item(Act::Open, Icon::Tab, "Open in new tab", Some("⌥↩")),
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
                let queries = if n == 1 { "1 query".to_string() } else { format!("{n} queries") };
                let asking = self.menu.as_ref().is_some_and(|m| m.ask.is_some());
                let delete = E::Item {
                    act: Act::DeleteFolder,
                    icon: Icon::Trash,
                    label: match (asking, n) {
                        (false, _) => "Delete folder".into(),
                        (true, 0) => "Sure?".into(),
                        (true, _) => format!("Sure? (deletes {queries})").into(),
                    },
                    detail: (n > 0 && !asking).then(|| format!("with {queries}").into()),
                    key: None,
                };
                vec![E::item(Act::Rename, Icon::Edit, "Rename", None), new_folder, E::Sep, delete]
            }
            MenuOn::Panel => vec![new_folder, E::Sep, import],
        }
    }

    fn run_menu(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut m) = self.menu.take() else { return };
        if let MenuOn::Entry(i) = m.on {
            match act {
                Act::Run => {
                    if let Some((conn, e)) = self.entries.get(i) {
                        cx.emit(HistoryEvent::Run(e.sql.clone(), conn.clone(), e.database.clone()));
                    }
                }
                Act::Open => {
                    if let Some((conn, e)) = self.entries.get(i) {
                        cx.emit(HistoryEvent::OpenInTab(e.sql.clone(), conn.clone(), e.database.clone()));
                    }
                }
                Act::ToggleFavorite => self.toggle_favorite(i, cx),
                Act::RemoveEntry => self.remove_entry(i, cx),
                _ => {}
            }
            return cx.notify();
        }
        let fav = match m.on {
            MenuOn::Fav(i) => self.favorites.get(i).cloned().map(|f| (i, f)),
            _ => None,
        };
        match (act, fav) {
            (Act::Run, Some((_, f))) => cx.emit(HistoryEvent::OpenFavorite(f, true)),
            (Act::Open, Some((_, f))) => cx.emit(HistoryEvent::OpenFavorite(f, false)),
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
                if let MenuOn::Folder(id) = &m.on {
                    if m.ask.is_some() {
                        let id = id.clone();
                        self.delete_folder(&id, cx);
                    } else {
                        m.ask = Some(self.ask_delete(cx));
                        self.menu = Some(m);
                    }
                }
            }
            (Act::Import, _) => cx.emit(HistoryEvent::ImportFavorites),
            _ => {}
        }
        cx.notify();
    }

    fn render_menu(&self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(m) = &self.menu else { return vec![] };
        let entries = self.menu_entries(&m.on);
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut sub = None;
        for (i, e) in entries.iter().enumerate() {
            match e {
                MenuEntry::Sep => rows.push(ui::menu_sep(t).into_any_element()),
                MenuEntry::Item { act, label, .. } if *act == Act::DeleteFolder && m.ask.is_some() => {
                    rows.push(self.render_delete_ask(i, &entries, label.clone(), t, cx).into_any_element());
                }
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
                    .child(ui::menu_in("fav-menu-in", div().on_children_prepainted(move |b, _, _| bounds.set(b.first().copied())).child(main))),
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
                        .child(ui::menu_in("fav-submenu-in", div().on_children_prepainted(move |b, _, _| bounds.set(b.first().copied())).child(pop))),
                )
                .priority(4)
                .into_any_element(),
            );
        }
        out
    }

    fn render_delete_ask(&self, i: usize, entries: &[MenuEntry], label: SharedString, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (hov, prs) = (t.err.opacity(0.85), t.err.opacity(0.7));
        let top = PAD + entries[..i].iter().map(MenuEntry::h).sum::<f32>();
        let menu = self.menu_bounds.clone();
        div()
            .id(("fav-menu-item", i))
            .h(px(ITEM_H))
            .px(px(10.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .whitespace_nowrap()
            .bg(t.err)
            .text_color(t.on_err)
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .cursor_pointer()
            .hover(move |s| s.bg(hov))
            .active(move |s| s.bg(prs))
            .child(icon(Icon::Trash).size(px(14.)).text_color(t.on_err))
            .child(label)
            .on_click(cx.listener(|h, _: &gpui::ClickEvent, window, cx| h.run_menu(Act::DeleteFolder, window, cx)))
            .on_hover(cx.listener(move |h, hovered: &bool, window, cx| {
                // GPUI reports a press held on it as the pointer leaving: only a pointer really
                // outside the row lets it go.
                let pos = window.mouse_position();
                let inside = menu.get().is_some_and(|b| b.contains(&pos) && pos.y >= b.top() + px(top) && pos.y < b.top() + px(top + ITEM_H));
                if !*hovered && !inside {
                    h.let_go_delete(cx);
                }
            }))
    }

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
            .child(ui::dropdown_arrow("history-chip-arrow", open, 8., t.tx3))
            .on_click(cx.listener(|h, _, _, cx| {
                h.chip_menu = !h.chip_menu;
                cx.notify();
            }));
        let menu = open.then(|| {
            let options = [(Chip::All, "All"), (Chip::Ok, "OK"), (Chip::Error, "Error"), (Chip::Cancelled, "Cancelled")];
            let rows: Vec<_> = options
                .into_iter()
                .map(|(c, lbl)| {
                    let on = self.chip == c;
                    ui::menu_item(t, lbl, lbl, on.then_some("✓"), true)
                        .on_click(cx.listener(move |h, _, _, cx| {
                            h.chip = c;
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
                        .child(ui::menu_in("chip-menu-in", ui::popover(t).w(px(140.)).on_mouse_down_out(cx.listener(|h, _, _, cx| {
                            h.chip_menu = false;
                            cx.notify();
                        })).children(rows))),
                )
                .priority(3),
            )
        });
        let bounds = self.chip_bounds.clone();
        div().flex().flex_row().relative().on_children_prepainted(move |b, _, _| bounds.set(b.first().copied())).child(button).children(menu)
    }

    fn render_clear(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let asking = self.clear_ask.is_some();
        let (hov, prs) = (t.hov, t.prs);
        let button = div()
            .id("history-clear")
            .h(px(24.))
            .min_w(px(24.))
            .px(px(if asking { 7. } else { 0. }))
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap(px(5.))
            .text_size(px(11.))
            .border_1()
            .border_color(if asking { t.err } else { t.line })
            .when(asking, |d| d.text_color(t.err))
            .hover(move |s| s.bg(hov))
            .active(move |s| s.bg(prs))
            .tooltip(ui::tooltip(if asking { "Click again to clear every connection's history" } else { "Clear history" }, None, None))
            .child(icon(Icon::Trash).size(px(12.)).text_color(if asking { t.err } else { t.tx3 }))
            .when(asking, |d| d.child("Clear all?"))
            .on_click(cx.listener(|h, _: &gpui::ClickEvent, _, cx| h.clear_clicked(cx)));
        let bounds = self.clear_bounds.clone();
        div().flex().flex_row().on_children_prepainted(move |b, _, _| bounds.set(b.first().copied())).child(button)
    }

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

    #[cfg(feature = "checks")]
    pub(crate) fn demo_folding(&self) -> bool {
        self.folding_moves()
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_entry_count(&self) -> usize {
        self.entries.len()
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_has_entry(&self, sql: &str) -> bool {
        self.entries.iter().map(|(_, e)| e).any(|e| e.sql.trim().trim_end_matches(';') == sql.trim().trim_end_matches(';'))
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_favorite_entry(&mut self, entry_ix: usize, cx: &mut Context<Self>) {
        self.toggle_favorite(entry_ix, cx);
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_marks(&mut self, favorites: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.set_favorites(favorites, cx);
        if favorites {
            let core = self.store.read(cx).core.clone();
            for (name, sql) in [("Orders by day", "SELECT date_trunc('day', p.created) AS day, count(*) FROM orders p GROUP BY 1 ORDER BY 1;"), ("Top customers", "SELECT c.name, count(*) FROM orders p JOIN customer c ON c.id = p.customer GROUP BY 1 ORDER BY 2 DESC LIMIT 10;")] {
                let mut f = dataman_core::Favorite::new(name, sql);
                f.connection_id = self.store.read(cx).profiles.first().map(|p| p.id.clone());
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

    #[cfg(feature = "checks")]
    pub(crate) fn demo_search(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.filter.update(cx, |f, cx| f.set_text(text, cx));
        self.apply_filter(cx);
        window.focus(&self.filter.focus_handle(cx), cx);
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_favorites(&self) -> Vec<(String, String)> {
        self.favorites.iter().map(|f| (f.id.clone(), f.name.clone())).collect()
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_filed(&self) -> Vec<(String, Option<String>)> {
        let folder = |id: &str| self.folders.iter().find(|d| d.id == id).map(|d| d.name.clone());
        self.favorites.iter().map(|f| (f.name.clone(), f.folder.as_deref().and_then(folder))).collect()
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_folders(&self) -> Vec<(String, String)> {
        self.folders.iter().map(|d| (d.id.clone(), d.name.clone())).collect()
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_rows(&self) -> Vec<String> {
        self.items
            .iter()
            .map(|it| match it {
                Item::Day(d) => d.to_string(),
                Item::Entry(_) => "entry".into(),
                Item::Folder { ix, open } => {
                    let open = *open && !self.shutting(&self.folders[*ix].id);
                    format!("{} {}", if open { "▾" } else { "▸" }, self.folders[*ix].name)
                }
                Item::Fav(i) => format!("{}{}", if self.favorites[*i].folder.is_some() { "  " } else { "" }, self.favorites[*i].name),
            })
            .collect()
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_item_named(&self, name: &str) -> Option<usize> {
        self.items.iter().position(|it| match it {
            Item::Folder { ix, .. } => self.folders[*ix].name == name,
            Item::Fav(i) => self.favorites[*i].name == name,
            _ => false,
        })
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_filter_text(&self, cx: &App) -> String {
        self.filter.read(cx).text().to_string()
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_showing_favorites(&self) -> bool {
        self.show_favorites
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_selected_favorite(&self) -> Option<(String, usize, bool)> {
        let ix = self.selected?;
        let Item::Fav(i) = self.items.get(ix)? else { return None };
        let view = self.list.viewport_bounds();
        let inside = self.list.bounds_for_item(ix).is_some_and(|b| b.top() >= view.top() - px(0.5) && b.bottom() <= view.bottom() + px(0.5));
        Some((self.favorites[*i].id.clone(), ix, inside))
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_show_favorites(&mut self, cx: &mut Context<Self>) {
        self.read_favorites(cx);
        self.set_favorites(true, cx);
        self.apply_filter(cx);
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_open_fav_menu(&mut self, item_ix: usize, cx: &mut Context<Self>) {
        if let (Some(Item::Fav(i)), Some(p)) = (self.items.get(item_ix), self.demo_item_point(item_ix)) {
            self.selected = Some(item_ix);
            let i = *i;
            self.open_menu(MenuOn::Fav(i), point(p.x + px(60.), p.y), cx);
        }
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_rename(&mut self, fav_ix: usize, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.start_rename(fav_ix, window, cx);
        if let Some((_, input)) = &self.renaming {
            input.update(cx, |i, cx| i.set_text(name, cx));
        }
        self.commit_rename(cx);
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_has_focus(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_dropdown_open(&self) -> bool {
        self.chip_menu
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_dropdown_bounds(&self) -> Option<Bounds<Pixels>> {
        self.chip_bounds.get()
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_clear_asking(&self) -> bool {
        self.clear_ask.is_some()
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_clear_bounds(&self) -> Option<Bounds<Pixels>> {
        self.clear_bounds.get()
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_open_entry_menu(&mut self, entry_ix: usize, cx: &mut Context<Self>) {
        let Some(ix) = self.items.iter().position(|it| matches!(it, Item::Entry(i) if *i == entry_ix)) else { return };
        if let Some(p) = self.demo_item_point(ix) {
            self.selected = Some(ix);
            self.open_menu(MenuOn::Entry(entry_ix), point(p.x + px(60.), p.y), cx);
        }
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_menu_labels(&self) -> Vec<String> {
        let Some(m) = &self.menu else { return vec![] };
        self.menu_entries(&m.on).iter().map(|e| e.label().map_or("—".to_string(), str::to_string)).collect()
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_new_folder_bounds(&self) -> Option<Bounds<Pixels>> {
        self.new_folder_bounds.get()
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_fav_menu_open(&self) -> bool {
        self.menu.is_some()
    }

    #[cfg(feature = "checks")]
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

    #[cfg(feature = "checks")]
    pub(crate) fn demo_drop_at(&self) -> Option<String> {
        match self.drop_at.as_ref()? {
            DropAt::Top => Some("top".into()),
            DropAt::Folder(id) => self.folders.iter().find(|d| d.id == *id).map(|d| d.name.clone()),
        }
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_folder_drop(&self) -> Option<(usize, Vec<(usize, bool)>)> {
        let slot = self.folder_drop?;
        Some((slot, (0..self.items.len()).filter_map(|ix| self.folder_line(ix).map(|after| (ix, after))).collect()))
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_renaming_index(&self) -> Option<usize> {
        match &self.renaming {
            Some((Naming::Fav(i), _)) => Some(*i),
            _ => None,
        }
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_renaming_folder(&self) -> Option<String> {
        match &self.renaming {
            Some((Naming::Folder(id), _)) => self.folders.iter().find(|d| d.id == *id).map(|d| d.name.clone()),
            _ => None,
        }
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_renaming_focused(&self, window: &Window, cx: &App) -> bool {
        self.renaming.as_ref().is_some_and(|(_, input)| input.focus_handle(cx).is_focused(window))
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_rename_field_point(&self, cx: &App) -> Option<Point<Pixels>> {
        let (x, y, w, h) = self.renaming.as_ref()?.1.read(cx).demo_bounds()?;
        Some(point(px(x + w * 0.5), px(y + h * 0.5)))
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_item_point(&self, item_ix: usize) -> Option<Point<Pixels>> {
        let b = self.list.bounds_for_item(item_ix)?;
        Some(point(b.origin.x + px(40.), b.origin.y + b.size.height * 0.5))
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_star_point(&self, item_ix: usize) -> Option<Point<Pixels>> {
        let b = self.list.bounds_for_item(item_ix)?;
        // Past the row's 6 pt margin and 8 pt padding, the 20 pt star ends its first line.
        Some(point(b.right() - px(6. + 8. + 10.), b.top() + px(7. + 10.)))
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_list_bounds(&self) -> Bounds<Pixels> {
        self.list.viewport_bounds()
    }
}

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
        let favs = self.show_favorites;
        let total = if favs { self.favorites.len() } else { self.entries.len() };
        if self.folding_moves() {
            window.request_animation_frame();
        } else if self.folding.as_ref().is_some_and(|f| f.2) {
            self.folding = None;
        }
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
                .when(top_drop, |d| d.child(div().absolute().inset_0().mx(px(6.)).border_1().border_color(t.acc.opacity(0.45))))
                .into_any_element()
        };
        // Transparent: the sidebar's glass shows through.
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
                if h.renaming.take().is_some() || h.menu.take().is_some() {
                    cx.notify();
                } else {
                    cx.emit(HistoryEvent::Close);
                }
            }))
            .child(
                div()
                    .id("history-head")
                    .h(px(34.))
                    .flex_none()
                    .pl(px(14.))
                    .pr(px(8.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().child(ui::caps(&t, if favs { "FAVORITES" } else { "HISTORY" })))
                    .when(favs, |d| {
                        d.child(self.render_new_folder(&t, cx)).on_mouse_down(gpui::MouseButton::Right, cx.listener(|h, ev: &gpui::MouseDownEvent, _, cx| {
                            h.open_menu(MenuOn::Panel, ev.position, cx);
                            cx.stop_propagation();
                        }))
                    }),
            )
            .child(
                div()
                    .mx(px(8.))
                    .mb(px(6.))
                    .h(px(28.))
                    .flex_none()
                    .pl(px(8.))
                    .pr(px(6.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(7.))
                    .relative()
                    .bg(t.hov)
                    .border_1()
                    .border_color(if filter_focused { t.mark_b } else { t.field_b })
                    .when(filter_focused, |d| d.child(ui::halo_ring(t.mark)))
                    .child(icon(Icon::Search).size(px(13.)).text_color(t.tx3))
                    .child(div().flex_1().min_w_0().text_color(t.tx).child(self.filter.clone()))
                    .when(!self.query.is_empty(), |d| {
                        d.child(div().flex_none().text_size(px(10.5)).text_color(t.tx3).whitespace_nowrap().child(format!("{} of {}", ui_count(self.shown), ui_count(total))))
                    }),
            )
            .when(!favs, |d| {
                d.child(
                    div()
                        .mx(px(8.))
                        .mb(px(6.))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.))
                        .child(self.render_chip_dropdown(&t, cx))
                        .child(self.render_clear(&t, cx)),
                )
            })
            .child(body)
            .children(self.render_menu(&t, window, cx))
    }
}
