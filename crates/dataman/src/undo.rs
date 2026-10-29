use std::collections::VecDeque;

use dataman_core::favorites::RemovedFolder;
use dataman_core::groups::Pane;
use dataman_core::layout::PanelLayout;
use dataman_core::pins::Pin;
use dataman_core::session::TabState;
use dataman_core::{ConnFolder, ConnectionProfile, Favorite};
use gpui::{Context, Entity, EntityId, Focusable, InteractiveElement, KeyBindingContextPredicate, Window, actions};

use crate::app::{Region, Workspace};
use crate::tab::QueryTab;

actions!(
    undo,
    [
        Undo,
        Redo,
    ]
);

pub const KEYS: &str = "Workspace && !Modal && !CodeEditor && !TextInput && !GridCellEditor && !ValueEditor";

pub const CAP: usize = 100;

#[derive(Clone)]
pub(crate) struct ClosedTab {
    pub state: TabState,
    pub ix: usize,
    pub pane: Pane,
    pub emptied: bool,
}

/// No `Debug`: it holds the deleted connection's passwords (memory only).
#[derive(Clone)]
pub(crate) struct DeletedConn {
    pub profile: ConnectionProfile,
    pub order: Vec<String>,
    pub password: Option<String>,
    pub ssh_secret: Option<String>,
    pub history: Option<Vec<u8>>,
    pub pins: Vec<Pin>,
    /// The explorer folder it was in.
    pub folder: Option<String>,
}

#[derive(Clone, PartialEq)]
pub(crate) struct Sizes {
    pub explorer_w: f32,
    pub side_w: f32,
    pub editor_frac: Option<f32>,
    pub ratio: Option<f32>,
}

#[derive(Clone)]
pub(crate) enum Op {
    Favorite { fav: Favorite, ix: usize, added: bool },
    FavoriteName { id: String, from: String, to: String },
    FavoriteFolder { id: String, from: Option<String>, to: Option<String> },
    Folder { removed: RemovedFolder, added: bool },
    FolderName { id: String, from: String, to: String },
    FolderOrder { from: Vec<String>, to: Vec<String> },
    ConnOrder { from: Vec<String>, to: Vec<String> },
    /// The explorer's folders, as a whole (made, renamed, colored, deleted, a connection moved).
    ConnFolders { from: Vec<ConnFolder>, to: Vec<ConnFolder> },
    ConnDeleted(Box<DeletedConn>),
    Pins { conn: String, from: Vec<Pin>, to: Vec<Pin> },
    Layout { from: PanelLayout, to: PanelLayout },
    Sizes { from: Sizes, to: Sizes },
    Tabs { closed: Vec<ClosedTab>, reopened: Vec<EntityId> },
    History { entry: Option<(String, dataman_core::HistoryEntry)>, taken: Vec<(String, Vec<u8>)> },
    Many(Vec<Op>),
}

#[derive(Clone)]
pub(crate) struct Entry {
    pub label: String,
    pub op: Op,
}

impl Entry {
    pub fn new(label: impl Into<String>, op: Op) -> Entry {
        Entry { label: label.into(), op }
    }
}

#[derive(Default)]
pub(crate) struct UndoStack {
    done: VecDeque<Entry>,
    undone: Vec<Entry>,
    replaying: bool,
}

impl UndoStack {
    pub fn push(&mut self, e: Entry) {
        if self.replaying {
            return;
        }
        self.undone.clear();
        if let Op::FolderName { id, to, .. } = &e.op
            && let Some(top) = self.done.back_mut()
            && let Op::Folder { removed, added: true } = &mut top.op
            && removed.folder.id == *id
        {
            removed.folder.name = to.clone();
            top.label = format!("New folder '{to}'");
            return;
        }
        self.done.push_back(e);
        while self.done.len() > CAP {
            self.done.pop_front();
        }
    }

    pub fn next_undo(&self) -> Option<&str> {
        self.done.back().map(|e| e.label.as_str())
    }

    pub fn next_redo(&self) -> Option<&str> {
        self.undone.last().map(|e| e.label.as_str())
    }

    #[cfg(test)]
    fn labels(&self) -> (Vec<&str>, Vec<&str>) {
        (self.done.iter().map(|e| e.label.as_str()).collect(), self.undone.iter().map(|e| e.label.as_str()).collect())
    }
}

pub(crate) fn outside_text(window: &Window) -> bool {
    KeyBindingContextPredicate::parse(KEYS).is_ok_and(|p| p.depth_of(&window.context_stack()).is_some())
}

