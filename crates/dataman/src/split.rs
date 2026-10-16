//! Two editor groups side by side (VS Code's "Open to the Side"; the model is
//! `dataman_core::groups`): the work surface splits into a left and a right group, each with its
//! own strip of tabs (in the title bar, over its group) and its own tab in front. One group has
//! the focus — a click anywhere in a group, or the keyboard focus landing in it, gives it the
//! focus — and the workspace's `active` tab is always the focused group's: Run, ⌘D, find, the
//! result zoom, the history and the side panels, an agent's query all act there. The focused
//! group's tab in front is a wash over the strip; the other group's a lighter one (never a
//! line: the focus is not underlined). Each strip scrolls sideways on its own when its tabs
//! overflow (a trackpad, or the wheel turned into a sideways scroll), keeping the tab in front in
//! view; each group has its own cell viewer at its right (`cell_viewer`).
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
//! A tab dragged over a group's editor or results shows where it would land, VS Code's way:
//! over the other group, all of it lights up (dropped, the tab moves into it); over the only
//! group, its right or left third lights up its half (dropped, the tab opens a group of its own
//! on that side, growing in like Open to the Side) and its middle all of it (it stays). The
//! preview glides between the zones (`motion::RectTween`; at once with Reduce Motion).
//!
//! The split opens with the new group growing in from the right while the first narrows
//! (`motion::SPLIT`), and folds away the same way; with Reduce Motion it is simply there. A
//! group's × closes that group: its tabs join the other group (none is closed), whose tab stays
//! in front, whichever group had the focus. Closing a group's
//! last tab, or dragging it out, closes the split too. The seam between the groups resizes them;
//! a double-click on it shares the width evenly again.

use std::time::Instant;

use dataman_core::groups::{self, Pane};
use gpui::{
    AnyElement, App, Context, Div, Entity, EntityId, Focusable, InteractiveElement, IntoElement, MouseButton, Stateful, Styled, WeakEntity, Window, actions, div,
    relative,
};

use crate::app::{Drag, DraggedTab, Workspace};
use crate::motion::{RectTween, SplitTween};
use crate::tab::QueryTab;
use crate::theme::{ActiveTheme, Pt, px};

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
/// while it folds; gone when it was closed) with its cell viewer's width then (0: shut).
pub(crate) struct SplitMove {
    pub leaving: Pane,
    pub ratio: f32,
    pub tween: SplitTween,
    pub ghost: Option<WeakEntity<QueryTab>>,
    pub viewer: f32,
}

/// Where a tab dragged over a group's editor or results would land.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropZone {
    /// In that group (from the other one; over the only group, where it is already).
    Whole,
    /// A group of its own on that side of the only one.
    Side(Pane),
}

impl DropZone {
    /// Its preview, as shares of the group: left, top, right, bottom.
    pub fn rect(self) -> [f32; 4] {
        match self {
            DropZone::Whole => [0., 0., 1., 1.],
            DropZone::Side(Pane::Left) => [0., 0., 0.5, 1.],
            DropZone::Side(Pane::Right) => [0.5, 0., 1., 1.],
        }
    }
}

/// The drop preview gliding from one zone to another within a group, and where it was last
/// drawn.
pub(crate) struct DropGlide {
    pane: Pane,
    tween: RectTween,
    shown: [f32; 4],
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

    fn move_split(&mut self, from: f32, leaving: Pane, ratio: f32, ghost: Option<WeakEntity<QueryTab>>, viewer: f32, cx: &App) {
        // Reduce motion: the groups are simply there (or gone).
        self.split_move = (!cx.reduce_motion()).then(|| SplitMove { leaving, ratio, tween: SplitTween::new(from), ghost, viewer });
    }

    /// One group from now on, `survivor` the one that stays: what is the group's own (its cell
    /// viewer, its strip's scroll) is the left group's from now on.
    fn group_survives(&mut self, survivor: Pane) {
        self.viewer_survives(survivor);
        if survivor == Pane::Right {
            self.strip_scroll[0] = self.strip_scroll[1].clone();
        }
        self.strip_scroll[1] = gpui::ScrollHandle::new();
        for f in &self.strip_front {
            f.set(None);
        }
    }

