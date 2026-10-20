use std::time::{Duration, Instant};

use dataman_core::groups::{self, Pane};
use gpui::{
    AnyElement, App, Context, Div, Entity, EntityId, Focusable, InteractiveElement, IntoElement, MouseButton, ParentElement, Stateful, StatefulInteractiveElement, Styled, WeakEntity, Window, actions, div,
    relative,
};

use crate::app::{Drag, DraggedTab, Workspace};
use crate::motion::{RectTween, SplitTween};
use crate::tab::QueryTab;
use crate::theme::{ActiveTheme, Pt, px};

actions!(
    split,
    [
        OpenToSide,
        MoveToOtherGroup,
        FocusOtherGroup,
        CloseGroup,
    ]
);

pub const PANE_MIN: f32 = 260.;
pub const SEAM: f32 = 1.;
const SEAM_HIT: f32 = 8.;
const CLOSE_ASK: Duration = Duration::from_secs(3);

pub(crate) struct CloseArm {
    pub pane: Pane,
    _timeout: gpui::Task<()>,
    _esc: gpui::Subscription,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Split {
    pub right: Vec<EntityId>,
    pub focus: Pane,
    pub other: EntityId,
    pub ratio: f32,
}

impl Split {
    pub fn pane_of(&self, id: EntityId) -> Pane {
        if self.right.contains(&id) { Pane::Right } else { Pane::Left }
    }
}

pub(crate) struct SplitMove {
    pub leaving: Pane,
    pub ratio: f32,
    pub tween: SplitTween,
    pub ghost: Option<WeakEntity<QueryTab>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DropZone {
    Whole,
    Side(Pane),
}

impl DropZone {
    pub fn rect(self) -> [f32; 4] {
        match self {
            DropZone::Whole => [0., 0., 1., 1.],
            DropZone::Side(Pane::Left) => [0., 0., 0.5, 1.],
            DropZone::Side(Pane::Right) => [0.5, 0., 1., 1.],
        }
    }
}

pub(crate) struct DropGlide {
    pane: Pane,
    tween: RectTween,
    shown: [f32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SplitFrame {
    pub open: f32,
    pub leaving: Pane,
    pub ratio: f32,
}

impl SplitFrame {
    pub fn widths(&self, w: f32) -> ((f32, f32), (f32, f32)) {
        (groups::widths(w, self.ratio, SEAM, self.open, self.leaving), groups::widths(w, self.ratio, SEAM, 1., self.leaving))
    }
}

impl Workspace {
    pub(crate) fn pane_of(&self, id: EntityId) -> Pane {
        self.split.as_ref().map_or(Pane::Left, |s| s.pane_of(id))
    }

    pub(crate) fn focused_pane(&self) -> Pane {
        self.split.as_ref().map_or(Pane::Left, |s| s.focus)
    }

    pub(crate) fn group(&self, pane: Pane) -> Vec<usize> {
        (0..self.tabs.len()).filter(|&i| self.pane_of(self.tabs[i].entity_id()) == pane).collect()
    }

    fn group_ids(&self, pane: Pane) -> Vec<EntityId> {
        self.group(pane).into_iter().map(|i| self.tabs[i].entity_id()).collect()
    }

    fn index_of(&self, id: EntityId) -> Option<usize> {
        self.tabs.iter().position(|t| t.entity_id() == id)
    }

    pub(crate) fn front_of(&self, pane: Pane) -> Option<&Entity<QueryTab>> {
        match &self.split {
            Some(s) if s.focus != pane => self.tabs.iter().find(|t| t.entity_id() == s.other),
            None if pane == Pane::Right => None,
            _ => self.active_tab(),
        }
    }

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
            self.conn_menu = false;
            self.copy_menu = false;
        }
    }

    pub(crate) fn focus_pane(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = &self.split else { return };
        if s.focus == pane {
            return;
        }
        if let Some(ix) = self.index_of(s.other) {
            self.activate(ix, window, cx);
        }
    }

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
            self.sync_bound_views(window, cx);
            cx.notify();
        }
    }

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

    pub(crate) fn alt_aside(&self, window: &Window) -> Option<Pane> {
        (self.alt_press && window.modifiers().alt).then(|| self.focused_pane().other())
    }

    fn split_progress(&self) -> f32 {
        let target = if self.split.is_some() { 1. } else { 0. };
        self.split_move.as_ref().and_then(|m| m.tween.at(target, Instant::now())).unwrap_or(target)
    }

