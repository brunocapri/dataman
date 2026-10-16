//! Two editor groups side by side (VS Code's "Open to the Side"; the model is
//! `dataman_core::groups`): the work surface splits into a left and a right group, each with its
//! own strip of tabs (in the title bar, over its group) and its own tab in front. One group has
//! the focus — a click anywhere in a group, or the keyboard focus landing in it, gives it the
//! focus — and the workspace's `active` tab is always the focused group's: Run, ⌘D, find, the
//! result zoom, the history and the side panels, an agent's query all act there. The focused
//! group's tab in front wears an accent line; the other group's a lighter wash.
//!
//! Opening a tab to the side (its menu, ⌘\, the palette) copies it into the other group. A query
//! tab is a session (a connection, a transaction, results), not a view of a document that two
//! editors could share: the copy takes the script, the connection and database, the title and
//! the favorite, and opens a session of its own (a table's data view opens the same table), while
//! the tab and whatever it holds stay as they are. A tab moves instead with "Move to the Other
//! Group" or when dragged onto the other strip; an activity monitor (one per connection) always
//! moves. ⌥ held while opening a table from the explorer or a favorite opens it in the other
//! group.
//!
//! The split opens with the new group growing in from the right while the first narrows
//! (`motion::SPLIT`), and folds away the same way; with Reduce Motion it is simply there. A
//! group's × closes the split: its tabs join the other group, none is closed. Closing a group's
//! last tab, or dragging it out, closes the split too. The seam between the groups resizes them;
//! a double-click on it shares the width evenly again.

use std::time::Instant;

use dataman_core::groups::{self, Pane};
use gpui::{App, Context, Entity, EntityId, Focusable, InteractiveElement, IntoElement, MouseButton, Styled, WeakEntity, Window, actions, div};

use crate::app::{Drag, Workspace};
use crate::motion::SplitTween;
use crate::tab::QueryTab;
use crate::theme::{ActiveTheme, px};

actions!(
    split,
    [
        /// A copy of the active tab in the other group (the split opens with it).
        OpenToSide,
        /// The active tab into the other group.
        MoveToOtherGroup,
        /// The other group takes the focus.
        FocusOtherGroup,
        /// The focused group closes: its tabs join the other one.
        CloseGroup,
    ]
);

/// The least width a group keeps while the seam is dragged.
pub const PANE_MIN: f32 = 260.;
/// The hairline between the groups, and its grabber (centred on it).
pub const SEAM: f32 = 1.;
const SEAM_HIT: f32 = 8.;

/// The second group (the right one) and the focus between the two.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Split {
    /// The right group's tabs (all the others are the left's).
    pub right: Vec<EntityId>,
    /// The group with the focus: the workspace's `active` tab is in it.
    pub focus: Pane,
    /// The other group's tab in front.
    pub other: EntityId,
    /// The left group's share of the work surface's width.
    pub ratio: f32,
}

impl Split {
    pub fn pane_of(&self, id: EntityId) -> Pane {
        if self.right.contains(&id) { Pane::Right } else { Pane::Left }
    }
}

/// The split opening or folding away: the group that grows in or folds (the right one when it
/// opens), the left share it moves with, the motion, and the tab the folding group showed (drawn
/// while it folds; gone when it was closed).
pub(crate) struct SplitMove {
    pub leaving: Pane,
    pub ratio: f32,
    pub tween: SplitTween,
    pub ghost: Option<WeakEntity<QueryTab>>,
}

/// The work surface this frame: how far the split is open (0 … 1), the group that moves while it
/// opens or folds, and the left group's share once open.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SplitFrame {
    pub open: f32,
    pub leaving: Pane,
    pub ratio: f32,
}

impl SplitFrame {
    /// The left and right groups' widths in a surface `w` points wide, and the widths they keep
    /// inside while they move (the moving one is clipped, never reflowed).
    pub fn widths(&self, w: f32) -> ((f32, f32), (f32, f32)) {
        (groups::widths(w, self.ratio, SEAM, self.open, self.leaving), groups::widths(w, self.ratio, SEAM, 1., self.leaving))
    }
}

