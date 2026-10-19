//! The panel docks (`dataman_core::layout`): the sidebar (the explorer's dock: its icon rail and
//! view, `sidebar`) on one side of the work surface, the side panel (a server panel, a feature's
//! sheet) on the other. The sidebar moves to the other side by dragging the grip on its top edge
//! there (or from the palette, the menu on its header: "Move Sidebar to the Right", "Reset Panel
//! Layout"); the side panel always goes opposite. Nothing else moves: no stacks, no columns.
//!
//! The geometry is computed here, once per frame, from the arrangement, the panels' sizes and
//! how open each one is ([`frame`]); `render` lays the docks out with exactly these sizes. A
//! drag's preview is the same computation for the arrangement the drop would make, so its
//! translucent box is the very area the sidebar will take. While it is dragged, the pointer
//! past the middle of the work surface picks the other dock ([`target_at`]). On the drop the old
//! place folds shut while the new one opens (`motion::DOCK_MOVE`), the work surface and the
//! side panel sliding along; with Reduce Motion it is simply there. Esc cancels the drag (`dnd`).
//!
//! Each panel keeps its own width (`explorer_w`, `side_w`), resized on its inner seam.

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
        /// The sidebar to the left (the default) or the right; the side panel goes opposite.
        MoveExplorerLeft,
        MoveExplorerRight,
        /// The sidebar back on the left.
        ResetPanelLayout,
    ]
);

/// The work surface keeps at least this much width between the docks.
pub const CENTER_MIN_W: f32 = 320.;
/// The grip's hit area, centred on a panel's top edge, and its dot.
const GRIP: (f32, f32) = (56., 10.);
const GRIP_DOT: f32 = 6.;
/// A seam's grabber: 8 pt straddling its hairline.
const SEAM_HIT: f32 = 8.;
/// The sidebar's header band (where its right-click menu opens).
const HEAD_H: f32 = 34.;

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

/// What takes room in a column this frame: a panel, or the place a moving panel is leaving (a
/// `ghost`, drawn empty), with how much of its room it takes (0 shut … 1 open) and its share of
/// the column's height when open (`Column::share`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slot {
    pub panel: PanelId,
    pub ghost: bool,
    pub weight: f32,
    pub share: f32,
}

/// The room the panels ask for: their widths.
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

/// A column as laid out: its area, the width its content keeps while it slides (the content is
/// clipped, never reflowed), and each slot's area.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColFrame {
    pub rect: Rect,
    pub full: f32,
    pub slots: Vec<(Slot, Rect)>,
}

/// A dock as laid out: its area and its columns, left to right.
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

/// The window's docks and the work surface between them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    /// In `Dock::ALL` order.
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

    /// Where panel `p` is (not the place it is leaving); `None` while it is shut.
    pub fn panel(&self, p: PanelId) -> Option<Rect> {
        self.docks.iter().flat_map(|d| d.slots()).find(|(s, _)| s.panel == p && !s.ghost).map(|(_, r)| *r)
    }
}

/// The slots of each dock (`Dock::ALL` order), column by column: the arrangement's panels, each
/// as open as `open` says, and those on the move (panel, where its ghost stands
/// ([`ghost_place`]), progress 0 → 1) growing in their new place while a ghost shrinks in the
/// old one.
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
            // Back in the stack it left, as tall as the panels it shared it with.
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

/// Where the ghost of `p`, leaving its place in `old`, stands in `new`: stacked with the panels
/// it shared a column with, wherever their column is now; a column of its own, right of the
/// column that was left of it.
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