    /// Two groups from now on: the right one grows in from the window's side.
    fn open_split(&mut self, s: Split, cx: &App) {
        let from = self.split_progress();
        let ratio = s.ratio;
        self.split = Some(s);
        self.move_split(from, Pane::Right, ratio, None, 0., cx);
    }

    /// Group `pane` has no tab left (its last one closed, or dragged out): one group from now on,
    /// the empty one folding away (`ghost`: the tab it showed, gone or now in the other group).
    pub(crate) fn split_emptied(&mut self, pane: Pane, ghost: Option<WeakEntity<QueryTab>>, cx: &App) {
        let from = self.split_progress();
        let Some(s) = self.split.take() else { return };
        let viewer = self.viewers[pane.index()].now();
        self.move_split(from, pane, s.ratio, ghost, viewer, cx);
        self.group_survives(pane.other());
    }

    /// A group's ×: group `pane` closes, one group from now on, its tabs joining the other's
    /// (after them, as they sat side by side). The other group's tab stays in front; it takes
    /// the focus when the closing group had it (else the keys stay where they are).
    pub(crate) fn close_group(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.split_progress();
        let Some(s) = self.split.clone() else { return };
        // Each group's tab in front, by group (whichever has the focus).
        let Some(stay) = self.front_of(pane.other()).map(|t| t.entity_id()) else { return };
        let ghost = self.front_of(pane).map(|t| t.downgrade());
        let front = self.active_tab().map(|t| t.entity_id());
        let viewer = self.viewers[pane.index()].now();
        self.tabs = groups::merged(&self.tabs, |t| s.right.contains(&t.entity_id()));
        // `active` follows its tab through the merge.
        if let Some(ix) = front.and_then(|f| self.index_of(f)) {
            self.active = ix;
        }
        self.split = None;
        self.move_split(from, pane, s.ratio, ghost, viewer, cx);
        self.group_survives(pane.other());
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

    /// Where tab `id` would land dropped at `pos` on group `pane`'s editor or results
    /// (`bounds`); `None`: nowhere (its own group beside another, the split moving).
    fn editor_zone(&self, id: EntityId, pane: Pane, pos: gpui::Point<gpui::Pixels>, bounds: gpui::Bounds<gpui::Pixels>) -> Option<DropZone> {
        if self.split_move.is_some() || self.index_of(id).is_none() {
            return None;
        }
        match &self.split {
            Some(s) => (s.pane_of(id) != pane).then_some(DropZone::Whole),
            // The only tab: nothing to split.
            None if self.tabs.len() < 2 => None,
            None => {
                let at = ((pos.x - bounds.left()).pt() / bounds.size.width.pt().max(1.)).clamp(0., 1.);
                Some(if at > 2. / 3. {
                    DropZone::Side(Pane::Right)
                } else if at < 1. / 3. {
                    DropZone::Side(Pane::Left)
                } else {
                    DropZone::Whole
                })
            }
        }
    }

    /// The preview over group `pane` points at `zone` now (`None`: not over it any more). A move
    /// between zones of the same group glides there.
    fn set_editor_drop(&mut self, pane: Pane, zone: Option<DropZone>, cx: &mut Context<Self>) {
        let was = self.editor_drop;
        let now = match zone {
            Some(z) => Some((pane, z)),
            // Off this group: its preview goes (another group's stays).
            None if was.is_some_and(|(p, _)| p == pane) => None,
            None => return,
        };
        if now == was {
            return;
        }
        self.editor_drop_glide = match (was, now) {
            (Some((p, a)), Some((q, b))) if p == q && a != b && !cx.reduce_motion() => {
                let from = self.editor_drop_glide.as_ref().filter(|g| g.pane == p).map_or(a.rect(), |g| g.shown);
                Some(DropGlide { pane: q, tween: RectTween::new(from), shown: from })
            }
            _ => None,
        };
        self.editor_drop = now;
        cx.notify();
    }

    /// A group's editor and results as a drop target for a dragged tab (`group`: the group's
    /// element, `relative`).
    pub(crate) fn editor_drop_target(&self, group: Stateful<Div>, pane: Pane, cx: &mut Context<Self>) -> Stateful<Div> {
        group
            .on_drag_move(cx.listener(move |w, ev: &gpui::DragMoveEvent<DraggedTab>, _, cx| {
                let pos = ev.event.position;
                let id = ev.dragged_item().downcast_ref::<DraggedTab>().map(|d| d.0);
                let zone = id.filter(|_| ev.bounds.contains(&pos)).and_then(|id| w.editor_zone(id, pane, pos, ev.bounds));
                w.set_editor_drop(pane, zone, cx);
            }))
            .on_drop(cx.listener(move |w, d: &DraggedTab, window, cx| w.drop_on_editor(d.0, pane, window, cx)))
    }

    /// Tab `id` dropped on group `pane`'s editor or results: into that group, or a group of its
    /// own beside the only one (the split opening as with Open to the Side).
    fn drop_on_editor(&mut self, id: EntityId, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        let zone = self.editor_drop.take().filter(|(p, _)| *p == pane).map(|(_, z)| z);
        self.editor_drop_glide = None;
        self.tab_drop = None;
        match zone {
            Some(DropZone::Whole) if self.split.is_some() => self.move_into(id, pane, None, window, cx),
            Some(DropZone::Side(Pane::Right)) => self.move_into(id, Pane::Right, None, window, cx),
            Some(DropZone::Side(Pane::Left)) => self.split_off_left(id, window, cx),
            _ => {
                if let Some(ix) = self.index_of(id) {
                    self.activate(ix, window, cx);
                }
            }
        }
        cx.notify();
    }

    /// Tab `id` in a group of its own at the left of the only one, which becomes the right group
    /// (its tab in front stays there): the left group grows in from the window's side.
    fn split_off_left(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        if self.split.is_some() {
            return;
        }
        let ids: Vec<EntityId> = self.tabs.iter().map(|t| t.entity_id()).collect();
        let front = self.active_tab().map(|t| t.entity_id());
        let Some(other) = (if front == Some(id) { groups::neighbour(&ids, id) } else { front }) else { return };
        let Some(ix) = self.index_of(id) else { return };
        // First in the list: the left group's strip reads as the two sit side by side.
        let tab = self.tabs.remove(ix);
        self.tabs.insert(0, tab);
        let from = self.split_progress();
        let right = ids.into_iter().filter(|k| *k != id).collect();
        self.split = Some(Split { right, focus: Pane::Left, other, ratio: groups::RATIO });
        self.move_split(from, Pane::Left, groups::RATIO, None, 0., cx);
        self.active = 0;
        self.activate(0, window, cx);
        cx.notify();
    }

    /// The preview over group `pane` of where the dragged tab would land: its zone lit, gliding
    /// from the one before.
    pub(crate) fn render_editor_drop(&mut self, pane: Pane, window: &mut Window, cx: &App) -> Option<AnyElement> {
        let (p, zone) = self.editor_drop?;
        if p != pane || !cx.has_active_drag() {
            return None;
        }
        let target = zone.rect();
        let r = match &mut self.editor_drop_glide {
            Some(g) if g.pane == pane => match g.tween.at(target, Instant::now()) {
                Some(r) => {
                    window.request_animation_frame();
                    g.shown = r;
                    r
                }
                None => target,
            },
            _ => target,
        };
        let t = cx.theme();
        Some(
            div()
                .absolute()
                .left(relative(r[0]))
                .top(relative(r[1]))
                .w(relative(r[2] - r[0]))
                .h(relative(r[3] - r[1]))
                .bg(t.acc.opacity(0.14))
                .border_1()
                .border_color(t.acc.opacity(0.6))
                .into_any_element(),
        )
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
