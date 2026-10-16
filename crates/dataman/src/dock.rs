//! The panel docks (`dataman_core::layout`): the explorer and the side panel each sit in the
//! left, right or bottom dock, and move by dragging the grip at the top of the panel (or from
//! the palette: "Move explorer to the right dock"…, "Reset panel layout").
//!
//! The geometry is computed here, once per frame, from the arrangement, the panels' sizes and
//! how open each one is ([`frame`]); `render` lays the docks out with exactly these sizes. A
//! drag's preview is the same computation for the arrangement the drop would make, so its
//! translucent box is the very area the panel will take. While a panel is dragged the pointer
//! picks a place ([`target_at`]): over another panel, the half it is in (above or below it; left
//! or right of it in the bottom dock); over the work surface, the band along its left, right or
//! bottom edge (that dock, at its end). On the drop the old place folds shut while the new one
//! opens (`motion::DOCK_MOVE`), the work surface and the other panels sliding along; with
//! Reduce Motion the panel is simply there.
//!
//! A panel keeps its own width in the left and right docks (`explorer_w`, `side_w`; a shared
//! dock is as wide as its widest panel); the bottom dock has one height (`bottom_h`).

use std::time::Instant;

use dataman_core::layout::{Dock, PanelId, PanelLayout, Place};
use gpui::{AnyElement, App, Context, DragMoveEvent, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, actions, div, prelude::FluentBuilder};

use crate::app::{Region, Side, Slide, Workspace};
use crate::motion::{self, Progress, RectTween};
use crate::theme::{ActiveTheme, Pt, Theme, metrics, px};
use crate::ui;

actions!(
    dock,
    [
        /// The explorer to the left dock (the default), the right one, or the bottom one.
        MoveExplorerLeft,
        MoveExplorerRight,
        MoveExplorerBottom,
        /// The side panel (history, cell viewer, server panels) to the left, right (the default)
        /// or bottom dock.
        MoveSidePanelLeft,
        MoveSidePanelRight,
        MoveSidePanelBottom,
        /// Every panel back in its home dock.
        ResetPanelLayout,
    ]
);

/// The bottom dock's height at first, and the least it can be resized to.
pub const BOTTOM_H: f32 = 260.;
pub const BOTTOM_MIN: f32 = 120.;
/// The work surface keeps at least this much height over the bottom dock, and this much width
/// between the left and right docks.
pub const CENTER_MIN_H: f32 = 180.;
pub const CENTER_MIN_W: f32 = 320.;
/// The grip's hit area, centred on a panel's top edge, and its bar.
const GRIP: (f32, f32) = (56., 10.);
const GRIP_BAR: (f32, f32) = (28., 3.);

/// A panel on the move (GPUI's drag; the ghost is `dnd`'s).
pub(crate) struct DraggedPanel(pub PanelId);

/// A rectangle in points, in window coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, (x, y): (f32, f32)) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    fn arr(self) -> [f32; 4] {
        [self.x, self.y, self.w, self.h]
    }

    fn of([x, y, w, h]: [f32; 4]) -> Rect {
        Rect { x, y, w, h }
    }
}

/// What takes room in a dock this frame: a panel, or the place a moving panel is leaving (a
/// `ghost`, drawn empty), with how much of its room it takes (0 shut … 1 open).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slot {
    pub panel: PanelId,
    pub ghost: bool,
    pub weight: f32,
}

/// The room the panels ask for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sizes {
    pub explorer_w: f32,
    pub side_w: f32,
    pub bottom_h: f32,
}

impl Sizes {
    /// `p`'s extent across dock `d`: its width at a side, the dock's height at the bottom.
    pub fn of(&self, p: PanelId, d: Dock) -> f32 {
        match (d.stacks(), p) {
            (false, _) => self.bottom_h,
            (true, PanelId::Explorer) => self.explorer_w,
            (true, PanelId::Side) => self.side_w,
        }
    }
}

/// A dock as laid out: its area, the extent its content keeps while it slides (the content is
/// clipped, never reflowed), and each slot's area.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DockFrame {
    pub rect: Rect,
    pub full: f32,
    pub slots: Vec<(Slot, Rect)>,
}

/// The window's docks and the work surface between them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    /// In `Dock::ALL` order.
    pub docks: [DockFrame; 3],
    pub center: Rect,
}

fn ix(d: Dock) -> usize {
    match d {
        Dock::Left => 0,
        Dock::Right => 1,
        Dock::Bottom => 2,
    }
}

impl Frame {
    pub fn dock(&self, d: Dock) -> &DockFrame {
        &self.docks[ix(d)]
    }