/// Lays out the docks in a window of `view` (points): the left dock from the title bar down to
/// the status bar at the window's left edge, the right one likewise at its right edge, the work
/// surface between them. A column is as wide as its most open panel asks (each panel's width
/// times its weight); its panels share its height in proportion to their weights and shares.
/// The work surface keeps [`CENTER_MIN_W`] when it can.
pub fn frame(view: (f32, f32), slots: [Vec<Vec<Slot>>; 2], sizes: &Sizes) -> Frame {
    let (w, h) = view;
    let top = metrics::TITLE_H;
    let body = (h - metrics::STATUS_H).max(top) - top;
    // A shut panel takes no room; a column with none open is gone.
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

/// The place the sidebar dragged to `pos` would drop in: the other dock, once the pointer is
/// past the middle of the work surface toward it (`None`: still on its own side, or outside
/// the work surface and the docks). `frame` is the arrangement at rest. The side panel is not
/// dragged.
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

/// A panel settling into a new place: where its ghost stands ([`ghost_place`]), and how far
/// along it is.
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

/// How much each dock holds the explorer (0 … 1), eased along a move of it (`DOCK_MOVE`): the
/// history beside it takes the explorer's glass and, in the left dock, the tab band slides over
/// it (the header slot it had alone) as the explorer arrives, and back as it leaves. With Reduce
/// Motion it is simply the arrangement's.
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

    /// Now, heading for `layout`'s, and whether it is still on its way.
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

    /// The explorer changed docks: from wherever the blend is now (a move mid-way turns
    /// around), or at once with Reduce Motion.
    fn start(&mut self, old: &PanelLayout, new: &PanelLayout, reduced: bool) {
        if Self::target(old) == Self::target(new) {
            return;
        }
        let (now, _) = self.at(old, Instant::now());
        self.from = now;
        self.progress = (!reduced).then(|| Progress::new(motion::DOCK_MOVE));
    }
}

/// The title bar's geometry this frame, from the docks' (`dock_pass`): the explorer carries the
/// window's header (its toggle, the back / forward arrows, "DATAMAN") to its end of the bar, and
/// the search goes to the other end (past the traffic lights when the explorer is on the right).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TitleFrame {
    /// The end of the bar with the header: the explorer's dock.
    pub header: Option<Dock>,
    /// Where the tabs start: past the traffic lights and what sits by them, and past the left
    /// dock's header columns (the explorer's; the history's when it is there alone). The history
    /// beside the explorer is under the tab band instead.
    pub lead: f32,
    /// How open those header columns are (0 … 1; the first tab lines up past their seam).
    pub seam: f32,
    /// The room the header takes at the right end (the explorer in the right dock), else 0.
    pub trail: f32,
    /// How much each dock holds the explorer ([`ExplorerBlend`]).
    pub joined: [f32; 2],
}

/// The least room the header keeps at its end of the bar, past the lights on the left (the
/// toggle 14 pt after them, the two arrows) or at the right edge (the arrows, the toggle).
pub const HEADER_MIN: f32 = 92.;
/// The least room past the lights for the search, when it is on the left.
pub const SEARCH_MIN: f32 = 96.;