impl Workspace {
    /// The group tab `id` is in (the left one while there is one group).
    pub(crate) fn pane_of(&self, id: EntityId) -> Pane {
        self.split.as_ref().map_or(Pane::Left, |s| s.pane_of(id))
    }

    pub(crate) fn focused_pane(&self) -> Pane {
        self.split.as_ref().map_or(Pane::Left, |s| s.focus)
    }

    /// The tabs of `pane` as indexes into `tabs`, in order (with one group, every tab is the
    /// left's).
    pub(crate) fn group(&self, pane: Pane) -> Vec<usize> {
        (0..self.tabs.len()).filter(|&i| self.pane_of(self.tabs[i].entity_id()) == pane).collect()
    }

    fn group_ids(&self, pane: Pane) -> Vec<EntityId> {
        self.group(pane).into_iter().map(|i| self.tabs[i].entity_id()).collect()
    }

    fn index_of(&self, id: EntityId) -> Option<usize> {
        self.tabs.iter().position(|t| t.entity_id() == id)
    }

    /// The tab in front of `pane`.
    pub(crate) fn front_of(&self, pane: Pane) -> Option<&Entity<QueryTab>> {
        match &self.split {
            Some(s) if s.focus != pane => self.tabs.iter().find(|t| t.entity_id() == s.other),
            None if pane == Pane::Right => None,
            _ => self.active_tab(),
        }
    }

    /// `activate`'s part: the tab at `ix` comes to the front; when it is in the other group, that
    /// group takes the focus (the tab in front so far is the other group's from then on).
    pub(crate) fn enter_group_of(&mut self, ix: usize) {
        let Some(id) = self.tabs.get(ix).map(|t| t.entity_id()) else { return };
        let current = self.tabs.get(self.active).map(|t| t.entity_id());
        let Some(s) = &mut self.split else { return };
        let pane = s.pane_of(id);
        if pane != s.focus {
            if let Some(c) = current {
                s.other = c;
            }
            s.focus = pane;
            // The popups belong to the group they opened in.
            self.conn_menu = false;
            self.copy_menu = false;
        }
    }

    /// A press in group `pane` (its strip or its surface): it takes the focus, and the keys go to
    /// its editor (the press's target may take them next: the grid, a field).
    pub(crate) fn focus_pane(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = &self.split else { return };
        if s.focus == pane {
            return;
        }
        if let Some(ix) = self.index_of(s.other) {
            self.activate(ix, window, cx);
        }
    }