    /// Where panel `p` is (not the place it is leaving); `None` while it is shut.
    pub fn panel(&self, p: PanelId) -> Option<Rect> {
        self.docks.iter().flat_map(|d| &d.slots).find(|(s, _)| s.panel == p && !s.ghost).map(|(_, r)| *r)
    }
}

/// The slots of each dock (`Dock::ALL` order): the arrangement's panels, each as open as
/// `open` says, and those on the move (panel, the place it left, progress 0 → 1) growing in
/// their new place while a ghost shrinks in the old one.
pub fn slots(layout: &PanelLayout, open: impl Fn(PanelId) -> f32, moves: &[(PanelId, Place, f32)]) -> [Vec<Slot>; 3] {
    let mut out: [Vec<Slot>; 3] = Default::default();
    for d in Dock::ALL {
        for &p in layout.dock(d) {
            let p_move = moves.iter().find(|m| m.0 == p).map_or(1., |m| m.2);
            out[ix(d)].push(Slot { panel: p, ghost: false, weight: open(p).clamp(0., 1.) * p_move });
        }
    }
    for &(p, from, k) in moves {
        let list = &mut out[ix(from.dock)];
        let at = from.index.min(list.len());
        list.insert(at, Slot { panel: p, ghost: true, weight: open(p).clamp(0., 1.) * (1. - k) });
    }
    out
}

/// Lays out the docks in a window of `view` (points): the left dock from the title bar down to
/// the status bar at the window's left edge, the right one likewise at its right edge, the
/// bottom one under the work surface between them. A dock is as large as its most open panel
/// asks (each panel's extent times its weight); its panels share it in proportion to their
/// weights. The work surface keeps [`CENTER_MIN_W`] × [`CENTER_MIN_H`] when it can.
pub fn frame(view: (f32, f32), slots: [Vec<Slot>; 3], sizes: &Sizes) -> Frame {
    let (w, h) = view;
    let top = metrics::TITLE_H;
    let bottom = (h - metrics::STATUS_H).max(top);
    let body = bottom - top;
    let [l, r, b] = slots.map(|v| v.into_iter().filter(|s| s.weight > 1e-3).collect::<Vec<_>>());
    let extent = |d: Dock, v: &[Slot]| v.iter().map(|s| s.weight.min(1.) * sizes.of(s.panel, d)).fold(0., f32::max);
    let full = |d: Dock, v: &[Slot]| v.iter().map(|s| sizes.of(s.panel, d)).fold(0., f32::max);
    let (mut el, mut er) = (extent(Dock::Left, &l), extent(Dock::Right, &r));
    let room = (w - CENTER_MIN_W).max(0.);
    if el + er > room {
        let k = room / (el + er);
        el *= k;
        er *= k;
    }
    let max_b = (body - CENTER_MIN_H).max(0.);
    let eb = extent(Dock::Bottom, &b).min(max_b);
    let mid = (w - el - er).max(0.);
    let left = Rect { x: 0., y: top, w: el, h: body };
    let right = Rect { x: w - er, y: top, w: er, h: body };
    let under = Rect { x: el, y: bottom - eb, w: mid, h: eb };
    let center = Rect { x: el, y: top, w: mid, h: body - eb };
    // The slots share the dock along its other axis, in proportion to their weights.
    let split = |rect: Rect, v: Vec<Slot>, stacks: bool| -> Vec<(Slot, Rect)> {
        let total: f32 = v.iter().map(|s| s.weight).sum();
        let mut at = 0.;
        v.into_iter()
            .map(|s| {
                let share = if total > 0. { s.weight / total } else { 0. };
                let r = if stacks {
                    Rect { y: rect.y + at * rect.h, h: share * rect.h, ..rect }
                } else {
                    Rect { x: rect.x + at * rect.w, w: share * rect.w, ..rect }
                };
                at += share;
                (s, r)
            })
            .collect()
    };
    let docks = [
        DockFrame { rect: left, full: full(Dock::Left, &l), slots: split(left, l, true) },
        DockFrame { rect: right, full: full(Dock::Right, &r), slots: split(right, r, true) },
        DockFrame { rect: under, full: full(Dock::Bottom, &b).min(max_b), slots: split(under, b, false) },
    ];
    Frame { docks, center }
}

