use std::time::Instant;

use dataman_core::layout::{Dock, PanelId, PanelLayout, Place};
use gpui::{AnyElement, App, Context, DragMoveEvent, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString, StatefulInteractiveElement, Styled, Window, actions, div, prelude::FluentBuilder};

use crate::app::{Drag, Side, Workspace};
use crate::motion::{self, Progress, RectTween};
use crate::theme::{ActiveTheme, Pt, Theme, metrics, px};
use crate::ui;

actions!(
    dock,
    [
        MoveExplorerLeft,
        MoveExplorerRight,
        ResetPanelLayout,
    ]
);

pub const CENTER_MIN_W: f32 = 320.;
const GRIP: (f32, f32) = (56., 10.);
const GRIP_DOT: f32 = 6.;
const SEAM_HIT: f32 = 8.;
const HEAD_H: f32 = 34.;

pub(crate) struct DraggedPanel(pub PanelId);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slot {
    pub panel: PanelId,
    pub ghost: bool,
    pub weight: f32,
    pub share: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sizes {
    pub explorer_w: f32,
    pub side_w: f32,
}

impl Sizes {
    pub fn of(&self, p: PanelId) -> f32 {
        match p {
            PanelId::Explorer => self.explorer_w,
            PanelId::Side => self.side_w,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColFrame {
    pub rect: Rect,
    pub full: f32,
    pub slots: Vec<(Slot, Rect)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DockFrame {
    pub rect: Rect,
    pub cols: Vec<ColFrame>,
}

impl DockFrame {
    pub fn slots(&self) -> impl Iterator<Item = &(Slot, Rect)> {
        self.cols.iter().flat_map(|c| &c.slots)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    pub docks: [DockFrame; 2],
    pub center: Rect,
}

pub(crate) fn ix(d: Dock) -> usize {
    match d {
        Dock::Left => 0,
        Dock::Right => 1,
    }
}

impl Frame {
    pub fn dock(&self, d: Dock) -> &DockFrame {
        &self.docks[ix(d)]
    }

    pub fn panel(&self, p: PanelId) -> Option<Rect> {
        self.docks.iter().flat_map(|d| d.slots()).find(|(s, _)| s.panel == p && !s.ghost).map(|(_, r)| *r)
    }
}

pub fn slots(layout: &PanelLayout, open: impl Fn(PanelId) -> f32, moves: &[(PanelId, Place, f32)]) -> [Vec<Vec<Slot>>; 2] {
    let mut out: [Vec<Vec<Slot>>; 2] = Default::default();
    for d in Dock::ALL {
        for c in layout.dock(d) {
            let col = c.panels.iter().enumerate().map(|(i, &p)| {
                let k = moves.iter().find(|m| m.0 == p).map_or(1., |m| m.2);
                Slot { panel: p, ghost: false, weight: open(p).clamp(0., 1.) * k, share: c.share(i) }
            });
            out[ix(d)].push(col.collect());
        }
    }
    for &(p, from, k) in moves {
        let cols = &mut out[ix(from.dock)];
        let weight = open(p).clamp(0., 1.) * (1. - k);
        match from.row.filter(|_| from.col < cols.len()) {
            Some(r) => {
                let c = &mut cols[from.col];
                let share = c.iter().map(|s| s.share).sum::<f32>() / c.len().max(1) as f32;
                c.insert(r.min(c.len()), Slot { panel: p, ghost: true, weight, share });
            }
            None => cols.insert(from.col.min(cols.len()), vec![Slot { panel: p, ghost: true, weight, share: 1. }]),
        }
    }
    out
}

pub fn ghost_place(old: &PanelLayout, new: &PanelLayout, p: PanelId) -> Place {
    let from = old.place_of(p);
    let cols = old.dock(from.dock);
    let now = |q: &PanelId| new.locate(*q).filter(|(d, ..)| *d == from.dock).map(|(_, c, _)| c);
    let mates = cols.get(from.col).and_then(|c| c.panels.iter().filter(|q| **q != p).find_map(now));
    match (from.row, mates) {
        (Some(r), Some(c)) => Place::stack(from.dock, c, r),
        _ => {
            let left = from.col.checked_sub(1).and_then(|c| cols.get(c)).and_then(|c| c.panels.iter().filter(|q| **q != p).find_map(now));
            Place::column(from.dock, left.map_or(0, |c| c + 1))
        }
    }
}

pub fn frame(view: (f32, f32), slots: [Vec<Vec<Slot>>; 2], sizes: &Sizes) -> Frame {
    let (w, h) = view;
    let top = metrics::TITLE_H;
    let body = (h - metrics::STATUS_H).max(top) - top;
    let cols = slots.map(|d| d.into_iter().map(|c| c.into_iter().filter(|s| s.weight > 1e-3).collect::<Vec<_>>()).filter(|c| !c.is_empty()).collect::<Vec<_>>());
    let extent = |c: &[Slot]| c.iter().map(|s| s.weight.min(1.) * sizes.of(s.panel)).fold(0., f32::max);
    let full = |c: &[Slot]| c.iter().map(|s| sizes.of(s.panel)).fold(0., f32::max);
    let mut widths = cols.each_ref().map(|d| d.iter().map(|c| extent(c)).collect::<Vec<f32>>());
    let total: f32 = widths.iter().flatten().sum();
    let room = (w - CENTER_MIN_W).max(0.);
    if total > room {
        let k = room / total;
        widths.iter_mut().flatten().for_each(|v| *v *= k);
    }
    let (el, er) = (widths[0].iter().sum::<f32>(), widths[1].iter().sum::<f32>());
    let left = Rect { x: 0., y: top, w: el, h: body };
    let right = Rect { x: w - er, y: top, w: er, h: body };
    let center = Rect { x: el, y: top, w: (w - el - er).max(0.), h: body };
    let lay = |rect: Rect, cols: Vec<Vec<Slot>>, widths: &[f32]| {
        let mut x = rect.x;
        let cols = cols
            .into_iter()
            .zip(widths)
            .map(|(c, &cw)| {
                let r = Rect { x, w: cw, ..rect };
                x += cw;
                let full = full(&c);
                let total: f32 = c.iter().map(|s| s.weight * s.share).sum();
                let mut at = 0.;
                let slots = c
                    .into_iter()
                    .map(|s| {
                        let k = if total > 0. { s.weight * s.share / total } else { 0. };
                        let sr = Rect { y: r.y + at * r.h, h: k * r.h, ..r };
                        at += k;
                        (s, sr)
                    })
                    .collect();
                ColFrame { rect: r, full, slots }
            })
            .collect();
        DockFrame { rect, cols }
    };
    let [lc, rc] = cols;
    Frame { docks: [lay(left, lc, &widths[0]), lay(right, rc, &widths[1])], center }
}

pub fn target_at(frame: &Frame, layout: &PanelLayout, dragged: PanelId, pos: (f32, f32)) -> Option<Place> {
    if dragged != PanelId::Explorer {
        return None;
    }
    let body = |r: Rect| pos.1 >= r.y && pos.1 <= r.bottom();
    if !body(frame.center) {
        return None;
    }
    let mid = frame.center.x + frame.center.w / 2.;
    let to = if pos.0 < mid { Dock::Left } else { Dock::Right };
    (to != layout.explorer_dock()).then(|| Place::column(to, 0))
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DockMove {
    pub panel: PanelId,
    pub from: Place,
    pub progress: Progress,
}

#[derive(Clone, Debug)]
pub(crate) struct PanelDrag {
    pub panel: PanelId,
    pub target: Option<Place>,
    pub preview: Option<Rect>,
    pub shown: Option<Rect>,
    glide: Option<RectTween>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ExplorerBlend {
    from: [f32; 2],
    progress: Option<Progress>,
}

impl ExplorerBlend {
    fn target(layout: &PanelLayout) -> [f32; 2] {
        let d = layout.place_of(PanelId::Explorer).dock;
        Dock::ALL.map(|x| (x == d) as u8 as f32)
    }

    fn at(&mut self, layout: &PanelLayout, now: Instant) -> ([f32; 2], bool) {
        let to = Self::target(layout);
        match self.progress.and_then(|p| p.at(now)) {
            Some(k) => (std::array::from_fn(|i| self.from[i] + (to[i] - self.from[i]) * k), true),
            None => {
                self.progress = None;
                (to, false)
            }
        }
    }

    fn start(&mut self, old: &PanelLayout, new: &PanelLayout, reduced: bool) {
        if Self::target(old) == Self::target(new) {
            return;
        }
        let (now, _) = self.at(old, Instant::now());
        self.from = now;
        self.progress = (!reduced).then(|| Progress::new(motion::DOCK_MOVE));
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TitleFrame {
    pub header: Option<Dock>,
    pub lead: f32,
    pub seam: f32,
    pub trail: f32,
    pub joined: [f32; 2],
}

pub const HEADER_MIN: f32 = 92.;
pub const SEARCH_MIN: f32 = 96.;

pub fn title_frame(frame: &Frame, layout: &PanelLayout, joined: [f32; 2], lights: f32) -> TitleFrame {
    let header = layout.place_of(PanelId::Explorer).dock;
    let explorer = |c: &ColFrame| c.slots.iter().any(|(s, _)| s.panel == PanelId::Explorer);
    let k = |c: &ColFrame| if explorer(c) { 1. } else { 1. - joined[ix(Dock::Left)] };
    let left = &frame.dock(Dock::Left).cols;
    let edge: f32 = left.iter().map(|c| c.rect.w * k(c)).sum();
    let full: f32 = left.iter().map(|c| c.full * k(c)).sum();
    let lead = edge.max(lights + if header == Dock::Left { HEADER_MIN } else { SEARCH_MIN });
    let trail = match header {
        Dock::Right => frame.dock(Dock::Right).cols.iter().filter(|c| c.slots.iter().any(|(s, _)| s.panel == PanelId::Explorer && !s.ghost)).map(|c| c.rect.w).sum::<f32>().max(HEADER_MIN),
        Dock::Left => 0.,
    };
    TitleFrame { header: Some(header), lead, seam: (edge / full.max(1.)).clamp(0., 1.), trail, joined }
}

pub(crate) struct DockPass {
    pub frame: Frame,
    pub side: Option<Side>,
    pub side_rest: f32,
    pub title: TitleFrame,
}

impl DockPass {
    pub fn band_left(&self) -> bool {
        self.title.joined[ix(Dock::Left)] > 0. && self.frame.dock(Dock::Left).cols.last().is_some_and(|c| !c.slots.iter().any(|(s, _)| s.panel == PanelId::Explorer))
    }
}

pub(crate) struct DockViews {
    pub left: Option<AnyElement>,
    pub right: Option<AnyElement>,
}

fn group(p: PanelId) -> SharedString {
    match p {
        PanelId::Explorer => "panel-explorer".into(),
        PanelId::Side => "panel-side".into(),
    }
}

impl Workspace {
    pub(crate) fn sizes(&self) -> Sizes {
        Sizes { explorer_w: self.explorer_w, side_w: self.side_w }
    }

    fn view_pt(window: &Window) -> (f32, f32) {
        let v = window.viewport_size();
        (v.width.pt(), v.height.pt())
    }

    pub(crate) fn rest_frame(&self, layout: &PanelLayout, window: &Window) -> Frame {
        let side = self.frame_now.panel(PanelId::Side).is_some();
        let open = |p: PanelId| match p {
            PanelId::Explorer => self.explorer_open as u8 as f32,
            PanelId::Side => side as u8 as f32,
        };
        frame(Self::view_pt(window), slots(layout, open, &[]), &self.sizes())
    }

    /// Called once per frame, before anything is laid out.
    pub(crate) fn dock_pass(&mut self, first_run: bool, window: &mut Window, cx: &App) -> DockPass {
        let now = Instant::now();
        let slide = self.side_slide(window, cx);
        let tab = self.active_tab().cloned();
        let rest = tab.as_ref().filter(|t| t.read(cx).has_run && t.read(cx).monitor.is_none()).map_or(1., |t| 1. - self.zoom_now(t, cx));
        if self.side.is_some() {
            self.side_seen = self.side.clone();
        }
        let shown = if first_run || tab.is_none() { None } else { self.side.clone().or_else(|| slide.and(self.side_seen.clone())).filter(|_| rest > 1e-3) };
        let side_open = match (&shown, slide) {
            (None, _) => 0.,
            (Some(_), Some(w)) => w / self.side_w.max(1.) * rest,
            (Some(_), None) => rest,
        };
        let explorer_open = self.explorer_now() / self.explorer_w.max(1.);
        let mut moves = Vec::new();
        self.dock_moves.retain(|m| match m.progress.at(now) {
            Some(k) => {
                moves.push((m.panel, m.from, k));
                true
            }
            None => false,
        });
        let (joined, blending) = self.explorer_blend.at(&self.layout, now);
        if !moves.is_empty() || blending {
            window.request_animation_frame();
        }
        let open = |p: PanelId| match p {
            PanelId::Explorer => explorer_open,
            PanelId::Side => side_open,
        };
        let frame = frame(Self::view_pt(window), slots(&self.layout, open, &moves), &self.sizes());
        let title = title_frame(&frame, &self.layout, joined, crate::csd::lead(window, cx));
        self.frame_now = frame.clone();
        self.title_now = title;
        DockPass { frame, side: shown, side_rest: rest, title }
    }

    fn panel_name(&self, p: PanelId) -> &'static str {
        match (p, &self.side) {
            (PanelId::Explorer, _) => "Sidebar",
            (PanelId::Side, Some(Side::Server(id, ..) | Side::Feature(id, ..))) => id,
            (PanelId::Side, None) => "Side panel",
        }
    }

    pub(crate) fn render_docks(&mut self, pass: &DockPass, production: bool, window: &mut Window, cx: &mut Context<Self>) -> DockViews {
        let t = cx.theme();
        let mut views: [Option<AnyElement>; 2] = Default::default();
        *self.slot_bounds.borrow_mut() = pass.frame.docks.each_ref().map(|d| vec![Vec::new(); d.cols.len()]);
        for d in Dock::ALL {
            let df = pass.frame.dock(d);
            if df.cols.is_empty() {
                continue;
            }
            let ncols = df.cols.len();
            let mut cols = Vec::with_capacity(ncols);
            let joined = pass.title.joined[ix(d)];
            for (ci, col) in df.cols.iter().enumerate() {
                let n = col.slots.len();
                let glass = col.slots.iter().any(|(s, _)| s.panel == PanelId::Explorer);
                let fill = (!glass).then_some(t.panel);
                let band = !glass && (d == Dock::Right || joined > 0.);
                let mut children = Vec::with_capacity(n);
                for (i, (s, r)) in col.slots.iter().enumerate() {
                    let content: AnyElement = match (s.ghost, s.panel, &pass.side) {
                        (true, ..) => div().into_any_element(),
                        (false, PanelId::Explorer, _) => self.render_sidebar(d, window, cx),
                        (false, PanelId::Side, Some(Side::Server(_, v, _) | Side::Feature(_, v, _))) => v.clone().into_any_element(),
                        (false, PanelId::Side, None) => div().into_any_element(),
                    };
                    children.push(self.render_slot(d, i, n, *s, *r, col.full, content, pass.side_rest, &t, cx));
                }
                let bounds = self.slot_bounds.clone();
                let k = ix(d);
                let record = move |b: Vec<gpui::Bounds<gpui::Pixels>>, _: &mut Window, _: &mut App| {
                    if let Some(c) = bounds.borrow_mut()[k].get_mut(ci) {
                        *c = b;
                    }
                };
                // The last column takes what is left (no gap from rounding).
                let last = ci + 1 == ncols;
                let seam_l = ci > 0 || (d == Dock::Right && !band);
                let body_l = ci == 0 && d == Dock::Right && band;
                let body_r = last && d == Dock::Left && band;
                let body = div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .border_color(t.line)
                    .when_some(fill, |b, f| b.bg(f))
                    .when(!glass && !production, |b| b.border_t_1())
                    .when(!glass && production, |b| b.border_t_2().border_color(t.err))
                    .on_children_prepainted(record)
                    .children(children);
                cols.push(
                    div()
                        .relative()
                        .h_full()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .border_color(t.line)
                        .when(!last, |c| c.w(px(col.rect.w)))
                        .when(last, |c| c.flex_1().min_w_0())
                        .when(seam_l, |c| c.border_l_1())
                        .child(div().h(px(metrics::TITLE_H)).flex_none().when(!glass, |b| b.bg(t.panel)))
                        .child(body)
                        .when(body_l || body_r, |c| {
                            let line = div().absolute().top(px(metrics::TITLE_H)).bottom_0().w(px(1.)).bg(t.line);
                            c.child(if body_l { line.left_0() } else { line.right_0() })
                        }),
                );
            }
            let rect = df.rect;
            let el = div().w(px(rect.w)).flex_none().h_full().overflow_hidden().flex().flex_row().children(cols);
            views[ix(d)] = Some(el.into_any_element());
        }
        let [left, right] = views;
        DockViews { left, right }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_slot(&self, d: Dock, i: usize, n: usize, s: Slot, r: Rect, full: f32, content: AnyElement, side_rest: f32, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let last = i + 1 == n;
        let inner = match d {
            Dock::Left => div().absolute().top_0().bottom_0().left_0().w(px(full)),
            Dock::Right => div().absolute().top_0().bottom_0().right_0().w(px(full)),
        };
        let dragged = !s.ghost && self.panel_drag.as_ref().is_some_and(|g| g.panel == s.panel);
        let fade = if s.panel == PanelId::Side && !s.ghost { side_rest } else { 1. };
        let p = s.panel;
        let head = r.y + HEAD_H;
        div()
            .relative()
            .flex_none()
            .overflow_hidden()
            .border_color(t.line)
            .group(group(p))
            .w_full()
            // The last slot takes what is left (no gap from rounding).
            .when(!last, |e| e.h(px(r.h)))
            .when(last, |e| e.flex_1().min_h_0())
            .when(i > 0, |e| e.border_t_1())
            .child(inner.child(content))
            .when(fade < 1., |e| e.opacity(fade))
            .when(dragged, |e| e.opacity(0.4))
            .when(!s.ghost && p == PanelId::Explorer, |e| {
                e.on_mouse_down(MouseButton::Right, cx.listener(move |w, ev: &gpui::MouseDownEvent, _, cx| {
                    if ev.position.y.pt() < head {
                        w.panel_menu = Some((p, ev.position));
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .child(self.render_grip(p, t, cx))
            })
            .into_any_element()
    }

    fn render_grip(&self, p: PanelId, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
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
                .items_center()
                .cursor_grab()
                .hover(move |s| s.bg(hov))
                .tooltip(ui::tooltip(format!("Drag to move the {}", label.to_lowercase()), Some("right-click for more"), None))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, cx.listener(move |w, ev: &gpui::MouseDownEvent, _, cx| {
                    w.panel_menu = Some((p, ev.position));
                    cx.stop_propagation();
                    cx.notify();
                }))
                .on_drag(DraggedPanel(p), move |_, _, _, cx| crate::dnd::ghost(label.clone(), None, cx))
                .child(div().size(px(GRIP_DOT)).rounded_full().when(held, |d| d.bg(line2)).group_hover(group(p), move |s| s.bg(line2))),
        )
    }

    pub(crate) fn render_seams(&self, frame: &Frame, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        for d in Dock::ALL {
            for (ci, col) in frame.dock(d).cols.iter().enumerate() {
                let r = col.rect;
                let x = if d == Dock::Left { r.right() } else { r.x };
                out.push(
                    div()
                        .id(("dock-resize", ix(d) * 8 + ci))
                        .absolute()
                        .top(px(metrics::TITLE_H))
                        .h(px(r.h))
                        .left(px(x - SEAM_HIT / 2.))
                        .w(px(SEAM_HIT))
                        .cursor_col_resize()
                        .on_mouse_down(MouseButton::Left, cx.listener(move |w, _, _, cx| {
                            w.drag = Some(Drag::Dock(d, ci));
                            cx.stop_propagation();
                        }))
                        .into_any_element(),
                );
            }
        }
        out
    }

    pub(crate) fn resize_column(&mut self, d: Dock, ci: usize, x: f32) {
        let Some(col) = self.frame_now.dock(d).cols.get(ci) else { return };
        let w = if d == Dock::Left { x - col.rect.x } else { col.rect.right() - x };
        for (s, _) in col.slots.clone() {
            match s.panel {
                _ if s.ghost => {}
                PanelId::Explorer if self.explorer_open => self.explorer_w = w.clamp(metrics::EXPLORER_MIN, metrics::EXPLORER_MAX),
                PanelId::Side if self.side.is_some() => self.side_w = w.clamp(metrics::SIDE_MIN, metrics::SIDE_MAX),
                _ => {}
            }
        }
    }

    pub(crate) fn panel_drag_moved(&mut self, ev: &DragMoveEvent<DraggedPanel>, window: &mut Window, cx: &mut Context<Self>) {
        let p = ev.drag(cx).0;
        let pos = (ev.event.position.x.pt(), ev.event.position.y.pt());
        let rest = self.rest_frame(&self.layout, window);
        let target = target_at(&rest, &self.layout, p, pos);
        let preview = target.and_then(|to| self.rest_frame(&self.layout.moved(p, to), window).panel(p));
        let reduced = cx.reduce_motion();
        let g = self.panel_drag.get_or_insert(PanelDrag { panel: p, target: None, preview: None, shown: None, glide: None });
        if g.panel != p || g.target != target || g.preview != preview {
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

    pub(crate) fn drop_panel(&mut self, p: PanelId, cx: &mut Context<Self>) {
        if let Some(to) = self.panel_drag.take().filter(|g| g.panel == p).and_then(|g| g.target) {
            self.move_panel(p, to, cx);
        }
        cx.notify();
    }

    pub(crate) fn move_panel(&mut self, p: PanelId, to: Place, cx: &mut Context<Self>) {
        let next = self.layout.moved(p, to);
        let from = self.layout.clone();
        self.set_layout(next, Some(p), cx);
        if self.layout != from {
            let to = self.layout.clone();
            self.did(format!("Move {}", self.panel_name(p).to_lowercase()), crate::undo::Op::Layout { from, to });
        }
    }

    pub(crate) fn set_layout(&mut self, next: PanelLayout, moved: Option<PanelId>, cx: &mut Context<Self>) {
        if next == self.layout {
            return;
        }
        let old = std::mem::replace(&mut self.layout, next);
        self.explorer_blend.start(&old, &self.layout, cx.reduce_motion());
        for p in PanelId::ALL {
            let (a, b) = (old.place_of(p), self.layout.place_of(p));
            if a.dock != b.dock || (moved == Some(p) && a != b) {
                self.dock_moves.retain(|m| m.panel != p);
                if !cx.reduce_motion() {
                    let from = ghost_place(&old, &self.layout, p);
                    self.dock_moves.push(DockMove { panel: p, from, progress: Progress::new(motion::DOCK_MOVE) });
                }
            }
        }
        cx.notify();
    }

    pub(crate) fn move_sidebar_to(&mut self, d: Dock, cx: &mut Context<Self>) {
        self.set_explorer_open(true, cx);
        if self.layout.explorer_dock() == d {
            return cx.notify();
        }
        self.move_panel(PanelId::Explorer, Place::column(d, 0), cx);
        self.notice(format!("Sidebar moved to the {}", d.name()), cx);
    }

    pub(crate) fn reset_panel_layout(&mut self, cx: &mut Context<Self>) {
        self.panel_menu = None;
        let from = self.layout.clone();
        self.set_layout(PanelLayout::default(), None, cx);
        if self.layout != from {
            self.did("Reset panel layout", crate::undo::Op::Layout { from, to: self.layout.clone() });
        }
        self.notice("Panel layout reset", cx);
    }

    pub(crate) fn drag_cancelled(&mut self, cx: &mut Context<Self>) {
        self.panel_drag = None;
        self.tab_drop = None;
        cx.notify();
    }

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

    pub(crate) fn render_panel_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (_, at) = self.panel_menu?;
        let t = cx.theme();
        let here = self.layout.explorer_dock();
        let item = |key: &'static str, label: &'static str, enabled: bool, f: fn(&mut Self, &mut Context<Self>)| {
            ui::menu_item(&t, key, label, None, enabled).when(enabled, |d| {
                d.on_click(cx.listener(move |w, _, _, cx| {
                    w.panel_menu = None;
                    f(w, cx);
                    cx.notify();
                }))
            })
        };
        let menu = ui::popover(&t)
            .w(px(210.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|w, _, _, cx| {
                w.panel_menu = None;
                cx.notify();
            }))
            .child(item("pm-left", "Move Sidebar to the Left", here != Dock::Left, |w, cx| w.move_sidebar_to(Dock::Left, cx)))
            .child(item("pm-right", "Move Sidebar to the Right", here != Dock::Right, |w, cx| w.move_sidebar_to(Dock::Right, cx)))
            .child(ui::menu_sep(&t))
            .child(item("pm-reset", "Reset Panel Layout", !self.layout.is_default(), |w, cx| w.reset_panel_layout(cx)));
        Some(gpui::deferred(gpui::anchored().position(at).snap_to_window_with_margin(px(8.)).child(ui::menu_in("dock-menu-in", menu))).priority(3).into_any_element())
    }
}

pub(crate) fn workspace_actions(d: gpui::Stateful<gpui::Div>, cx: &mut Context<Workspace>) -> gpui::Stateful<gpui::Div> {
    d.on_action(cx.listener(|w, _: &MoveExplorerLeft, _, cx| w.move_sidebar_to(Dock::Left, cx)))
        .on_action(cx.listener(|w, _: &MoveExplorerRight, _, cx| w.move_sidebar_to(Dock::Right, cx)))
        .on_action(cx.listener(|w, _: &ResetPanelLayout, _, cx| w.reset_panel_layout(cx)))
        .on_drag_move(cx.listener(|w, ev: &DragMoveEvent<DraggedPanel>, window, cx| w.panel_drag_moved(ev, window, cx)))
        .on_drop(cx.listener(|w, d: &DraggedPanel, _, cx| w.drop_panel(d.0, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use PanelId::{Explorer, Side as S};

    const VIEW: (f32, f32) = (1400., 900.);
    const SIZES: Sizes = Sizes { explorer_w: 260., side_w: 360. };

    fn rest(l: &PanelLayout, side: bool) -> Frame {
        frame(VIEW, slots(l, |p| if p == S && !side { 0. } else { 1. }, &[]), &SIZES)
    }

    fn near(a: Rect, b: Rect) -> bool {
        (a.x - b.x).abs() < 0.01 && (a.y - b.y).abs() < 0.01 && (a.w - b.w).abs() < 0.01 && (a.h - b.h).abs() < 0.01
    }

    fn body() -> f32 {
        900. - metrics::TITLE_H - metrics::STATUS_H
    }

    #[test]
    fn default_frame() {
        let f = rest(&PanelLayout::default(), true);
        assert!(near(f.panel(Explorer).unwrap(), Rect { x: 0., y: metrics::TITLE_H, w: 260., h: body() }));
        assert!(near(f.panel(S).unwrap(), Rect { x: 1040., y: metrics::TITLE_H, w: 360., h: body() }));
        assert!(near(f.center, Rect { x: 260., y: metrics::TITLE_H, w: 780., h: body() }));
        let f = rest(&PanelLayout::default(), false);
        assert_eq!(f.panel(S), None);
        assert_eq!(f.dock(Dock::Right).rect.w, 0.);
        assert_eq!(f.center.w, 1140.);
        let m = rest(&PanelLayout::explorer_at(Dock::Right), true);
        assert!(near(m.panel(Explorer).unwrap(), Rect { x: 1140., y: metrics::TITLE_H, w: 260., h: body() }));
        assert!(near(m.panel(S).unwrap(), Rect { x: 0., y: metrics::TITLE_H, w: 360., h: body() }));
        let narrow = frame((800., 900.), slots(&PanelLayout::default(), |_| 1., &[]), &SIZES);
        assert!((narrow.center.w - CENTER_MIN_W).abs() < 0.01);
    }

    #[test]
    fn a_move_folds_one_place_and_opens_the_other() {
        let l = PanelLayout::explorer_at(Dock::Right);
        let from = ghost_place(&PanelLayout::default(), &l, Explorer);
        assert_eq!(from, Place::column(Dock::Left, 0));
        let half = frame(VIEW, slots(&l, |_| 1., &[(Explorer, from, 0.5)]), &SIZES);
        assert!(half.dock(Dock::Left).cols.iter().flat_map(|c| &c.slots).any(|(s, _)| s.ghost && s.panel == Explorer));
        assert_eq!(half.dock(Dock::Right).rect.w, 130.);
        let done = frame(VIEW, slots(&l, |_| 1., &[(Explorer, from, 1.)]), &SIZES);
        assert_eq!(done, rest(&l, true));
    }

    #[test]
    fn drop_targets() {
        let l = PanelLayout::default();
        let f = rest(&l, true);
        let mid = f.center.x + f.center.w / 2.;
        assert_eq!(target_at(&f, &l, Explorer, (mid + 10., 400.)), Some(Place::column(Dock::Right, 0)));
        assert_eq!(target_at(&f, &l, Explorer, (f.panel(S).unwrap().x + 20., 400.)), Some(Place::column(Dock::Right, 0)));
        assert_eq!(target_at(&f, &l, Explorer, (mid - 10., 400.)), None);
        assert_eq!(target_at(&f, &l, Explorer, (650., 10.)), None, "the title bar");
        assert_eq!(target_at(&f, &l, S, (100., 400.)), None, "the side panel does not move");
        let r = PanelLayout::explorer_at(Dock::Right);
        let fr = rest(&r, true);
        let mid = fr.center.x + fr.center.w / 2.;
        assert_eq!(target_at(&fr, &r, Explorer, (mid - 10., 400.)), Some(Place::column(Dock::Left, 0)));
        let p = rest(&l.moved(Explorer, Place::column(Dock::Right, 0)), true).panel(Explorer).unwrap();
        assert!(near(p, Rect { x: 1140., y: metrics::TITLE_H, w: 260., h: body() }), "{p:?}");
    }

    #[test]
    fn the_title_follows_the_explorer() {
        let lights = 70.;
        let r = PanelLayout::explorer_at(Dock::Right);
        let t = title_frame(&rest(&r, true), &r, [0., 1.], lights);
        assert_eq!((t.header, t.lead, t.trail), (Some(Dock::Right), 360., 260.));
        let l = PanelLayout::default();
        let t = title_frame(&rest(&l, true), &l, [1., 0.], lights);
        assert_eq!((t.header, t.lead), (Some(Dock::Left), 260.));
        let shut = frame(VIEW, slots(&l, |p| if p == Explorer { 0. } else { 1. }, &[]), &SIZES);
        assert_eq!(title_frame(&shut, &l, [1., 0.], lights).lead, lights + HEADER_MIN);
    }
}
