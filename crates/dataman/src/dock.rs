use dataman_core::layout::{Dock, PanelId, PanelLayout};
use gpui::{AnyElement, App, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Styled, Window, div, prelude::FluentBuilder};

use crate::app::{Drag, Side, Workspace};
use crate::theme::{ActiveTheme, Pt, Theme, metrics, px};

pub const CENTER_MIN_W: f32 = 320.;
const SEAM_HIT: f32 = 8.;

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
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slot {
    pub panel: PanelId,
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
    #[cfg(any(test, feature = "checks"))]
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

    #[cfg(any(test, feature = "checks"))]
    pub fn panel(&self, p: PanelId) -> Option<Rect> {
        self.docks.iter().flat_map(|d| d.slots()).find(|(s, _)| s.panel == p).map(|(_, r)| *r)
    }
}

pub fn slots(layout: &PanelLayout, open: impl Fn(PanelId) -> f32) -> [Vec<Vec<Slot>>; 2] {
    let mut out: [Vec<Vec<Slot>>; 2] = Default::default();
    for d in Dock::ALL {
        for c in layout.dock(d) {
            let col = c.panels.iter().enumerate().map(|(i, &p)| Slot { panel: p, weight: open(p).clamp(0., 1.), share: c.share(i) });
            out[ix(d)].push(col.collect());
        }
    }
    out
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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TitleFrame {
    pub lead: f32,
    pub seam: f32,
}

pub const HEADER_MIN: f32 = 92.;

pub fn title_frame(frame: &Frame, lights: f32) -> TitleFrame {
    let left = &frame.dock(Dock::Left).cols;
    let edge: f32 = left.iter().map(|c| c.rect.w).sum();
    let full: f32 = left.iter().map(|c| c.full).sum();
    TitleFrame { lead: edge.max(lights + HEADER_MIN), seam: (edge / full.max(1.)).clamp(0., 1.) }
}

pub(crate) struct DockPass {
    pub frame: Frame,
    pub side: Option<Side>,
    pub side_rest: f32,
}

pub(crate) struct DockViews {
    pub left: Option<AnyElement>,
    pub right: Option<AnyElement>,
}

impl Workspace {
    pub(crate) fn sizes(&self) -> Sizes {
        Sizes { explorer_w: self.explorer_w, side_w: self.side_w }
    }

    fn view_pt(window: &Window) -> (f32, f32) {
        let v = window.viewport_size();
        (v.width.pt(), v.height.pt())
    }

    /// Called once per frame, before anything is laid out.
    pub(crate) fn dock_pass(&mut self, first_run: bool, window: &mut Window, cx: &App) -> DockPass {
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
        let open = |p: PanelId| match p {
            PanelId::Explorer => explorer_open,
            PanelId::Side => side_open,
        };
        let frame = frame(Self::view_pt(window), slots(&PanelLayout::default(), open), &self.sizes());
        self.frame_now = frame.clone();
        self.title_now = title_frame(&frame, crate::csd::lead(window, cx));
        DockPass { frame, side: shown, side_rest: rest }
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
            for (ci, col) in df.cols.iter().enumerate() {
                let n = col.slots.len();
                let glass = col.slots.iter().any(|(s, _)| s.panel == PanelId::Explorer);
                let fill = (!glass).then_some(t.panel);
                let band = !glass && d == Dock::Right;
                let mut children = Vec::with_capacity(n);
                for (i, (s, r)) in col.slots.iter().enumerate() {
                    let content: AnyElement = match (s.panel, &pass.side) {
                        (PanelId::Explorer, _) => self.render_sidebar(window, cx),
                        (PanelId::Side, Some(Side::Feature(_, v, _))) => v.clone().into_any_element(),
                        (PanelId::Side, None) => div().into_any_element(),
                    };
                    children.push(self.render_slot(d, i, n, *s, *r, col.full, content, pass.side_rest, &t));
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
                let body_l = ci == 0 && band;
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
                        .when(body_l, |c| c.child(div().absolute().top(px(metrics::TITLE_H)).bottom_0().left_0().w(px(1.)).bg(t.line))),
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
    fn render_slot(&self, d: Dock, i: usize, n: usize, s: Slot, r: Rect, full: f32, content: AnyElement, side_rest: f32, t: &Theme) -> AnyElement {
        let last = i + 1 == n;
        let inner = match d {
            Dock::Left => div().absolute().top_0().bottom_0().left_0().w(px(full)),
            Dock::Right => div().absolute().top_0().bottom_0().right_0().w(px(full)),
        };
        let fade = if s.panel == PanelId::Side { side_rest } else { 1. };
        div()
            .relative()
            .flex_none()
            .overflow_hidden()
            .border_color(t.line)
            .w_full()
            // The last slot takes what is left (no gap from rounding).
            .when(!last, |e| e.h(px(r.h)))
            .when(last, |e| e.flex_1().min_h_0())
            .when(i > 0, |e| e.border_t_1())
            .child(inner.child(content))
            .when(fade < 1., |e| e.opacity(fade))
            .into_any_element()
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
                PanelId::Explorer if self.explorer_open => self.explorer_w = w.clamp(metrics::EXPLORER_MIN, metrics::EXPLORER_MAX),
                PanelId::Side if self.side.is_some() => self.side_w = w.clamp(metrics::SIDE_MIN, metrics::SIDE_MAX),
                _ => {}
            }
        }
    }

    pub(crate) fn drag_cancelled(&mut self, cx: &mut Context<Self>) {
        self.tab_drop = None;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PanelId::{Explorer, Side as S};

    const VIEW: (f32, f32) = (1400., 900.);
    const SIZES: Sizes = Sizes { explorer_w: 260., side_w: 360. };

    fn rest(side: bool) -> Frame {
        frame(VIEW, slots(&PanelLayout::default(), |p| if p == S && !side { 0. } else { 1. }), &SIZES)
    }

    fn near(a: Rect, b: Rect) -> bool {
        (a.x - b.x).abs() < 0.01 && (a.y - b.y).abs() < 0.01 && (a.w - b.w).abs() < 0.01 && (a.h - b.h).abs() < 0.01
    }

    fn body() -> f32 {
        900. - metrics::TITLE_H - metrics::STATUS_H
    }

    #[test]
    fn default_frame() {
        let f = rest(true);
        assert!(near(f.panel(Explorer).unwrap(), Rect { x: 0., y: metrics::TITLE_H, w: 260., h: body() }));
        assert!(near(f.panel(S).unwrap(), Rect { x: 1040., y: metrics::TITLE_H, w: 360., h: body() }));
        assert!(near(f.center, Rect { x: 260., y: metrics::TITLE_H, w: 780., h: body() }));
        let f = rest(false);
        assert_eq!(f.panel(S), None);
        assert_eq!(f.dock(Dock::Right).rect.w, 0.);
        assert_eq!(f.center.w, 1140.);
        let narrow = frame((800., 900.), slots(&PanelLayout::default(), |_| 1.), &SIZES);
        assert!((narrow.center.w - CENTER_MIN_W).abs() < 0.01);
    }

    #[test]
    fn the_title_follows_the_explorer() {
        let lights = 70.;
        let t = title_frame(&rest(true), lights);
        assert_eq!((t.lead, t.seam), (260., 1.));
        let shut = frame(VIEW, slots(&PanelLayout::default(), |p| if p == Explorer { 0. } else { 1. }), &SIZES);
        assert_eq!(title_frame(&shut, lights).lead, lights + HEADER_MIN);
    }
}