/// The place a panel dragged to `pos` would drop in (`None`: nowhere, or where it already is).
/// `frame` is the arrangement at rest.
pub fn target_at(frame: &Frame, layout: &PanelLayout, dragged: PanelId, pos: (f32, f32)) -> Option<Place> {
    let others = |d: Dock| layout.dock(d).iter().copied().filter(|p| *p != dragged).collect::<Vec<_>>();
    let mut place = None;
    'docks: for d in Dock::ALL {
        for (s, r) in &frame.dock(d).slots {
            if s.ghost || !r.contains(pos) {
                continue;
            }
            if s.panel == dragged {
                return None;
            }
            let i = others(d).iter().position(|p| *p == s.panel)?;
            let after = if d.stacks() { pos.1 > r.y + r.h / 2. } else { pos.0 > r.x + r.w / 2. };
            place = Some(Place { dock: d, index: i + after as usize });
            break 'docks;
        }
    }
    let place = match place {
        Some(p) => p,
        None => {
            // The bands along the work surface's edges: the nearest edge (relative to its band)
            // wins where they meet in a corner.
            let c = frame.center;
            if !c.contains(pos) {
                return None;
            }
            let bw = (c.w * 0.25).clamp(48., 220.);
            let bh = (c.h * 0.3).clamp(48., 220.);
            let near = [(Dock::Left, (pos.0 - c.x) / bw), (Dock::Right, (c.right() - pos.0) / bw), (Dock::Bottom, (c.bottom() - pos.1) / bh)];
            let (d, k) = near.into_iter().min_by(|a, b| a.1.total_cmp(&b.1))?;
            if k >= 1. {
                return None;
            }
            Place { dock: d, index: others(d).len() }
        }
    };
    (layout.moved(dragged, place) != *layout).then_some(place)
}

/// A panel settling into a new place: where it left from, and how far along it is.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DockMove {
    pub panel: PanelId,
    pub from: Place,
    pub progress: Progress,
}

/// A panel being dragged: the place the pointer picks (`None`: nowhere it could go), the area
/// it would take there, and the preview's box as last drawn (it glides to a new area).
#[derive(Clone, Debug)]
pub(crate) struct PanelDrag {
    pub panel: PanelId,
    pub target: Option<Place>,
    pub preview: Option<Rect>,
    pub shown: Option<Rect>,
    glide: Option<RectTween>,
}

/// This frame's docks: their geometry, the side view on screen, and how much of the side panel
/// the result zoom leaves (1: all of it).
pub(crate) struct DockPass {
    pub frame: Frame,
    pub side: Option<Side>,
    pub side_rest: f32,
}

/// The docks' elements, for `render` to place.
pub(crate) struct DockViews {
    pub left: Option<AnyElement>,
    pub right: Option<AnyElement>,
    pub bottom: Option<AnyElement>,
}

fn group(p: PanelId) -> SharedString {
    match p {
        PanelId::Explorer => "panel-explorer".into(),
        PanelId::Side => "panel-side".into(),
    }
}

impl Workspace {
    pub(crate) fn sizes(&self) -> Sizes {
        Sizes { explorer_w: self.explorer_w, side_w: self.side_w, bottom_h: self.bottom_h }
    }

    fn view_pt(window: &Window) -> (f32, f32) {
        let v = window.viewport_size();
        (v.width.pt(), v.height.pt())
    }

    /// `layout` at rest, with the panels open that are open now (what a drag measures): its
    /// hit test, and the area of each place it offers.
    pub(crate) fn rest_frame(&self, layout: &PanelLayout, window: &Window) -> Frame {
        let side = self.frame_now.panel(PanelId::Side).is_some();
        let open = |p: PanelId| match p {
            PanelId::Explorer => self.explorer_open as u8 as f32,
            PanelId::Side => side as u8 as f32,
        };
        frame(Self::view_pt(window), slots(layout, open, &[]), &self.sizes())
    }

    /// This frame's geometry, with the panels sliding open or shut, the result zoom folding the
    /// side panel away and panels settling into new places; kept in `frame_now` (the title bar
    /// and the checks read it). Asks for another frame while a panel moves. Called once per
    /// frame, before anything is laid out.
    pub(crate) fn dock_pass(&mut self, first_run: bool, window: &mut Window, cx: &App) -> DockPass {
        let now = Instant::now();
        // The history and the cell viewer slide in and out; the zoomed result covers the side
        // panel too, which folds away with the editor.
        let slide = self.side_slide(window, cx);
        let tab = self.active_tab().cloned();
        let rest = tab.as_ref().filter(|t| t.read(cx).has_run && t.read(cx).monitor.is_none()).map_or(1., |t| 1. - self.zoom_now(t, cx));
        let shown = if first_run || tab.is_none() { None } else { self.side.clone().or_else(|| slide.map(|(k, _)| k.side())).filter(|_| rest > 1e-3) };
        let side_open = shown.as_ref().map_or(0., |s| {
            let slides = slide.filter(|(k, _)| Slide::of(s) == Some(*k));
            slides.map_or(1., |(_, w)| w / self.side_w.max(1.)) * rest
        });
        let explorer_open = self.explorer_now() / self.explorer_w.max(1.);
        let mut moves = Vec::new();
        self.dock_moves.retain(|m| match m.progress.at(now) {
            Some(k) => {
                moves.push((m.panel, m.from, k));
                true
            }
            None => false,
        });
        if !moves.is_empty() {
            window.request_animation_frame();
        }
        let open = |p: PanelId| match p {
            PanelId::Explorer => explorer_open,
            PanelId::Side => side_open,
        };
        let frame = frame(Self::view_pt(window), slots(&self.layout, open, &moves), &self.sizes());
        self.frame_now = frame.clone();
        DockPass { frame, side: shown, side_rest: rest }
    }