/// The title bar's geometry for a frame of the docks: `lights` is the traffic lights' right
/// edge.
pub fn title_frame(frame: &Frame, layout: &PanelLayout, joined: [f32; 2], lights: f32) -> TitleFrame {
    let header = layout.place_of(PanelId::Explorer).dock;
    let explorer = |c: &ColFrame| c.slots.iter().any(|(s, _)| s.panel == PanelId::Explorer);
    // The left dock's header columns: the explorer's, and the history's as far as it is on its
    // own there (the tab band slides over it as the explorer joins it).
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

/// This frame's docks: their geometry, the history's view on screen, and how much of it the
/// result zoom leaves (1: all of it).
pub(crate) struct DockPass {
    pub frame: Frame,
    pub side: Option<Side>,
    pub side_rest: f32,
    pub title: TitleFrame,
}

impl DockPass {
    /// The left dock's last column is the history's under the tab band (beside the explorer):
    /// the work column's seam is that column's, under the band.
    pub fn band_left(&self) -> bool {
        self.title.joined[ix(Dock::Left)] > 0. && self.frame.dock(Dock::Left).cols.last().is_some_and(|c| !c.slots.iter().any(|(s, _)| s.panel == PanelId::Explorer))
    }
}

/// The docks' elements, for `render` to place.
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
    /// side panel away and the panels settling into new places; kept in `frame_now` (the title bar and the checks read it).
    /// Asks for another frame while a panel moves. Called once per frame, before anything is
    /// laid out.
    pub(crate) fn dock_pass(&mut self, first_run: bool, window: &mut Window, cx: &App) -> DockPass {
        let now = Instant::now();
        // The side panel slides in and out (the view it last showed while it slides out); the
        // zoomed result covers it too, folding it away with the editor.
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
        let title = title_frame(&frame, &self.layout, joined, metrics::lights_right_at(crate::theme::zoom()));
        self.frame_now = frame.clone();
        self.title_now = title;
        DockPass { frame, side: shown, side_rest: rest, title }
    }

    /// What the dragged panel is called on its ghost and on the preview.
    fn panel_name(&self, p: PanelId) -> &'static str {
        match (p, &self.side) {
            (PanelId::Explorer, _) => "Sidebar",
            (PanelId::Side, Some(Side::Server(id, ..) | Side::Feature(id, ..))) => id,
            (PanelId::Side, None) => "Side panel",
        }
    }

    /// The docks as elements, each from the window's top edge to the status bar (the title bar
    /// is laid over them): each column holds its panels stacked, each slot clipping its panel,
    /// which keeps the column's full width (anchored to the window's edge) so it never reflows
    /// mid-slide.
    ///
    /// The explorer's column is the glass sidebar, the title band over it its header. The
    /// history with it (stacked, or a column beside it) takes the same glass; a column of its own
    /// beside the explorer starts under the tab band, which runs over it (the band's hairline,
    /// red on production, and no seam through the band). Alone in a dock, the history has the
    /// work column's fill: in the left dock the band over it is the header slot, in the right
    /// one the tab band runs over it. Between the two the fill fades as the explorer comes and
    /// goes (`TitleFrame::joined`). `production`: the work column's red accent.
    pub(crate) fn render_docks(&mut self, pass: &DockPass, production: bool, window: &mut Window, cx: &mut Context<Self>) -> DockViews {
        let t = cx.theme();
        let mut views: [Option<AnyElement>; 2] = Default::default();
        // An empty dock lays out nothing: no stale bounds.
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
                // Out of the explorer's column, the history keeps the work column's fill (beside
                // the explorer too: only stacked with it does it take the glass).
                let fill = (!glass).then_some(t.panel);
                // Under the tab band (not a header slot): no seam through the band beside it.
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
                // The seams: between columns, from the top (beside a header); the work column's
                // own on the left (app.rs) unless this one is under the tab band, then this
                // column's, under the band; on the right this column's, likewise.
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
                    // The title band's hairline (red on production), as over the work column.
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
                        // The title band over it: the header (glass), or the band's fill.
                        .child(div().h(px(metrics::TITLE_H)).flex_none().when(!glass, |b| b.bg(t.panel)))
                        .child(body)
                        // The seam under the band: drawn over the edge (it takes no room, as the
                        // work column's own would not have).
                        .when(body_l || body_r, |c| {
                            let line = div().absolute().top(px(metrics::TITLE_H)).bottom_0().w(px(1.)).bg(t.line);
                            c.child(if body_l { line.left_0() } else { line.right_0() })
                        }),
                );
            }
            let rect = df.rect;
            // On the glass, from the window's top edge (the title bar is laid over it).
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
        // The one being dragged waits, dimmed, until it is dropped.
        let dragged = !s.ghost && self.panel_drag.as_ref().is_some_and(|g| g.panel == s.panel);
        let fade = if s.panel == PanelId::Side && !s.ghost { side_rest } else { 1. };
        let p = s.panel;
        // A right click on the sidebar's header: its menu (move it, reset the layout).
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
            // Only the sidebar moves: the side panel has no grip.
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

    /// The grip on the sidebar's top edge: a dot that shows while it is hovered; dragging it
    /// to the other side moves the sidebar there, a right click opens its menu.
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
                // Not the panel's own press (a click in the tree, the history's selection).
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

    /// The docks' seams, laid over everything (window coordinates; not while a panel moves):
    /// each column's inner edge resizes it.
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

    /// Column `ci`'s inner seam dragged to `x` (window points): its open panels take the new
    /// width, each within its own bounds.
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

    /// Panel `p` to place `to`: an undo step.
    pub(crate) fn move_panel(&mut self, p: PanelId, to: Place, cx: &mut Context<Self>) {
        let next = self.layout.moved(p, to);
        let from = self.layout.clone();
        self.set_layout(next, Some(p), cx);
        if self.layout != from {
            let to = self.layout.clone();
            self.did(format!("Move {}", self.panel_name(p).to_lowercase()), crate::undo::Op::Layout { from, to });
        }
    }

    /// A new arrangement: each panel that changes dock (and `moved`, when it changes place in
    /// its own) settles into its new place from the old one.
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

    /// The palette's "Move Sidebar to the Left / Right": the sidebar there (shown if it was
    /// shut), the side panel opposite.
    pub(crate) fn move_sidebar_to(&mut self, d: Dock, cx: &mut Context<Self>) {
        self.set_explorer_open(true, cx);
        if self.layout.explorer_dock() == d {
            return cx.notify();
        }
        self.move_panel(PanelId::Explorer, Place::column(d, 0), cx);
        self.notice(format!("Sidebar moved to the {}", d.name()), cx);
    }

    /// The sidebar back on the left.
    pub(crate) fn reset_panel_layout(&mut self, cx: &mut Context<Self>) {
        self.panel_menu = None;
        let from = self.layout.clone();
        self.set_layout(PanelLayout::default(), None, cx);
        if self.layout != from {
            self.did("Reset panel layout", crate::undo::Op::Layout { from, to: self.layout.clone() });
        }
        self.notice("Panel layout reset", cx);
    }

    /// A drag let go of with esc (`dnd`): no preview, no drop.
    pub(crate) fn drag_cancelled(&mut self, cx: &mut Context<Self>) {
        self.panel_drag = None;
        self.tab_drop = None;
        cx.notify();
    }

    /// The drop preview: a translucent box over exactly the area the dragged panel would take,
    /// gliding from one place to the next. Gone with the drag (dropped, cancelled, or let go
    /// elsewhere).
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

    /// The sidebar's right-click menu (on its header or its grip): move it to the other side, or
    /// back home.
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