    fn move_split(&mut self, from: f32, leaving: Pane, ratio: f32, ghost: Option<WeakEntity<QueryTab>>, cx: &App) {
        self.split_move = (!cx.reduce_motion()).then(|| SplitMove { leaving, ratio, tween: SplitTween::new(from), ghost });
    }

    fn group_survives(&mut self, survivor: Pane) {
        if survivor == Pane::Right {
            self.strip_scroll[0] = self.strip_scroll[1].clone();
        }
        self.strip_scroll[1] = gpui::ScrollHandle::new();
        for f in &self.strip_front {
            f.set(None);
        }
    }

    fn open_split(&mut self, s: Split, cx: &App) {
        let from = self.split_progress();
        let ratio = s.ratio;
        self.split = Some(s);
        self.move_split(from, Pane::Right, ratio, None, cx);
    }

    pub(crate) fn split_emptied(&mut self, pane: Pane, ghost: Option<WeakEntity<QueryTab>>, cx: &App) {
        self.close_arm = None;
        let from = self.split_progress();
        let Some(s) = self.split.take() else { return };
        self.move_split(from, pane, s.ratio, ghost, cx);
        self.group_survives(pane.other());
    }

    pub(crate) fn ask_close_group(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        if self.split.is_none() {
            return;
        }
        let held = self.group(pane).into_iter().any(|i| self.tabs[i].read(cx).tx_open());
        if held || self.close_arm.as_ref().is_some_and(|a| a.pane == pane) {
            return self.close_group(pane, window, cx);
        }
        let timeout = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CLOSE_ASK).await;
            let _ = this.update(cx, |w, cx| w.let_go_close(cx));
        });
        // Esc lets it go and does nothing else: an interceptor runs before any binding and key
        // handler, wherever the keys are.
        let ws = cx.entity().downgrade();
        let esc = cx.intercept_keystrokes(move |ev, _, cx| {
            if ev.keystroke.key == "escape" {
                cx.stop_propagation();
                let _ = ws.update(cx, |w, cx| w.let_go_close(cx));
            }
        });
        self.close_arm = Some(CloseArm { pane, _timeout: timeout, _esc: esc });
        cx.notify();
    }

    pub(crate) fn let_go_close(&mut self, cx: &mut Context<Self>) {
        if self.close_arm.take().is_some() {
            cx.notify();
        }
    }

    pub(crate) fn close_group(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        self.close_arm = None;
        if self.split.is_none() {
            return;
        }
        let front = self.front_of(pane).cloned();
        let mut close: Vec<Entity<QueryTab>> = self.group(pane).into_iter().map(|i| self.tabs[i].clone()).filter(|t| Some(t) != front.as_ref()).collect();
        close.extend(front);
        self.close_tabs(close, window, cx);
        cx.notify();
    }

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
                Some(view) => t.open_table(view, cx),
                None => t.restore(&st, cx),
            }
            t.favorite = favorite;
            t.want = want;
        });
        cx.notify();
    }

    pub(crate) fn move_into(&mut self, id: EntityId, to: Pane, slot: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if self.index_of(id).is_none() {
            return;
        }
        let from = self.pane_of(id);
        let front = self.active_tab().map(|t| t.entity_id());
        let from_front = self.front_of(from).map(|t| t.entity_id());
        let next = groups::neighbour(&self.group_ids(from), id);
        if from != to && next.is_none() && self.split.is_none() {
            return;
        }
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
        if let Some(ix) = front.and_then(|f| self.index_of(f)) {
            self.active = ix;
        }
        if from != to {
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

    fn editor_zone(&self, id: EntityId, pane: Pane, pos: gpui::Point<gpui::Pixels>, bounds: gpui::Bounds<gpui::Pixels>) -> Option<DropZone> {
        if self.split_move.is_some() || self.index_of(id).is_none() {
            return None;
        }
        match &self.split {
            Some(s) => (s.pane_of(id) != pane).then_some(DropZone::Whole),
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

    fn set_editor_drop(&mut self, pane: Pane, zone: Option<DropZone>, cx: &mut Context<Self>) {
        let was = self.editor_drop;
        let now = match zone {
            Some(z) => Some((pane, z)),
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

    pub(crate) fn split_off_left(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        if self.split.is_some() {
            return;
        }
        let ids: Vec<EntityId> = self.tabs.iter().map(|t| t.entity_id()).collect();
        let front = self.active_tab().map(|t| t.entity_id());
        let Some(other) = (if front == Some(id) { groups::neighbour(&ids, id) } else { front }) else { return };
        let Some(ix) = self.index_of(id) else { return };
        let tab = self.tabs.remove(ix);
        self.tabs.insert(0, tab);
        let from = self.split_progress();
        let right = ids.into_iter().filter(|k| *k != id).collect();
        self.split = Some(Split { right, focus: Pane::Left, other, ratio: groups::RATIO });
        self.move_split(from, Pane::Left, groups::RATIO, None, cx);
        self.active = 0;
        self.activate(0, window, cx);
        cx.notify();
    }

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

    pub(crate) fn resize_groups(&mut self, x: f32) {
        let c = self.frame_now.center;
        let room = (c.w - SEAM).max(1.);
        let lo = (PANE_MIN / room).max(groups::RATIO_MIN);
        let r = if lo >= 0.5 { 0.5 } else { ((x - self.surface_x() - SEAM / 2.) / room).clamp(lo, 1. - lo) };
        if let Some(s) = &mut self.split {
            s.ratio = r;
        }
    }

    /// Called once per frame (`render`), after the docks are laid out.
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

    pub(crate) fn pane_view(&self, pane: Pane) -> (Option<Entity<QueryTab>>, bool) {
        match (&self.split, &self.split_move) {
            (Some(s), _) => (self.front_of(pane).cloned(), s.focus == pane),
            (None, Some(m)) if m.leaving == pane => (m.ghost.as_ref().and_then(|g| g.upgrade()).filter(|t| self.active_tab() != Some(t)), false),
            _ => (self.active_tab().cloned(), true),
        }
    }

    pub(crate) fn cycle_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let g = self.group(self.focused_pane());
        let Some(at) = g.iter().position(|&i| i == self.active) else { return };
        let n = g.len();
        self.activate(g[if forward { (at + 1) % n } else { (at + n - 1) % n }], window, cx);
    }

    pub(crate) fn activate_nth(&mut self, n: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let g = self.group(self.focused_pane());
        if let Some(&ix) = n.map_or(g.last(), |n| g.get(n)) {
            self.activate(ix, window, cx);
        }
    }

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

    /// The title bar paints it after the work column (over the band's hairline) and before the
    /// sheets, over the room the surface keeps for it as laid out this very frame (`pane_bounds`,
    /// recorded at prepaint): the band's part cannot stray from the rest by a pixel.
    pub(crate) fn render_seam_line(&self, cx: &App) -> Option<impl IntoElement + use<>> {
        self.split_now?;
        let color = cx.theme().line2;
        let parts = self.pane_bounds.clone();
        let painted = self.seam_line.clone();
        Some(
            gpui::canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    let Some(room) = parts.borrow().get(1).copied() else { return };
                    let b = gpui::Bounds::from_corners(gpui::point(room.left(), gpui::px(0.)), room.bottom_right());
                    window.paint_quad(gpui::fill(b, color));
                    painted.set(Some(b));
                },
            )
            .absolute()
            .size_0(),
        )
    }

    pub(crate) fn surface_x(&self) -> f32 {
        let c = self.frame_now.center;
        let beside = self.frame_now.dock(dataman_core::layout::Dock::Left).rect.w > 0.5;
        c.x + if beside { 1. / crate::theme::zoom() } else { 0. }
    }

    pub(crate) fn render_close_ask(&self, pane: Pane, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = cx.theme();
        let (hov, prs) = (t.err.opacity(0.85), t.err.opacity(0.7));
        let parts = self.strip_parts.clone();
        div()
            .id("close-group-ask")
            .flex_none()
            .h(px(20.))
            .px(px(7.))
            .flex()
            .items_center()
            .bg(t.err)
            .text_color(t.on_err)
            .text_size(px(11.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .whitespace_nowrap()
            .cursor_pointer()
            .hover(move |s| s.bg(hov))
            .active(move |s| s.bg(prs))
            .child("Sure?")
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |w, _, window, cx| w.close_group(pane, window, cx)))
            .on_hover(cx.listener(move |w, hovered: &bool, window, cx| {
                // GPUI reports a press held on it as the pointer leaving: only a pointer really
                // outside (the strip's last part, this) lets it go.
                let inside = parts.borrow()[pane.index()].last().is_some_and(|b| b.contains(&window.mouse_position()));
                if !*hovered && !inside {
                    w.let_go_close(cx);
                }
            }))
            .on_mouse_down_out(cx.listener(|w, _, _, cx| w.let_go_close(cx)))
    }
}

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
        w.ask_close_group(pane, window, cx);
    }))
}