    /// What the dragged panel is called on its ghost and on the preview.
    fn panel_name(&self, p: PanelId) -> &'static str {
        match (p, &self.side) {
            (PanelId::Explorer, _) => "Explorer",
            (PanelId::Side, Some(Side::History)) => "History",
            (PanelId::Side, Some(Side::Viewer)) => "Cell viewer",
            (PanelId::Side, Some(Side::Server(id, ..) | Side::Feature(id, ..))) => id,
            (PanelId::Side, None) => "Side panel",
        }
    }

    /// The docks as elements: each slot clips its panel, which keeps its full extent (anchored to
    /// the window's edge at the sides, to the top at the bottom) so it never reflows mid-slide.
    pub(crate) fn render_docks(&mut self, pass: &DockPass, window: &mut Window, cx: &mut Context<Self>) -> DockViews {
        let t = cx.theme();
        let region = self.focus_visible.then(|| self.focused_region(window, cx)).flatten();
        let moving = !self.dock_moves.is_empty();
        let mut views: [Option<AnyElement>; 3] = Default::default();
        // An empty dock lays out nothing: no stale bounds.
        *self.slot_bounds.borrow_mut() = Default::default();
        for d in Dock::ALL {
            let df = pass.frame.dock(d);
            if df.slots.is_empty() {
                continue;
            }
            let n = df.slots.len();
            let mut children = Vec::with_capacity(n);
            for (i, (s, r)) in df.slots.iter().enumerate() {
                let content: AnyElement = match (s.ghost, s.panel, &pass.side) {
                    (true, ..) => div().into_any_element(),
                    (false, PanelId::Explorer, _) => self.tree.clone().into_any_element(),
                    (false, PanelId::Side, Some(Side::History)) => self.history.clone().into_any_element(),
                    (false, PanelId::Side, Some(Side::Viewer)) => match self.active_tab().cloned() {
                        Some(tab) => self.render_viewer(&tab, &t, cx).into_any_element(),
                        None => div().into_any_element(),
                    },
                    (false, PanelId::Side, Some(Side::Server(_, v, _) | Side::Feature(_, v, _))) => v.clone().into_any_element(),
                    (false, PanelId::Side, None) => div().into_any_element(),
                };
                let side_focus = s.panel == PanelId::Side && !s.ghost && region == Some(Region::Side);
                children.push(self.render_slot(d, i, n, *s, *r, df.full, content, side_focus, pass.side_rest, &t));
            }
            let rect = df.rect;
            let bounds = self.slot_bounds.clone();
            let k = ix(d);
            let record = move |b: Vec<gpui::Bounds<gpui::Pixels>>, _: &mut Window, _: &mut App| bounds.borrow_mut()[k] = b;
            let el = match d {
                // On the glass, from the window's top edge (the title bar is laid over it).
                Dock::Left => div().w(px(rect.w)).flex_none().h_full().overflow_hidden().pt(px(metrics::TITLE_H)).flex().flex_col().on_children_prepainted(record).children(children),
                Dock::Right => div()
                    .relative()
                    .w(px(rect.w))
                    .flex_none()
                    .h_full()
                    .border_l_1()
                    .border_color(t.line)
                    .flex()
                    .flex_col()
                    .on_children_prepainted(record)
                    .children(children)
                    .when(!moving, |d| d.child(self.resize_seam(Dock::Right, cx))),
                Dock::Bottom => div()
                    .relative()
                    .h(px(rect.h))
                    .flex_none()
                    .w_full()
                    .border_t_1()
                    .border_color(t.line)
                    .flex()
                    .flex_row()
                    .on_children_prepainted(record)
                    .children(children)
                    .when(!moving, |d| d.child(self.resize_seam(Dock::Bottom, cx))),
            };
            views[ix(d)] = Some(el.into_any_element());
        }
        let [left, right, bottom] = views;
        DockViews { left, right, bottom }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_slot(&self, d: Dock, i: usize, n: usize, s: Slot, r: Rect, full: f32, content: AnyElement, focus: bool, side_rest: f32, t: &Theme) -> AnyElement {
        let last = i + 1 == n;
        let inner = match d {
            Dock::Left => div().absolute().top_0().bottom_0().left_0().w(px(full)),
            Dock::Right => div().absolute().top_0().bottom_0().right_0().w(px(full)),
            Dock::Bottom => div().absolute().left_0().right_0().top_0().h(px(full)),
        };
        // The one being dragged waits, dimmed, until it is dropped.
        let dragged = !s.ghost && self.panel_drag.as_ref().is_some_and(|g| g.panel == s.panel);
        let fade = if s.panel == PanelId::Side && !s.ghost { side_rest } else { 1. };
        let el = div().relative().flex_none().overflow_hidden().border_color(t.line).group(group(s.panel));
        // The last slot takes what is left (no gap from rounding).
        let el = if d.stacks() {
            el.w_full().when(!last, |e| e.h(px(r.h))).when(last, |e| e.flex_1().min_h_0()).when(i > 0, |e| e.border_t_1())
        } else {
            el.h_full().when(!last, |e| e.w(px(r.w))).when(last, |e| e.flex_1().min_w_0()).when(i > 0, |e| e.border_l_1())
        };
        el.child(inner.child(content))
            .when(fade < 1., |e| e.opacity(fade))
            .when(dragged, |e| e.opacity(0.4))
            .children(focus.then(|| crate::chrome::focus_ring(t)))
            .when(!s.ghost, |e| e.child(self.render_grip(s.panel, t)))
            .into_any_element()
    }

    /// The grip on a panel's top edge: a bar that shows while the panel is hovered; dragging it
    /// moves the panel.
    fn render_grip(&self, p: PanelId, t: &Theme) -> impl IntoElement + use<> {
        let label: SharedString = self.panel_name(p).into();
        let (line2, hov) = (t.line2, t.hov);
        let held = self.panel_drag.as_ref().is_some_and(|g| g.panel == p);
        div().absolute().top_0().left_0().right_0().h(px(GRIP.1)).flex().flex_row().justify_center().child(
            div()
                .id(("panel-grip", p as usize))
                .w(px(GRIP.0))
                .h_full()
                .flex()
                .flex_row()
                .justify_center()
                .pt(px(3.))
                .cursor_grab()
                .hover(move |s| s.bg(hov))
                .tooltip(ui::tooltip(format!("Drag to move the {}", label.to_lowercase()), None, None))
                // Not the panel's own press (a click in the tree, the history's selection).
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_drag(DraggedPanel(p), move |_, _, _, cx| crate::dnd::ghost(label.clone(), None, cx))
                .child(
                    div()
                        .w(px(GRIP_BAR.0))
                        .h(px(GRIP_BAR.1))
                        .when(held, |d| d.bg(line2))
                        .group_hover(group(p), move |s| s.bg(line2)),
                ),
        )
    }

    /// The resize grabber on a dock's inner edge: 8 pt straddling its hairline.
    fn resize_seam(&self, d: Dock, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let seam = div().id(("dock-resize", ix(d))).absolute();
        let seam = match d {
            Dock::Bottom => seam.left_0().right_0().top(px(-4.)).h(px(8.)).cursor_row_resize(),
            Dock::Right => seam.top_0().bottom_0().left(px(-4.)).w(px(8.)).cursor_col_resize(),
            Dock::Left => seam.top_0().bottom_0().right(px(-4.)).w(px(8.)).cursor_col_resize(),
        };
        seam.on_mouse_down(MouseButton::Left, cx.listener(move |w, _, _, cx| {
            w.drag = Some(crate::app::Drag::Dock(d));
            cx.stop_propagation();
        }))
    }

    /// A dock's seam dragged to `pos` (window points): its panels take the new extent, each
    /// within its own bounds.
    pub(crate) fn resize_dock(&mut self, d: Dock, pos: (f32, f32), window: &Window) {
        let (w, h) = Self::view_pt(window);
        match d {
            Dock::Left | Dock::Right => {
                let x = if d == Dock::Left { pos.0 } else { w - pos.0 };
                for p in self.layout.dock(d).to_vec() {
                    match p {
                        PanelId::Explorer if self.explorer_open => self.explorer_w = x.clamp(metrics::EXPLORER_MIN, metrics::EXPLORER_MAX),
                        PanelId::Side if self.side.is_some() => self.side_w = x.clamp(metrics::SIDE_MIN, metrics::SIDE_MAX),
                        _ => {}
                    }
                }
            }
            Dock::Bottom => {
                let body = h - metrics::STATUS_H - metrics::TITLE_H;
                self.bottom_h = (h - metrics::STATUS_H - pos.1).clamp(BOTTOM_MIN, (body - CENTER_MIN_H).max(BOTTOM_MIN));
            }
        }
    }

    /// A dragged panel moved: the place under the pointer and the area it would take there.
    pub(crate) fn panel_drag_moved(&mut self, ev: &DragMoveEvent<DraggedPanel>, window: &mut Window, cx: &mut Context<Self>) {
        let p = ev.drag(cx).0;
        let pos = (ev.event.position.x.pt(), ev.event.position.y.pt());
        let rest = self.rest_frame(&self.layout, window);
        let target = target_at(&rest, &self.layout, p, pos);
        let preview = target.and_then(|to| self.rest_frame(&self.layout.moved(p, to), window).panel(p));
        let reduced = cx.reduce_motion();
        let g = self.panel_drag.get_or_insert(PanelDrag { panel: p, target: None, preview: None, shown: None, glide: None });
        if g.panel != p || g.target != target || g.preview != preview {
            // From one place to the next the box glides; it appears where it first points.
            g.glide = match (g.shown, preview) {
                (Some(from), Some(_)) if !reduced && g.preview.is_some() => Some(RectTween::new(from.arr())),
                _ => None,
            };
            g.panel = p;
            g.target = target;
            g.preview = preview;
            cx.notify();
        }
    }

    /// The panel dropped: to the place the preview showed, if any.
    pub(crate) fn drop_panel(&mut self, p: PanelId, cx: &mut Context<Self>) {
        if let Some(to) = self.panel_drag.take().filter(|g| g.panel == p).and_then(|g| g.target) {
            self.move_panel(p, to, cx);
        }
        cx.notify();
    }

    pub(crate) fn move_panel(&mut self, p: PanelId, to: Place, cx: &mut Context<Self>) {
        let next = self.layout.moved(p, to);
        self.set_layout(next, Some(p), cx);
    }

    /// A new arrangement: each panel that changes dock (and `moved`, when it changes place in
    /// its own) settles into its new place from the old one.
    pub(crate) fn set_layout(&mut self, next: PanelLayout, moved: Option<PanelId>, cx: &mut Context<Self>) {
        if next == self.layout {
            return;
        }
        let old = std::mem::replace(&mut self.layout, next);
        for p in PanelId::ALL {
            let (a, b) = (old.place_of(p), self.layout.place_of(p));
            if a.dock != b.dock || (moved == Some(p) && a != b) {
                self.dock_moves.retain(|m| m.panel != p);
                if !cx.reduce_motion() {
                    self.dock_moves.push(DockMove { panel: p, from: a, progress: Progress::new(motion::DOCK_MOVE) });
                }
            }
        }
        cx.notify();
    }

    /// The palette's "Move … to the … dock": at that dock's end, shown if it was shut.
    pub(crate) fn move_panel_to(&mut self, p: PanelId, d: Dock, cx: &mut Context<Self>) {
        match p {
            PanelId::Explorer => self.set_explorer_open(true, cx),
            PanelId::Side if self.side.is_none() => self.side = Some(self.last_side.clone().unwrap_or(Side::History)),
            PanelId::Side => {}
        }
        if self.layout.place_of(p).dock == d {
            return cx.notify();
        }
        let index = self.layout.dock(d).iter().filter(|x| **x != p).count();
        self.move_panel(p, Place { dock: d, index }, cx);
        self.notice(format!("{} moved to the {} dock", self.panel_name(p), d.name()), cx);
    }

    /// Every panel back in its home dock, the bottom dock at its first height.
    pub(crate) fn reset_panel_layout(&mut self, cx: &mut Context<Self>) {
        self.set_layout(PanelLayout::default(), None, cx);
        self.bottom_h = BOTTOM_H;
        self.notice("Panel layout reset", cx);
    }

    /// The drop preview: a translucent box over exactly the area the dragged panel would take,
    /// gliding from one place to the next. Gone with the drag (dropped, or let go elsewhere).
    pub(crate) fn render_drop_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !cx.has_active_drag() {
            self.panel_drag = None;
            return None;
        }
        let name = self.panel_name(self.panel_drag.as_ref()?.panel);
        let g = self.panel_drag.as_mut()?;
        let Some(target) = g.preview else {
            g.shown = None;
            return None;
        };
        let now = g.glide.and_then(|tw| tw.at(target.arr(), Instant::now())).map(Rect::of);
        if now.is_some() {
            window.request_animation_frame();
        } else {
            g.glide = None;
        }
        let r = now.unwrap_or(target);
        g.shown = Some(r);
        let dock = g.target.map_or("", |p| p.dock.name());
        let t = cx.theme();
        Some(
            div()
                .absolute()
                .left(px(r.x))
                .top(px(r.y))
                .w(px(r.w))
                .h(px(r.h))
                .bg(t.acc.opacity(0.14))
                .border_1()
                .border_color(t.acc.opacity(0.6))
                .flex()
                .items_center()
                .justify_center()
                .overflow_hidden()
                // What lands there, on an opaque badge (the box is over the grid's text).
                .child(
                    div()
                        .px(px(8.))
                        .py(px(4.))
                        .bg(t.pop)
                        .border_1()
                        .border_color(t.acc.opacity(0.6))
                        .child(ui::caps(&t, format!("{name} · {dock}")).text_color(t.tx2)),
                )
                .into_any_element(),
        )
    }
}