    /// The keyboard focus is in the other group's tab (a press the groups did not see, F6, a
    /// sheet giving it back): that group takes the focus; the keys stay where they are.
    pub(crate) fn sync_group_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.split.as_ref().and_then(|s| self.index_of(s.other)) else { return };
        let inside = {
            let t = self.tabs[ix].read(cx);
            t.editor.focus_handle(cx).contains_focused(window, cx)
                || t.grid.focus_handle(cx).contains_focused(window, cx)
                || t.plan.focus_handle(cx).contains_focused(window, cx)
                || t.monitor.as_ref().is_some_and(|m| m.read(cx).focus_target(cx).contains_focused(window, cx))
        };
        if inside {
            self.nav_record(Some(window), cx);
            self.enter_group_of(ix);
            self.active = ix;
            self.history_pin = None;
            self.sync_bound_views(window, cx);
            cx.notify();
        }
    }

    /// Where a new tab goes (`new_tab`, `prev` the tab in front until now): the focused group, or
    /// the one `open_in` asks for, which then takes the focus (the right one opens the split).
    pub(crate) fn place_new_tab(&mut self, id: EntityId, prev: Option<EntityId>, cx: &App) {
        let want = self.open_in.take();
        match &mut self.split {
            Some(s) => {
                let pane = want.unwrap_or(s.focus);
                if pane == Pane::Right {
                    s.right.push(id);
                }
                if pane != s.focus {
                    if let Some(p) = prev {
                        s.other = p;
                    }
                    s.focus = pane;
                }
            }
            None if want == Some(Pane::Right) => {
                if let Some(p) = prev {
                    self.open_split(Split { right: vec![id], focus: Pane::Right, other: p, ratio: groups::RATIO }, cx);
                }
            }
            None => {}
        }
    }

    /// The group what is opening goes to when ⌥ was held on the press that opens it (a table in
    /// the explorer, a favorite): the other one.
    pub(crate) fn alt_aside(&self, window: &Window) -> Option<Pane> {
        (self.alt_press && window.modifiers().alt).then(|| self.focused_pane().other())
    }

    /// How far the split is open now (0 … 1).
    fn split_progress(&self) -> f32 {
        let target = if self.split.is_some() { 1. } else { 0. };
        self.split_move.as_ref().and_then(|m| m.tween.at(target, Instant::now())).unwrap_or(target)
    }

    fn move_split(&mut self, from: f32, leaving: Pane, ratio: f32, ghost: Option<WeakEntity<QueryTab>>, cx: &App) {
        // Reduce motion: the groups are simply there (or gone).
        self.split_move = (!cx.reduce_motion()).then(|| SplitMove { leaving, ratio, tween: SplitTween::new(from), ghost });
    }

    /// Two groups from now on: the right one grows in from the window's side.
    fn open_split(&mut self, s: Split, cx: &App) {
        let from = self.split_progress();
        let ratio = s.ratio;
        self.split = Some(s);
        self.move_split(from, Pane::Right, ratio, None, cx);
    }

    /// Group `pane` has no tab left (its last one closed, or dragged out): one group from now on,
    /// the empty one folding away (`ghost`: the tab it showed, gone or now in the other group).
    pub(crate) fn split_emptied(&mut self, pane: Pane, ghost: Option<WeakEntity<QueryTab>>, cx: &App) {
        let from = self.split_progress();
        let Some(s) = self.split.take() else { return };
        self.move_split(from, pane, s.ratio, ghost, cx);
    }

    /// A group's ×: one group from now on, its tabs joining the other's (after them, as they sat
    /// side by side); the other group's tab stays in front, and takes the focus.
    pub(crate) fn close_group(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.split_progress();
        let Some(s) = self.split.clone() else { return };
        let Some(front) = self.active_tab().map(|t| t.entity_id()) else { return };
        let (stay, leave) = if s.focus == pane { (s.other, front) } else { (front, s.other) };
        let ghost = self.tabs.iter().find(|t| t.entity_id() == leave).map(|t| t.downgrade());
        self.tabs = groups::merged(&self.tabs, |t| s.right.contains(&t.entity_id()));
        self.split = None;
        self.move_split(from, pane, s.ratio, ghost, cx);
        let Some(ix) = self.index_of(stay) else { return };
        if s.focus == pane {
            self.activate(ix, window, cx);
        } else {
            self.active = ix;
        }
        cx.notify();
    }

    /// Opens `tab` in the other group (the split opens with it when there is one group): a copy
    /// of a query tab, the tab itself when it is an activity monitor (one per connection).
    pub(crate) fn open_to_side(&mut self, tab: &Entity<QueryTab>, window: &mut Window, cx: &mut Context<Self>) {
        let id = tab.entity_id();
        let to = self.pane_of(id).other();
        if tab.read(cx).monitor.is_some() {
            return self.move_into(id, to, None, window, cx);
        }
        let (st, conn, want, favorite, table) = {
            let t = tab.read(cx);
            (t.session_state(cx), t.connection().cloned(), t.want.clone(), t.favorite.clone(), t.table.as_ref().map(|d| d.view.clone()))
        };
        self.open_in = Some(to);
        let copy = self.new_tab(window, cx);
        copy.update(cx, |t, cx| {
            t.set_connection(conn.clone(), cx);
            if conn.is_some()
                && let Some(db) = st.database.clone()
            {
                t.set_database(db, cx);
            }
            match table.filter(|_| conn.is_some()) {
                // A table's data view: the same table, its page read again in this tab.
                Some(view) => t.open_table(view, cx),
                None => t.restore(&st, cx),
            }
            t.favorite = favorite;
            t.want = want;
        });
        cx.notify();
    }

    /// Moves tab `id` into group `to`, at gap `slot` of that group's strip (`None`: after its last
    /// tab); it comes to the front there, with the focus. With one group, the right one opens with
    /// it (unless it is the only tab); the group it leaves empty folds away.
    pub(crate) fn move_into(&mut self, id: EntityId, to: Pane, slot: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if self.index_of(id).is_none() {
            return;
        }
        let from = self.pane_of(id);
        let front = self.active_tab().map(|t| t.entity_id());
        let from_front = self.front_of(from).map(|t| t.entity_id());
        let next = groups::neighbour(&self.group_ids(from), id);
        if from != to && next.is_none() && self.split.is_none() {
            // The only tab: nothing to split.
            return;
        }
        // The new order, by identity.
        let mut ids: Vec<EntityId> = self.tabs.iter().map(|t| t.entity_id()).collect();
        let split = self.split.clone();
        let in_to = |k: &EntityId| match &split {
            Some(s) => s.pane_of(*k) == to,
            None => to == Pane::Left,
        };
        let moved = groups::place(&mut ids, id, slot.unwrap_or(usize::MAX), in_to);
        if moved {
            let mut tabs = std::mem::take(&mut self.tabs);
            self.tabs = ids.iter().filter_map(|k| tabs.iter().position(|t| t.entity_id() == *k).map(|i| tabs.swap_remove(i))).collect();
        }
        // `active` follows its tab through the reorder.
        if let Some(ix) = front.and_then(|f| self.index_of(f)) {
            self.active = ix;
        }
        if from != to {
            // The front of the group it leaves, once it has left.
            let left_behind = if from_front == Some(id) { next } else { from_front };
            let mut emptied = false;
            match &mut self.split {
                None => {
                    if let Some(other) = left_behind {
                        self.open_split(Split { right: vec![id], focus: to, other, ratio: groups::RATIO }, cx);
                    }
                }
                Some(s) => {
                    if to == Pane::Right {
                        s.right.push(id);
                    } else {
                        s.right.retain(|k| *k != id);
                    }
                    match left_behind {
                        Some(other) => {
                            s.focus = to;
                            s.other = other;
                        }
                        None => emptied = true,
                    }
                }
            }
            if emptied {
                self.split_emptied(from, None, cx);
            }
            if let Some(ix) = self.index_of(id) {
                self.active = ix;
            }
        }
        if let Some(ix) = self.index_of(id) {
            self.activate(ix, window, cx);
        }
        cx.notify();
    }

    /// The seam dragged to `x` (window points): the left group's share, each group keeping
    /// [`PANE_MIN`] while there is room for both.
    pub(crate) fn resize_groups(&mut self, x: f32) {
        let c = self.frame_now.center;
        let room = (c.w - SEAM).max(1.);
        let lo = (PANE_MIN / room).max(groups::RATIO_MIN);
        let r = if lo >= 0.5 { 0.5 } else { ((x - c.x - SEAM / 2.) / room).clamp(lo, 1. - lo) };
        if let Some(s) = &mut self.split {
            s.ratio = r;
        }
    }

    /// The split this frame, asking for the next frame while it moves; `None`: one group, at
    /// rest. Called once per frame (`render`), after the docks are laid out.
    pub(crate) fn split_frame(&mut self, window: &mut Window) -> Option<SplitFrame> {
        let target = if self.split.is_some() { 1. } else { 0. };
        if let Some(m) = &self.split_move {
            match m.tween.at(target, Instant::now()) {
                Some(open) => {
                    window.request_animation_frame();
                    let ratio = self.split.as_ref().map_or(m.ratio, |s| s.ratio);
                    return Some(SplitFrame { open, leaving: m.leaving, ratio });
                }
                None => self.split_move = None,
            }
        }
        self.split.as_ref().map(|s| SplitFrame { open: 1., leaving: Pane::Right, ratio: s.ratio })
    }

    /// What group `pane` shows this frame: its tab in front, or — while the split folds — the
    /// tab the folding group showed (`None` once it was closed), and whether it has the focus.
    pub(crate) fn pane_view(&self, pane: Pane) -> (Option<Entity<QueryTab>>, bool) {
        match (&self.split, &self.split_move) {
            (Some(s), _) => (self.front_of(pane).cloned(), s.focus == pane),
            (None, Some(m)) if m.leaving == pane => (m.ghost.as_ref().and_then(|g| g.upgrade()).filter(|t| self.active_tab() != Some(t)), false),
            _ => (self.active_tab().cloned(), true),
        }
    }

    /// ⌃Tab / ⌃⇧Tab: the next or previous tab of the focused group.
    pub(crate) fn cycle_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let g = self.group(self.focused_pane());
        let Some(at) = g.iter().position(|&i| i == self.active) else { return };
        let n = g.len();
        self.activate(g[if forward { (at + 1) % n } else { (at + n - 1) % n }], window, cx);
    }

    /// ⌘1…⌘8: the focused group's tab `n` (counted from 0); `None` (⌘9): its last.
    pub(crate) fn activate_nth(&mut self, n: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let g = self.group(self.focused_pane());
        if let Some(&ix) = n.map_or(g.last(), |n| g.get(n)) {
            self.activate(ix, window, cx);
        }
    }

    /// The seam between the groups, over both (laid in the surface, `left_w` from its left edge):
    /// its grabber resizes them, a double-click shares the width evenly again.
    pub(crate) fn render_seam(&self, left_w: f32, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .id("group-seam")
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(left_w + SEAM / 2. - SEAM_HIT / 2.))
            .w(px(SEAM_HIT))
            .cursor_col_resize()
            .on_mouse_down(MouseButton::Left, cx.listener(|w, ev: &gpui::MouseDownEvent, _, cx| {
                cx.stop_propagation();
                if ev.click_count == 2 {
                    if let Some(s) = &mut w.split {
                        s.ratio = groups::RATIO;
                    }
                    cx.notify();
                } else {
                    w.drag = Some(Drag::Groups);
                }
            }))
    }

    /// The hairline between the groups through the title band (the strips' seam), `x` from the
    /// window's left edge.
    pub(crate) fn render_title_seam(&self, x: f32, cx: &App) -> impl IntoElement + use<> {
        let t = cx.theme();
        div().absolute().top_0().bottom_0().left(px(x)).w(px(SEAM)).bg(t.line)
    }
}

/// The groups' commands, on the workspace.
pub(crate) fn workspace_actions(d: gpui::Stateful<gpui::Div>, cx: &mut Context<Workspace>) -> gpui::Stateful<gpui::Div> {
    d.on_action(cx.listener(|w, _: &OpenToSide, window, cx| {
        if let Some(t) = w.active_tab().cloned() {
            w.open_to_side(&t, window, cx);
        }
    }))
    .on_action(cx.listener(|w, _: &MoveToOtherGroup, window, cx| {
        if let Some(id) = w.active_tab().map(|t| t.entity_id()) {
            let to = w.focused_pane().other();
            w.move_into(id, to, None, window, cx);
        }
    }))
    .on_action(cx.listener(|w, _: &FocusOtherGroup, window, cx| {
        let other = w.focused_pane().other();
        w.focus_pane(other, window, cx);
    }))
    .on_action(cx.listener(|w, _: &CloseGroup, window, cx| {
        let pane = w.focused_pane();
        w.close_group(pane, window, cx);
    }))
}
