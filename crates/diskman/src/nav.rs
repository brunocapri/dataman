//! Back / forward through the workspace, like a browser's: every jump (another tab, a table
//! opened from the explorer, a reference followed, another result) remembers where the focus was;
//! ⌃⌘← / ⌃⌘→, the mouse's side buttons and the title bar's arrows go back there and forth.

use gpui::{Context, EntityId, Window};
use diskman_core::batch::Key;

use crate::app::{Region, Workspace};
use crate::grid::GridSpot;

/// Entries kept behind (and ahead).
pub const CAP: usize = 100;

/// The two stacks. A place is the same as another when `same` says so: a jump to where one
/// already stands is no jump, and going back skips places that are where one is now.
#[derive(Debug)]
pub struct History<P> {
    back: Vec<P>,
    fwd: Vec<P>,
    cap: usize,
}

impl<P> Default for History<P> {
    fn default() -> Self {
        History { back: Vec::new(), fwd: Vec::new(), cap: CAP }
    }
}

impl<P> History<P> {
    #[cfg(test)]
    fn with_cap(cap: usize) -> Self {
        History { back: Vec::new(), fwd: Vec::new(), cap }
    }

    /// A jump away from `from`: it goes behind, what was ahead is gone.
    pub fn push(&mut self, from: P, same: impl Fn(&P, &P) -> bool) {
        self.fwd.clear();
        if let Some(top) = self.back.last_mut()
            && same(top, &from)
        {
            *top = from;
            return;
        }
        self.back.push(from);
        if self.back.len() > self.cap {
            self.back.remove(0);
        }
    }

    /// Back from `here`: the last place still `alive` and not `here`; `here` goes ahead.
    pub fn back(&mut self, here: P, alive: impl Fn(&P) -> bool, same: impl Fn(&P, &P) -> bool) -> Option<P> {
        step(&mut self.back, &mut self.fwd, self.cap, here, alive, same)
    }

    /// Forward from `here`, the mirror of `back`.
    pub fn forward(&mut self, here: P, alive: impl Fn(&P) -> bool, same: impl Fn(&P, &P) -> bool) -> Option<P> {
        step(&mut self.fwd, &mut self.back, self.cap, here, alive, same)
    }

    pub fn can_back(&self, alive: impl Fn(&P) -> bool) -> bool {
        self.back.iter().any(alive)
    }

    pub fn can_forward(&self, alive: impl Fn(&P) -> bool) -> bool {
        self.fwd.iter().any(alive)
    }

    /// Drops the places that went away (a closed tab).
    pub fn retain(&mut self, alive: impl Fn(&P) -> bool) {
        self.back.retain(&alive);
        self.fwd.retain(&alive);
    }

    pub fn clear(&mut self) {
        self.back.clear();
        self.fwd.clear();
    }
}

fn step<P>(from: &mut Vec<P>, to: &mut Vec<P>, cap: usize, here: P, alive: impl Fn(&P) -> bool, same: impl Fn(&P, &P) -> bool) -> Option<P> {
    while let Some(p) = from.pop() {
        if !alive(&p) || same(&p, &here) {
            continue;
        }
        to.push(here);
        if to.len() > cap {
            to.remove(0);
        }
        return Some(p);
    }
    None
}

/// A place in the workspace: the tab, the region with the keys, the result on screen, the
/// grid's cell and scroll, the editor's cursor.
#[derive(Clone, Debug)]
pub struct Place {
    pub tab: EntityId,
    pub region: Option<Region>,
    pub result: Option<Key>,
    pub grid: Option<GridSpot>,
    pub cursor: Option<usize>,
}

/// Same tab, same result: the cell and the focus inside it do not make another place.
pub fn same(a: &Place, b: &Place) -> bool {
    a.tab == b.tab && a.result == b.result
}

/// The workspace's history and whether a step is being taken (it records nothing).
#[derive(Default)]
pub struct Nav {
    pub history: History<Place>,
    pub stepping: bool,
}

impl Workspace {
    /// Where the workspace stands now (`None` before the first tab). Without a window the
    /// region is unknown and a step back leaves the focus where it is.
    pub(crate) fn nav_place(&self, window: Option<&Window>, cx: &gpui::App) -> Option<Place> {
        let tab = self.active_tab()?;
        let t = tab.read(cx);
        Some(Place {
            tab: tab.entity_id(),
            region: window.and_then(|w| self.focused_region(w, cx)),
            result: t.shown_key(),
            grid: Some(t.grid.read(cx).spot()),
            cursor: Some(t.editor.read(cx).selection_ends().1),
        })
    }

    /// Called before a jump: the place being left goes behind.
    pub(crate) fn nav_record(&mut self, window: Option<&Window>, cx: &gpui::App) {
        if self.nav.stepping {
            return;
        }
        if let Some(p) = self.nav_place(window, cx) {
            self.nav.history.push(p, same);
        }
    }