/// The workspace's dock commands (the palette's entries).
pub(crate) fn workspace_actions(d: gpui::Stateful<gpui::Div>, cx: &mut Context<Workspace>) -> gpui::Stateful<gpui::Div> {
    use PanelId::{Explorer, Side as SidePanel};
    d.on_action(cx.listener(|w, _: &MoveExplorerLeft, _, cx| w.move_panel_to(Explorer, Dock::Left, cx)))
        .on_action(cx.listener(|w, _: &MoveExplorerRight, _, cx| w.move_panel_to(Explorer, Dock::Right, cx)))
        .on_action(cx.listener(|w, _: &MoveExplorerBottom, _, cx| w.move_panel_to(Explorer, Dock::Bottom, cx)))
        .on_action(cx.listener(|w, _: &MoveSidePanelLeft, _, cx| w.move_panel_to(SidePanel, Dock::Left, cx)))
        .on_action(cx.listener(|w, _: &MoveSidePanelRight, _, cx| w.move_panel_to(SidePanel, Dock::Right, cx)))
        .on_action(cx.listener(|w, _: &MoveSidePanelBottom, _, cx| w.move_panel_to(SidePanel, Dock::Bottom, cx)))
        .on_action(cx.listener(|w, _: &ResetPanelLayout, _, cx| w.reset_panel_layout(cx)))
        .on_drag_move(cx.listener(|w, ev: &DragMoveEvent<DraggedPanel>, window, cx| w.panel_drag_moved(ev, window, cx)))
        .on_drop(cx.listener(|w, d: &DraggedPanel, _, cx| w.drop_panel(d.0, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use PanelId::{Explorer, Side as S};

    const VIEW: (f32, f32) = (1400., 900.);
    const SIZES: Sizes = Sizes { explorer_w: 260., side_w: 360., bottom_h: 260. };

    fn rest(l: &PanelLayout, side: bool) -> Frame {
        frame(VIEW, slots(l, |p| if p == S && !side { 0. } else { 1. }, &[]), &SIZES)
    }

    fn near(a: Rect, b: Rect) -> bool {
        (a.x - b.x).abs() < 0.01 && (a.y - b.y).abs() < 0.01 && (a.w - b.w).abs() < 0.01 && (a.h - b.h).abs() < 0.01
    }

    /// The default arrangement: the explorer at the left from under the title bar to the status
    /// bar, the side panel at the right, the work surface between them.
    #[test]
    fn default_frame() {
        let f = rest(&PanelLayout::default(), true);
        let body = 900. - metrics::TITLE_H - metrics::STATUS_H;
        assert!(near(f.panel(Explorer).unwrap(), Rect { x: 0., y: metrics::TITLE_H, w: 260., h: body }));
        assert!(near(f.panel(S).unwrap(), Rect { x: 1040., y: metrics::TITLE_H, w: 360., h: body }));
        assert!(near(f.center, Rect { x: 260., y: metrics::TITLE_H, w: 780., h: body }));
        // Shut: no room at all.
        let f = rest(&PanelLayout::default(), false);
        assert_eq!(f.panel(S), None);
        assert_eq!(f.dock(Dock::Right).rect.w, 0.);
        assert_eq!(f.center.w, 1140.);
    }

    /// Two panels in one dock share it (stacked at a side, side by side at the bottom); the
    /// dock is as wide as the wider one; the bottom dock sits under the work surface only.
    #[test]
    fn shared_docks_and_the_bottom() {
        let l = PanelLayout::default().moved(Explorer, Place { dock: Dock::Right, index: 0 });
        let f = rest(&l, true);
        let (e, s) = (f.panel(Explorer).unwrap(), f.panel(S).unwrap());
        assert_eq!((e.x, e.w, s.w), (1040., 360., 360.));
        assert!((e.h - s.h).abs() < 0.01 && (e.bottom() - s.y).abs() < 0.01, "{e:?} {s:?}");
        assert_eq!(f.dock(Dock::Left).rect.w, 0.);
        let l = PanelLayout::default().moved(S, Place { dock: Dock::Bottom, index: 0 });
        let f = rest(&l, true);
        let s = f.panel(S).unwrap();
        assert_eq!((s.x, s.w, s.h), (260., 1140., 260.));
        assert!((s.bottom() - (900. - metrics::STATUS_H)).abs() < 0.01);
        assert!((f.center.bottom() - s.y).abs() < 0.01);
        // Too tall for the window: the work surface keeps its least height.
        let tall = frame(VIEW, slots(&l, |_| 1., &[]), &Sizes { bottom_h: 2000., ..SIZES });
        assert!((tall.center.h - CENTER_MIN_H).abs() < 0.01);
    }

    /// A move half-way: the old place half shut, the new one half open, nothing else in the
    /// way; done, it is exactly the arrangement at rest.
    #[test]
    fn a_move_folds_one_place_and_opens_the_other() {
        let from = Place { dock: Dock::Left, index: 0 };
        let l = PanelLayout::default().moved(Explorer, Place { dock: Dock::Bottom, index: 0 });
        let half = frame(VIEW, slots(&l, |_| 1., &[(Explorer, from, 0.5)]), &SIZES);
        assert_eq!(half.dock(Dock::Left).rect.w, 130.);
        assert_eq!(half.dock(Dock::Bottom).rect.h, 130.);
        assert!(half.dock(Dock::Left).slots[0].0.ghost);
        assert_eq!(half.dock(Dock::Left).full, 260., "the content keeps its width while it folds");
        let done = frame(VIEW, slots(&l, |_| 1., &[(Explorer, from, 1.)]), &SIZES);
        assert_eq!(done, rest(&l, true));
        // Within one dock the ghost holds the old slot: [ghost, side, explorer].
        let r = PanelLayout::default().moved(Explorer, Place { dock: Dock::Right, index: 0 });
        let swapped = r.moved(Explorer, Place { dock: Dock::Right, index: 1 });
        let s = slots(&swapped, |_| 1., &[(Explorer, Place { dock: Dock::Right, index: 0 }, 0.25)]);
        let order: Vec<(PanelId, bool)> = s[1].iter().map(|s| (s.panel, s.ghost)).collect();
        assert_eq!(order, [(Explorer, true), (S, false), (Explorer, false)]);
    }

    /// Where a drop lands: the halves of another panel, the bands along the work surface's
    /// edges; nothing over the panel itself, the middle of the work surface, or the place it
    /// already has.
    #[test]
    fn drop_targets() {
        let l = PanelLayout::default();
        let f = rest(&l, true);
        let top = metrics::TITLE_H;
        // Over the side panel: above it or below it.
        assert_eq!(target_at(&f, &l, Explorer, (1200., top + 100.)), Some(Place { dock: Dock::Right, index: 0 }));
        assert_eq!(target_at(&f, &l, Explorer, (1200., 800.)), Some(Place { dock: Dock::Right, index: 1 }));
        // Its own place, and the band of its own dock (where it already is): nowhere.
        assert_eq!(target_at(&f, &l, Explorer, (100., 300.)), None);
        assert_eq!(target_at(&f, &l, Explorer, (300., 300.)), None);
        // The middle of the work surface: nowhere.
        assert_eq!(target_at(&f, &l, Explorer, (650., 400.)), None);
        // The bottom band, the right band (the right dock's end, under the side panel).
        assert_eq!(target_at(&f, &l, Explorer, (650., 800.)), Some(Place { dock: Dock::Bottom, index: 0 }));
        assert_eq!(target_at(&f, &l, Explorer, (1000., 400.)), Some(Place { dock: Dock::Right, index: 1 }));
        // The side panel to the left band: under the explorer.
        assert_eq!(target_at(&f, &l, S, (300., 400.)), Some(Place { dock: Dock::Left, index: 1 }));
        // In the bottom dock the halves are left and right.
        let b = l.moved(S, Place { dock: Dock::Bottom, index: 0 });
        let fb = rest(&b, true);
        let s = fb.panel(S).unwrap();
        assert_eq!(target_at(&fb, &b, Explorer, (s.x + 10., s.y + 50.)), Some(Place { dock: Dock::Bottom, index: 0 }));
        assert_eq!(target_at(&fb, &b, Explorer, (s.right() - 10., s.y + 50.)), Some(Place { dock: Dock::Bottom, index: 1 }));
        // Outside everything (the title bar): nowhere.
        assert_eq!(target_at(&f, &l, Explorer, (650., 10.)), None);
    }

    /// The preview is the area the panel takes once dropped there.
    #[test]
    fn the_preview_is_the_landing_area() {
        let l = PanelLayout::default();
        let f = rest(&l, true);
        let to = target_at(&f, &l, Explorer, (650., 800.)).unwrap();
        let after = l.moved(Explorer, to);
        let preview = rest(&after, true).panel(Explorer).unwrap();
        // Dropped: the left dock is gone, the bottom dock runs from the window's left edge to
        // the side panel.
        assert_eq!((preview.x, preview.w, preview.h), (0., 1040., 260.));
    }
}