pub(crate) fn workspace_actions(d: gpui::Stateful<gpui::Div>, cx: &mut Context<Workspace>) -> gpui::Stateful<gpui::Div> {
    d.on_action(cx.listener(|w, _: &Undo, window, cx| w.undo(window, cx)))
        .on_action(cx.listener(|w, _: &Redo, window, cx| w.redo(window, cx)))
        .on_action(cx.listener(|w, _: &crate::editor::Undo, window, cx| {
            if outside_text(window) {
                w.undo(window, cx);
            }
        }))
        .on_action(cx.listener(|w, _: &crate::editor::Redo, window, cx| {
            if outside_text(window) {
                w.redo(window, cx);
            }
        }))
}

type Applied = Result<(), String>;

fn said<T>(r: anyhow::Result<T>) -> Result<T, String> {
    r.map_err(|e| format!("{e:#}"))
}

impl Workspace {
    pub(crate) fn did(&mut self, label: impl Into<String>, op: Op) {
        self.undo.push(Entry::new(label, op));
    }

    pub(crate) fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step(true, window, cx);
    }

    pub(crate) fn redo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step(false, window, cx);
    }

    fn step(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let next = if back { self.undo.done.pop_back() } else { self.undo.undone.pop() };
        let Some(mut e) = next else {
            self.notice(if back { "Nothing to undo" } else { "Nothing to redo" }, cx);
            return;
        };
        let region = self.focused_region(window, cx);
        self.undo.replaying = true;
        let r = self.apply(&mut e.op, back, window, cx);
        self.undo.replaying = false;
        self.keep_focus(region, window, cx);
        self.history.update(cx, |h, cx| h.reload_favorites(cx));
        self.sync_favorite_tabs(cx);
        match r {
            Ok(()) => {
                self.notice(format!("{}: {}", if back { "Undid" } else { "Redid" }, e.label), cx);
                if back {
                    self.undo.undone.push(e);
                } else {
                    self.undo.done.push_back(e);
                }
            }
            Err(why) => self.notice(format!("Cannot {} {}: {why}", if back { "undo" } else { "redo" }, e.label), cx),
        }
        cx.notify();
    }

    fn keep_focus(&mut self, region: Option<Region>, window: &mut Window, cx: &mut Context<Self>) {
        let f = match region {
            Some(Region::Explorer) if self.view == dataman_core::layout::NavView::Explorer => Some(self.tree.focus_handle(cx)),
            Some(Region::Explorer) if self.view == dataman_core::layout::NavView::Agents => Some(self.agents.focus_handle(cx)),
            Some(Region::Explorer) => Some(self.history.focus_handle(cx)),
            Some(Region::Grid) if self.results_shown(cx) => self.active_tab().map(|t| {
                let t = t.read(cx);
                if t.shows_plan(cx) { t.plan.focus_handle(cx) } else { t.grid.focus_handle(cx) }
            }),
            _ => None,
        };
        if let Some(f) = f {
            window.focus(&f, cx);
        }
    }

    fn apply(&mut self, op: &mut Op, back: bool, window: &mut Window, cx: &mut Context<Self>) -> Applied {
        let core = self.store.read(cx).core.clone();
        let favs = &core.favorites;
        match op {
            Op::Favorite { fav, ix, added } => {
                if *added == back {
                    let (i, f) = said(favs.take(&fav.id))?.ok_or("the favorite is gone")?;
                    (*ix, *fav) = (i, f);
                } else if !said(favs.restore(*ix, fav.clone()))? {
                    return Err("it is saved already, or its folder is gone".into());
                }
            }
            Op::FavoriteName { id, from, to } => {
                said(favs.rename(id, if back { from } else { to }))?.ok_or("the favorite is gone")?;
            }
            Op::FavoriteFolder { id, from, to } => {
                said(favs.move_to_folder(id, if back { from.as_deref() } else { to.as_deref() }))?.ok_or("the favorite is gone")?;
            }
            Op::Folder { removed, added } => {
                if *added == back {
                    *removed = said(favs.remove_folder(&removed.folder.id))?.ok_or("the folder is gone")?;
                } else if !said(favs.restore_folder(removed))? {
                    return Err("the folder is there already".into());
                }
            }
            Op::FolderName { id, from, to } => {
                said(favs.rename_folder(id, if back { from } else { to }))?.ok_or("the folder is gone")?;
            }
            Op::FolderOrder { from, to } => said(favs.reorder_folders(if back { from } else { to }))?,
            Op::ConnOrder { from, to } => {
                let ids = if back { from } else { to };
                said(core.config.reorder(ids))?;
                self.store.update(cx, |s, cx| s.reload(cx));
            }
            Op::ConnFolders { from, to } => {
                let next = if back { from.clone() } else { to.clone() };
                said(self.store.update(cx, |s, cx| s.edit_folders(|f| *f = next, cx)))?;
                self.tree.update(cx, |t, cx| t.folders_changed(cx));
            }
            Op::ConnDeleted(d) => {
                if back {
                    self.restore_connection(d, cx)?;
                } else {
                    **d = said(self.remove_connection(&d.profile.id.clone(), cx))?;
                }
            }
            Op::Pins { conn, from, to } => {
                if self.store.read(cx).profile(conn).is_none() {
                    return Err("its connection is gone".into());
                }
                said(core.pins.set_of(conn, if back { from } else { to }))?;
                self.tree.update(cx, |t, cx| t.rebuild(cx));
            }
            Op::Layout { from, to } => {
                let next = if back { from.clone() } else { to.clone() };
                self.set_layout(next, None, cx);
            }
            Op::Sizes { from, to } => self.set_sizes(if back { from } else { to }),
            Op::Tabs { closed, reopened } => {
                if back {
                    *reopened = self.reopen_tabs(closed, window, cx);
                } else {
                    let open: Vec<Entity<QueryTab>> = reopened.iter().filter_map(|id| self.tabs.iter().find(|t| t.entity_id() == *id).cloned()).collect();
                    if open.is_empty() {
                        return Err("its tabs are closed already".into());
                    }
                    *closed = self.close_tabs_quietly(open, window, cx);
                    reopened.clear();
                }
            }
            Op::History { entry, taken } => {
                let h = &core.history;
                if back {
                    for (conn, bytes) in taken.iter() {
                        said(h.restore(conn, bytes))?;
                    }
                } else {
                    *taken = match entry {
                        Some((conn, e)) => vec![(conn.clone(), said(h.remove(conn, e))?.ok_or("the entry is gone")?)],
                        None => self.store.read(cx).profiles.iter().filter_map(|p| h.take(&p.id).ok().flatten().map(|b| (p.id.clone(), b))).collect(),
                    };
                }
                self.history.update(cx, |h, cx| h.reload(cx));
            }
            Op::Many(ops) => {
                if back {
                    for op in ops.iter_mut().rev() {
                        self.apply(op, true, window, cx)?;
                    }
                } else {
                    for op in ops.iter_mut() {
                        self.apply(op, false, window, cx)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn remove_connection(&mut self, id: &str, cx: &mut Context<Self>) -> anyhow::Result<DeletedConn> {
        let core = self.store.read(cx).core.clone();
        let store = self.store.read(cx);
        let profile = store.profile(id).cloned().ok_or_else(|| anyhow::anyhow!("no connection {id}"))?;
        let order: Vec<String> = store.profiles.iter().map(|p| p.id.clone()).collect();
        let (password, ssh_secret) = if profile.save_password {
            (core.secrets.get(id).ok().flatten(), core.secrets.get(&profile.ssh_secret_id()).ok().flatten())
        } else {
            (None, None)
        };
        let pins = core.pins.of(id);
        let folder = store.folder_of(id).map(|d| d.id.clone());
        core.remove_connection(id)?;
        let history = core.history.take(id).ok().flatten();
        self.disconnect(id.to_string(), cx);
        self.store.update(cx, |s, cx| s.reload(cx));
        if self.current_conn.as_deref() == Some(id) {
            self.current_conn = self.store.read(cx).profiles.first().map(|p| p.id.clone());
        }
        Ok(DeletedConn { profile, order, password, ssh_secret, history, pins, folder })
    }

    fn restore_connection(&mut self, d: &DeletedConn, cx: &mut Context<Self>) -> Applied {
        let id = d.profile.id.clone();
        if self.store.read(cx).profile(&id).is_some() {
            return Err("it is there already".into());
        }
        let core = self.store.read(cx).core.clone();
        said(core.save_connection(d.profile.clone(), d.password.as_deref(), d.ssh_secret.as_deref()))?;
        said(core.config.reorder(&d.order))?;
        if let Some(h) = &d.history {
            said(core.history.put_back(&id, h))?;
        }
        said(core.pins.set_of(&id, &d.pins))?;
        if let Some(f) = &d.folder {
            said(core.config.edit_folders(|all| {
                if let Some(x) = all.iter_mut().find(|x| x.id == *f) {
                    x.connections.push(id.clone());
                }
            }))?;
        }
        self.store.update(cx, |s, cx| s.reload(cx));
        self.tree.update(cx, |t, cx| t.rebuild(cx));
        Ok(())
    }

    pub(crate) fn sizes_now(&self) -> Sizes {
        Sizes {
            explorer_w: self.explorer_w,
            side_w: self.side_w,
            editor_frac: self.editor_frac,
            ratio: self.split.as_ref().map(|s| s.ratio),
        }
    }

    fn set_sizes(&mut self, s: &Sizes) {
        self.explorer_w = s.explorer_w;
        self.side_w = s.side_w;
        self.editor_frac = s.editor_frac;
        if let (Some(sp), Some(r)) = (&mut self.split, s.ratio) {
            sp.ratio = r;
        }
    }

    pub(crate) fn closing(&self, ix: usize, cx: &gpui::App) -> Option<ClosedTab> {
        let tab = self.tabs.get(ix)?;
        if tab.read(cx).monitor.is_some() {
            return None;
        }
        let id = tab.entity_id();
        let pane = self.pane_of(id);
        let emptied = self.split.is_some() && self.group(pane).len() == 1;
        Some(ClosedTab { state: tab.read(cx).session_state(cx), ix, pane, emptied })
    }

    fn reopen_tabs(&mut self, closed: &[ClosedTab], window: &mut Window, cx: &mut Context<Self>) -> Vec<EntityId> {
        let lone = (self.tabs.len() == 1).then(|| self.tabs[0].clone()).filter(|t| {
            let t = t.read(cx);
            t.monitor.is_none() && t.result().is_none() && t.editor.read(cx).text().trim().is_empty()
        });
        let mut ids = Vec::new();
        for c in closed.iter().rev() {
            let left_alone = c.pane == Pane::Left && c.emptied && self.split.is_none() && !self.tabs.is_empty();
            self.open_in = match c.pane {
                Pane::Right => Some(Pane::Right),
                Pane::Left if self.split.is_some() => Some(Pane::Left),
                Pane::Left => None,
            };
            let tab = self.new_tab(window, cx);
            self.open_in = None;
            self.fill_tab(&tab, &c.state, cx);
            let id = tab.entity_id();
            if left_alone {
                self.split_off_left(id, window, cx);
            }
            if let Some(from) = self.tabs.iter().position(|t| t.entity_id() == id) {
                let t = self.tabs.remove(from);
                let to = c.ix.min(self.tabs.len());
                self.tabs.insert(to, t);
            }
            ids.push(id);
        }
        if let Some(lone) = lone
            && self.tabs.len() > 1
            && let Some(ix) = self.tabs.iter().position(|t| *t == lone)
        {
            self.close_tab_quietly(ix, window, cx);
        }
        if let Some(ix) = ids.last().and_then(|id| self.tabs.iter().position(|t| t.entity_id() == *id)) {
            self.active = ix;
            self.activate(ix, window, cx);
        }
        ids
    }
}

pub(crate) fn palette_items(w: &Workspace) -> Vec<crate::palette::Item> {
    use crate::palette::{Item, Target};
    let item = |verb: &str, next: Option<&str>, action: Box<dyn gpui::Action>| {
        let mut it = match next {
            Some(l) => Item::command(&format!("{verb} {l}"), "outside text fields and the editor", "", action, Target::Workspace),
            None => Item::command(verb, &format!("nothing to {}", verb.to_lowercase()), "", action, Target::Workspace),
        };
        it.disabled = next.is_none();
        it
    };
    vec![item("Undo", w.undo.next_undo(), Box::new(Undo)), item("Redo", w.undo.next_redo(), Box::new(Redo))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use dataman_core::favorites::Folder;
    use gpui::{KeyContext, Keymap};

    fn fav_op(name: &str) -> Entry {
        Entry::new(format!("Add favorite '{name}'"), Op::Favorite { fav: Favorite::new(name, format!("select '{name}'")), ix: 0, added: true })
    }

    #[test]
    fn cap_and_redo_cleared() {
        let mut s = UndoStack::default();
        for i in 0..CAP + 5 {
            s.push(fav_op(&format!("f{i}")));
        }
        assert_eq!(s.done.len(), CAP);
        assert_eq!(s.done.front().map(|e| e.label.as_str()), Some("Add favorite 'f5'"), "the oldest went first");
        assert_eq!(s.next_undo(), Some(&*format!("Add favorite 'f{}'", CAP + 4)));
        assert_eq!(s.next_redo(), None);
        let e = s.done.pop_back().unwrap();
        s.undone.push(e);
        assert_eq!(s.next_redo(), Some(&*format!("Add favorite 'f{}'", CAP + 4)));
        s.push(fav_op("new"));
        assert_eq!(s.next_redo(), None, "a new step: nothing to redo");
        assert_eq!(s.next_undo(), Some("Add favorite 'new'"));
    }

    #[test]
    fn replaying_records_nothing() {
        let mut s = UndoStack::default();
        s.push(fav_op("a"));
        s.replaying = true;
        s.push(fav_op("b"));
        s.replaying = false;
        assert_eq!(s.labels(), (vec!["Add favorite 'a'"], vec![]));
    }

    #[test]
    fn a_new_folder_and_its_name_are_one_step() {
        let mut s = UndoStack::default();
        let folder = Folder { id: "d1".into(), name: "New folder".into() };
        let made = || Entry::new("New folder 'New folder'", Op::Folder { removed: RemovedFolder { index: 0, folder: folder.clone(), favorites: vec![] }, added: true });
        let named = |to: &str| Entry::new("Rename folder", Op::FolderName { id: "d1".into(), from: "New folder".into(), to: to.into() });
        s.push(made());
        s.push(named("Reports"));
        assert_eq!(s.labels(), (vec!["New folder 'Reports'"], vec![]));
        let Some(Op::Folder { removed, .. }) = s.done.back().map(|e| &e.op) else { panic!() };
        assert_eq!(removed.folder.name, "Reports", "redone, it comes back with its name");
        s.push(fav_op("x"));
        s.push(named("Monthly"));
        assert_eq!(s.labels().0, vec!["New folder 'Reports'", "Add favorite 'x'", "Rename folder"]);
    }

    fn stack(s: &[&str]) -> Vec<KeyContext> {
        s.iter().map(|c| KeyContext::parse(c).unwrap()).collect()
    }

    #[test]
    fn undo_keys_stay_out_of_text() {
        let keymap = Keymap::new(vec![
            crate::keymap::kb("cmd-z", crate::editor::Undo, Some("CodeEditor")),
            crate::keymap::kb("cmd-z", crate::input::Undo, Some("TextInput")),
            crate::keymap::kb("cmd-z", Undo, Some(KEYS)),
            crate::keymap::kb("ctrl-z", Undo, Some(KEYS)),
            crate::keymap::kb("cmd-shift-z", Redo, Some(KEYS)),
            crate::keymap::kb("ctrl-shift-z", Redo, Some(KEYS)),
        ]);
        let first = |k: &str, s: &[&str]| keymap.bindings_for_input(&[crate::keymap::keystroke(k)], &stack(s)).0.first().map(|b| b.action().name());
        for k in ["cmd-z", "ctrl-z"] {
            assert_eq!(first(k, &["Workspace", "Explorer"]), Some("undo::Undo"), "{k} on the explorer");
            assert_eq!(first(k, &["Workspace", "ResultGrid"]), Some("undo::Undo"), "{k} on the grid");
            assert_eq!(first(k, &["Workspace", "HistoryPanel"]), Some("undo::Undo"), "{k} on the favorites");
            assert_eq!(first(k, &["Workspace"]), Some("undo::Undo"));
            assert_ne!(first(k, &["Workspace", "Explorer", "TextInput"]), Some("undo::Undo"), "{k} in the explorer's filter");
            assert_ne!(first(k, &["Workspace", "ResultGrid", "GridCellEditor", "TextInput"]), Some("undo::Undo"), "{k} in the cell editor");
            assert_ne!(first(k, &["Workspace", "Palette Modal", "TextInput"]), Some("undo::Undo"), "{k} behind the palette");
            assert_ne!(first(k, &["Workspace", "ConfirmDiscard Modal"]), Some("undo::Undo"));
        }
        assert_eq!(first("cmd-z", &["Workspace", "CodeEditor"]), Some("code_editor::Undo"), "⌘Z in the editor is its text's");
        #[cfg(target_os = "macos")]
        assert_eq!(first("ctrl-z", &["Workspace", "CodeEditor"]), None, "⌃Z in the editor is nothing");
        assert_eq!(first("cmd-z", &["Workspace", "HistoryPanel", "TextInput"]), Some("text_input::Undo"), "a rename field's own");
        #[cfg(target_os = "macos")]
        assert_eq!(first("ctrl-z", &["Workspace", "HistoryPanel", "TextInput"]), None);
        assert_eq!(first("cmd-shift-z", &["Workspace", "Explorer"]), Some("undo::Redo"));
        assert_eq!(first("ctrl-shift-z", &["Workspace", "ResultGrid"]), Some("undo::Redo"));
        #[cfg(target_os = "macos")]
        assert_eq!(first("ctrl-shift-z", &["Workspace", "CodeEditor"]), None);
    }
}