    fn nav_alive(&self) -> impl Fn(&Place) -> bool + '_ {
        move |p: &Place| self.tabs.iter().any(|t| t.entity_id() == p.tab)
    }

    pub(crate) fn can_go(&self, forward: bool) -> bool {
        let alive = self.nav_alive();
        if forward { self.nav.history.can_forward(alive) } else { self.nav.history.can_back(alive) }
    }

    /// ⌃⌘← / ⌃⌘→, the side buttons, the title bar's arrows.
    pub(crate) fn go(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(here) = self.nav_place(Some(window), cx) else { return };
        let tabs: Vec<EntityId> = self.tabs.iter().map(|t| t.entity_id()).collect();
        let alive = |p: &Place| tabs.contains(&p.tab);
        let to = if forward { self.nav.history.forward(here, alive, same) } else { self.nav.history.back(here, alive, same) };
        let Some(to) = to else { return };
        self.nav.stepping = true;
        self.nav_restore(&to, window, cx);
        self.nav.stepping = false;
        cx.notify();
    }

    fn nav_restore(&mut self, p: &Place, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter().find(|t| t.entity_id() == p.tab).cloned() else { return };
        if self.active_tab() != Some(&tab) {
            self.activate_tab(&tab, window, cx);
        }
        if let Some(k) = p.result
            && tab.read(cx).shown_key() != Some(k)
            && tab.read(cx).result_tabs().iter().any(|r| r.key == k)
        {
            tab.update(cx, |t, cx| t.select_result(k, cx));
        }
        let (grid, editor) = {
            let t = tab.read(cx);
            (t.grid.clone(), t.editor.clone())
        };
        if let Some(s) = &p.grid {
            grid.update(cx, |g, cx| g.set_spot(s, cx));
        }
        if let Some(c) = p.cursor
            && editor.read(cx).selection_ends().1 != c
        {
            editor.update(cx, |e, cx| e.set_cursor(c, cx));
        }
        // A sheet that took the keys on the way in keeps them.
        let sheet = {
            let t = tab.read(cx);
            t.pending_run.is_some() || t.confirm.is_some() || t.params.is_some()
        };
        if let Some(r) = p.region
            && !sheet
            && self.focused_region(window, cx) != Some(r)
        {
            let visible = self.focus_visible;
            self.focus_region(r, window, cx);
            self.focus_visible = visible;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eq(a: &u32, b: &u32) -> bool {
        a == b
    }

    fn all(_: &u32) -> bool {
        true
    }

    #[test]
    fn back_and_forward() {
        let mut h = History::default();
        // 1 → 2 → 3
        h.push(1, eq);
        h.push(2, eq);
        assert!(h.can_back(all) && !h.can_forward(all));
        assert_eq!(h.back(3, all, eq), Some(2));
        assert_eq!(h.back(2, all, eq), Some(1));
        assert_eq!(h.back(1, all, eq), None);
        assert!(!h.can_back(all) && h.can_forward(all));
        assert_eq!(h.forward(1, all, eq), Some(2));
        assert_eq!(h.forward(2, all, eq), Some(3));
        assert_eq!(h.forward(3, all, eq), None);
        assert_eq!(h.back(3, all, eq), Some(2));
    }

    #[test]
    fn a_new_jump_drops_forward() {
        let mut h = History::default();
        h.push(1, eq);
        h.push(2, eq);
        assert_eq!(h.back(3, all, eq), Some(2));
        assert!(h.can_forward(all));
        // From 2 to 4: 3 is gone.
        h.push(2, eq);
        assert!(!h.can_forward(all));
        assert_eq!(h.back(4, all, eq), Some(2));
        assert_eq!(h.back(2, all, eq), Some(1));
    }

    #[test]
    fn closed_places_are_skipped() {
        let mut h = History::default();
        for p in [1, 2, 3] {
            h.push(p, eq);
        }
        let alive = |p: &u32| *p != 2;
        assert_eq!(h.back(4, alive, eq), Some(3));
        assert_eq!(h.back(3, alive, eq), Some(1));
        assert_eq!(h.forward(1, alive, eq), Some(3));
        h.retain(|p| *p == 1);
        assert!(!h.can_forward(all));
        assert!(h.can_back(all));
        assert!(!h.can_back(|p| *p != 1));
    }

    #[test]
    fn the_same_place_is_one_entry() {
        let mut h = History::default();
        h.push(1, eq);
        h.push(1, eq);
        h.push(2, eq);
        assert_eq!(h.back(3, all, eq), Some(2));
        assert_eq!(h.back(2, all, eq), Some(1));
        assert_eq!(h.back(1, all, eq), None);
        // Back skips where one stands.
        let mut h = History::default();
        h.push(1, eq);
        h.push(2, eq);
        assert_eq!(h.back(2, all, eq), Some(1));
    }

    #[test]
    fn capped() {
        let mut h = History::with_cap(3);
        for p in 0..10 {
            h.push(p, eq);
        }
        let mut seen = vec![];
        let mut here = 10;
        while let Some(p) = h.back(here, all, eq) {
            seen.push(p);
            here = p;
        }
        assert_eq!(seen, vec![9, 8, 7]);
    }
}