/// The workspace's dock commands (the palette's entries).
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

    /// The default arrangement: the sidebar at the left from under the title bar to the status
    /// bar, the side panel at the right, the work surface between them; mirrored, the other way.
    #[test]
    fn default_frame() {
        let f = rest(&PanelLayout::default(), true);
        assert!(near(f.panel(Explorer).unwrap(), Rect { x: 0., y: metrics::TITLE_H, w: 260., h: body() }));
        assert!(near(f.panel(S).unwrap(), Rect { x: 1040., y: metrics::TITLE_H, w: 360., h: body() }));
        assert!(near(f.center, Rect { x: 260., y: metrics::TITLE_H, w: 780., h: body() }));
        // Shut: no room at all.
        let f = rest(&PanelLayout::default(), false);
        assert_eq!(f.panel(S), None);
        assert_eq!(f.dock(Dock::Right).rect.w, 0.);
        assert_eq!(f.center.w, 1140.);
        let m = rest(&PanelLayout::explorer_at(Dock::Right), true);
        assert!(near(m.panel(Explorer).unwrap(), Rect { x: 1140., y: metrics::TITLE_H, w: 260., h: body() }));
        assert!(near(m.panel(S).unwrap(), Rect { x: 0., y: metrics::TITLE_H, w: 360., h: body() }));
        // Too wide for the window: the panels give way, the work surface keeps its least width.
        let narrow = frame((800., 900.), slots(&PanelLayout::default(), |_| 1., &[]), &SIZES);
        assert!((narrow.center.w - CENTER_MIN_W).abs() < 0.01);
    }

    /// A move half-way: the old place half shut, the new one half open; done, it is exactly the
    /// arrangement at rest.
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

    /// The sidebar drops on the other side once the pointer is past the work surface's middle;
    /// on its own side, outside the body or for the side panel: nowhere.
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
        // The preview is the area it takes once dropped there.
        let p = rest(&l.moved(Explorer, Place::column(Dock::Right, 0)), true).panel(Explorer).unwrap();
        assert!(near(p, Rect { x: 1140., y: metrics::TITLE_H, w: 260., h: body() }), "{p:?}");
    }

    /// The header goes with the sidebar; the tabs start past the left dock (the side panel
    /// alone there is the header slot).
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
