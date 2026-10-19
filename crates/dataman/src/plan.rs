//! The plan view: an EXPLAIN result shown as a plan in place of the grid (a result tab
//! kind). The node tree (collapsible, with connectors, the hot-node bars,
//! the misestimate flags and the hint chips), the flame view of exclusive time, the details of
//! the selected node and the hints over the tree. The plan is parsed off the UI thread
//! (`dataman_core::explain`); the tree is a `uniform_list` (only the rows on screen are built)
//! and the flame is one canvas, so plans of thousands of nodes stay smooth.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, Div, FocusHandle, Focusable, FontWeight, Hsla, KeyBinding, MouseButton, MouseDownEvent, Pixels, SharedString,
    Task, TextAlign, UniformListScrollHandle, Window, actions, canvas, div, point, prelude::*, size, uniform_list,
};
use crate::theme::{Pt, px};
use dataman_core::ResultSet;
use dataman_core::explain::{self, Format, Plan, Rule, fmt_cost, fmt_count, fmt_kb, fmt_ms};

use crate::theme::{ActiveTheme, Theme, mono};
use crate::ui;

actions!(plan, [SelectPrev, SelectNext, Collapse, Expand]);

pub const CONTEXT: &str = "PlanView";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrev, c),
        KeyBinding::new("down", SelectNext, c),
        KeyBinding::new("left", Collapse, c),
        KeyBinding::new("right", Expand, c),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Tree,
    Flame,
}

const ROW_H: f32 = 22.;
const INDENT: f32 = 14.;
const FLAME_H: f32 = 18.;
const DETAILS_W: f32 = 330.;
/// The hints listed over the tree (the rest are on their rows).
const HINTS_SHOWN: usize = 5;
// Columns of the tree (analyzed / estimated).
const BAR_W: f32 = 96.;
const TIME_W: f32 = 68.;
const ROWS_W: f32 = 118.;
const LOOPS_W: f32 = 46.;
const BUF_W: f32 = 108.;

/// A node's box in the flame view: its span (fractions of the width) and its level.
#[derive(Clone, Copy, Debug)]
struct FlameBox {
    node: usize,
    x0: f32,
    x1: f32,
    depth: u16,
}

pub struct PlanView {
    focus: FocusHandle,
    /// The EXPLAIN result on screen, and its plan once parsed.
    rs: Option<Arc<ResultSet>>,
    plan: Option<Arc<Plan>>,
    pub mode: Mode,
    collapsed: HashSet<usize>,
    selected: Option<usize>,
    /// Visible rows (node indexes), and where each node's subtree ends.
    rows: Vec<usize>,
    ends: Vec<usize>,
    /// The hint chips of each node.
    chips: Vec<Vec<(String, Rule)>>,
    flame: Vec<FlameBox>,
    flame_depth: u16,
    flame_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    scroll: UniformListScrollHandle,
    _parse: Option<Task<()>>,
}

impl Focusable for PlanView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The bar and flame color of a node by its share: hot, warm, or plain.
pub fn heat(t: &Theme, share: f64) -> Hsla {
    if share >= 0.4 {
        t.err
    } else if share >= 0.15 {
        t.warn
    } else {
        t.tx3
    }
}

/// The misestimate flag (`×42`), from 10× off.
fn flag(t: &Theme, m: Option<explain::Misestimate>) -> Option<Div> {
    let m = m.filter(|m| m.factor >= 10.)?;
    let color = if m.factor >= 100. { t.err } else { t.warn };
    Some(div().flex_none().text_size(px(11.)).text_color(color).child(format!("×{}", fmt_count(m.factor.round()))))
}

fn chip(t: &Theme, text: &str) -> Div {
    div()
        .flex_none()
        .h(px(16.))
        .px(px(5.))
        .flex()
        .items_center()
        .bg(t.warn_s)
        .text_color(t.warn)
        .text_size(px(10.))
        .whitespace_nowrap()
        .child(text.to_string())
}

impl PlanView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        PlanView {
            focus: cx.focus_handle(),
            rs: None,
            plan: None,
            mode: Mode::Tree,
            collapsed: HashSet::new(),
            selected: None,
            rows: Vec::new(),
            ends: Vec::new(),
            chips: Vec::new(),
            flame: Vec::new(),
            flame_depth: 0,
            flame_bounds: Rc::new(Cell::new(None)),
            scroll: UniformListScrollHandle::new(),
            _parse: None,
        }
    }

    /// The EXPLAIN result to show (`None`: none). Parsing runs in the background; the same
    /// result again is free.
    pub fn set_result(&mut self, rs: Option<Arc<ResultSet>>, cx: &mut Context<Self>) {
        match (&self.rs, &rs) {
            (Some(a), Some(b)) if Arc::ptr_eq(a, b) => return,
            (None, None) => return,
            _ => {}
        }
        self.rs = rs.clone();
        self.plan = None;
        self.collapsed.clear();
        self.selected = None;
        self.rows.clear();
        self._parse = None;
        cx.notify();
        let Some(rs) = rs else { return };
        let snap = rs.snapshot();
        let task = cx.background_spawn(async move {
            let cols: Vec<String> = snap.columns.iter().map(|c| c.name.clone()).collect();
            let rows: Vec<Vec<Option<String>>> =
                (0..snap.rows()).map(|r| (0..cols.len()).map(|c| snap.text(r, c).map(|s| s.into_owned())).collect()).collect();
            Plan::parse(&Plan::source_of(&cols, &rows))
        });
        self._parse = Some(cx.spawn(async move |this, cx| {
            let plan = task.await;
            let _ = this.update(cx, |v, cx| v.set_plan(Arc::new(plan), cx));
        }));
    }

    /// Shows `plan` (demo stages, tests): selects its hottest node.
    pub fn set_plan(&mut self, plan: Arc<Plan>, cx: &mut Context<Self>) {
        let n = plan.nodes.len();
        let mut size = vec![1usize; n];
        for i in (0..n).rev() {
            if let Some(p) = plan.nodes[i].parent {
                size[p] += size[i];
            }
        }
        self.ends = (0..n).map(|i| i + size[i]).collect();
        self.chips = vec![Vec::new(); n];
        for h in &plan.hints {
            if !h.chip.is_empty() && h.node < n {
                self.chips[h.node].push((h.chip.clone(), h.rule));
            }
        }
        (self.flame, self.flame_depth) = flame_layout(&plan);
        self.selected = plan.nodes.iter().enumerate().max_by(|a, b| a.1.share.total_cmp(&b.1.share)).map(|(i, _)| i).filter(|_| n > 0);
        if self.mode == Mode::Flame && self.flame.is_empty() {
            self.mode = Mode::Tree;
        }
        self.plan = Some(plan);
        self.collapsed.clear();
        self.rebuild_rows();
        cx.notify();
    }

    pub fn plan(&self) -> Option<&Arc<Plan>> {
        self.plan.as_ref()
    }

    /// It shows `rs` (parsed or still parsing).
    pub fn is_for(&self, rs: &Arc<ResultSet>) -> bool {
        self.rs.as_ref().is_some_and(|r| Arc::ptr_eq(r, rs))
    }

    /// The flame view has something to show (times or costs).
    pub fn has_flame(&self) -> bool {
        !self.flame.is_empty()
    }

    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.mode = if mode == Mode::Flame && !self.has_flame() { Mode::Tree } else { mode };
        cx.notify();
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Rows the tree shows (folded subtrees left out).
    pub fn visible_rows(&self) -> usize {
        self.rows.len()
    }

    pub fn select(&mut self, i: usize, cx: &mut Context<Self>) {
        // A node inside a collapsed subtree: open the way to it.
        if let Some(p) = &self.plan {
            let mut a = p.nodes.get(i).and_then(|n| n.parent);
            while let Some(x) = a {
                self.collapsed.remove(&x);
                a = p.nodes[x].parent;
            }
        }
        self.rebuild_rows();
        self.selected = Some(i);
        if let Some(ix) = self.rows.iter().position(|&r| r == i) {
            self.scroll.scroll_to_item(ix, gpui::ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    pub fn toggle(&mut self, i: usize, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&i) {
            self.collapsed.insert(i);
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let n = self.plan.as_ref().map_or(0, |p| p.nodes.len());
        let mut i = 0;
        while i < n {
            self.rows.push(i);
            i = if self.collapsed.contains(&i) { self.ends[i] } else { i + 1 };
        }
    }

    fn step(&mut self, d: isize, cx: &mut Context<Self>) {
        let cur = self.selected.and_then(|s| self.rows.iter().position(|&r| r == s));
        let next = match cur {
            Some(ix) => (ix as isize + d).clamp(0, self.rows.len() as isize - 1) as usize,
            None => 0,
        };
        if let Some(&node) = self.rows.get(next) {
            self.select(node, cx);
        }
    }

    fn collapse_or_parent(&mut self, cx: &mut Context<Self>) {
        let (Some(s), Some(p)) = (self.selected, self.plan.clone()) else { return };
        if !p.nodes[s].children.is_empty() && !self.collapsed.contains(&s) {
            self.toggle(s, cx);
        } else if let Some(parent) = p.nodes[s].parent {
            self.select(parent, cx);
        }
    }

    fn expand_or_child(&mut self, cx: &mut Context<Self>) {
        let (Some(s), Some(p)) = (self.selected, self.plan.clone()) else { return };
        if self.collapsed.contains(&s) {
            self.toggle(s, cx);
        } else if let Some(&c) = p.nodes[s].children.first() {
            self.select(c, cx);
        }
    }

    fn flame_click(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        let Some(b) = self.flame_bounds.get() else { return };
        let (x, y) = ((ev.position.x - b.left()) / b.size.width, ((ev.position.y - b.top()) / px(FLAME_H)).floor());
        let hit = self.flame.iter().find(|f| f.depth as f32 == y && x >= f.x0 && x < f.x1).map(|f| f.node);
        if let Some(i) = hit {
            self.select(i, cx);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Drawing.

    /// The connectors of a row: for each level above the node, whether a line goes on down
    /// (`│`), and for the node itself whether it is the last of its siblings (`└` or `├`).
    fn guides(p: &Plan, i: usize) -> (Vec<bool>, bool) {
        let d = p.nodes[i].depth as usize;
        let mut cont = vec![false; d.saturating_sub(1)];
        let mut a = p.nodes[i].parent;
        while let Some(x) = a {
            let dx = p.nodes[x].depth as usize;
            if dx >= 1 && dx - 1 < cont.len() {
                cont[dx - 1] = !p.is_last(x);
            }
            a = p.nodes[x].parent;
        }
        (cont, p.is_last(i))
    }

    fn render_row(&self, ix: usize, t: &Theme, focused: bool, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let p = self.plan.clone().expect("rows only exist with a plan");
        let i = self.rows[ix];
        let n = &p.nodes[i];
        let sel = self.selected == Some(i);
        let hov = t.hov;
        let (cont, last) = Self::guides(&p, i);
        let line = t.line2;
        let depth = n.depth as usize;
        let guides = canvas(
            |_, _, _| (),
            move |b: Bounds<Pixels>, _, window, _| {
                let mid_y = b.top() + px(ROW_H / 2.);
                let quad = |x: Pixels, y: Pixels, w: f32, h: Pixels| gpui::fill(Bounds::new(point(x, y), size(px(w), h)), line);
                for (k, on) in cont.iter().enumerate() {
                    if *on {
                        window.paint_quad(quad(b.left() + px(k as f32 * INDENT + 6.), b.top(), 1., b.size.height));
                    }
                }
                if depth > 0 {
                    let x = b.left() + px((depth - 1) as f32 * INDENT + 6.);
                    let h = if last { mid_y - b.top() } else { b.size.height };
                    window.paint_quad(quad(x, b.top(), 1., h));
                    window.paint_quad(quad(x, mid_y, INDENT - 7., px(1.)));
                }
            },
        )
        .flex_none()
        .w(px(depth as f32 * INDENT))
        .h_full();
        let caret = (!n.children.is_empty()).then(|| {
            let open = !self.collapsed.contains(&i);
            div()
                .id(("plan-caret", i))
                .flex_none()
                .w(px(14.))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(9.))
                .text_color(t.tx3)
                .child(ui::disclosure(("plan-chev", i), open, 9., t.tx3))
                .on_mouse_down(MouseButton::Left, cx.listener(move |v, _, _, cx| {
                    cx.stop_propagation();
                    v.toggle(i, cx);
                }))
        });
        let label = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .overflow_hidden()
            .when(caret.is_none(), |d| d.pl(px(14.)))
            .children(n.subplan().map(|s| div().flex_none().text_color(t.tx3).child(s.split(" (").next().unwrap_or(s).to_string())))
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(if n.never_executed() { t.tx4 } else { t.tx })
                    .child(n.title.clone()),
            )
            .children(self.chips[i].iter().map(|(c, _)| chip(t, c)));
        let mut row = div()
            .id(("plan-row", i))
            .h(px(ROW_H))
            .w_full()
            .pr(px(10.))
            .flex()
            .flex_row()
            .items_center()
            .text_size(px(12.))
            .when(sel, |d| d.bg(if focused { t.sel } else { t.hov }))
            .when(!sel, |d| d.hover(move |s| s.bg(hov)))
            .on_mouse_down(MouseButton::Left, cx.listener(move |v, ev: &MouseDownEvent, window, cx| {
                window.focus(&v.focus, cx);
                if ev.click_count == 2 {
                    v.toggle(i, cx);
                }
                v.select(i, cx);
            }))
            .child(div().flex_none().w(px(8.)))
            .child(guides)
            .children(caret)
            .child(label);
        let num = |w: f32, s: String, c: Hsla| div().flex_none().w(px(w)).text_right().text_color(c).whitespace_nowrap().overflow_hidden().child(s);
        let has_metrics = p.analyzed || p.nodes.iter().any(|x| x.total_cost().is_some());
        if has_metrics {
            row = row.child(self.bar(n.share, t));
        }
        if p.analyzed {
            let rows = div()
                .flex_none()
                .w(px(ROWS_W))
                .flex()
                .flex_row()
                .justify_end()
                .items_baseline()
                .gap(px(5.))
                .whitespace_nowrap()
                .children(n.act_rows().filter(|_| !n.never_executed()).map(|a| div().text_color(t.tx2).child(fmt_count(a))))
                .children(n.est_rows().map(|e| div().text_size(px(11.)).text_color(t.tx4).child(format!("({})", fmt_count(e)))))
                .children(flag(t, n.misestimate()));
            let (excl, incl) = if n.never_executed() { (String::new(), "never".to_string()) } else { (fmt_ms(n.excl_ms), fmt_ms(n.incl_ms)) };
            let loops = n.loops().filter(|&l| l != 1.).map(fmt_count).unwrap_or_default();
            row = row
                .child(num(TIME_W, excl, t.tx))
                .child(num(TIME_W, incl, t.tx3))
                .child(rows)
                .child(num(LOOPS_W, loops, t.tx3))
                .child(self.buffers(n, t));
        } else if has_metrics {
            row = row
                .child(num(TIME_W, fmt_cost(n.excl_cost), t.tx))
                .child(num(TIME_W, n.total_cost().map(fmt_cost).unwrap_or_default(), t.tx3))
                .child(num(LOOPS_W + 18., n.est_rows().map(fmt_count).unwrap_or_default(), t.tx2));
        }
        row
    }

    /// The share bar: a track with the node's part of the time (or cost), and the percent.
    fn bar(&self, share: f64, t: &Theme) -> Div {
        let c = heat(t, share);
        let fill = (share.clamp(0., 1.) * 56.) as f32;
        div()
            .flex_none()
            .w(px(BAR_W))
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .gap(px(6.))
            .child(
                div()
                    .w(px(56.))
                    .h(px(6.))
                    .bg(t.hov)
                    .child(div().h_full().w(px(if share > 0. { fill.max(2.) } else { 0. })).bg(if c == t.tx3 { t.tx3.opacity(0.55) } else { c })),
            )
            .child(
                div()
                    .w(px(30.))
                    .text_right()
                    .text_size(px(11.))
                    .text_color(if c == t.tx3 { t.tx3 } else { c })
                    .child(if share >= 0.005 { format!("{:.0}%", share * 100.) } else { String::new() }),
            )
    }

    /// `h 12k r 1.6k t 4.1k`: shared hits, shared reads, temp blocks (zeros left out).
    fn buffers(&self, n: &explain::Node, t: &Theme) -> Div {
        let b = n.buffers();
        let parts: Vec<(&str, f64)> = [("h", b.shared_hit), ("r", b.shared_read), ("t", b.temp_read + b.temp_written)].into_iter().filter(|(_, v)| *v > 0.).collect();
        div()
            .flex_none()
            .w(px(BUF_W))
            .flex()
            .flex_row()
            .justify_end()
            .gap(px(5.))
            .whitespace_nowrap()
            .text_size(px(11.))
            .children(parts.into_iter().map(|(k, v)| {
                div().flex().flex_row().child(div().text_color(t.tx4).child(k)).child(div().text_color(if k == "t" { t.warn } else { t.tx2 }).child(fmt_count(v)))
            }))
    }

    fn render_header(&self, p: &Plan, t: &Theme) -> Div {
        let label = |w: f32, s: &'static str| div().flex_none().w(px(w)).text_right().child(s);
        let has_metrics = p.analyzed || p.nodes.iter().any(|x| x.total_cost().is_some());
        let mut d = div()
            .h(px(24.))
            .flex_none()
            .pl(px(8.))
            .pr(px(10.))
            .flex()
            .flex_row()
            .items_center()
            .text_size(px(10.5))
            .text_color(t.tx3)
            .border_b_1()
            .border_color(t.line)
            .child(div().flex_1().pl(px(14.)).child(format!("{} nodes", p.nodes.len())));
        if p.analyzed {
            d = d
                .child(label(BAR_W, "time"))
                .child(label(TIME_W, "self"))
                .child(label(TIME_W, "total"))
                .child(label(ROWS_W, "rows (est)"))
                .child(label(LOOPS_W, "loops"))
                .child(label(BUF_W, "buffers"));
        } else if has_metrics {
            d = d.child(label(BAR_W, "cost")).child(label(TIME_W, "self")).child(label(TIME_W, "total")).child(label(LOOPS_W + 18., "rows"));
        }
        d
    }

    /// The findings over the tree: the costliest first; a click selects the node.
    fn render_hints(&self, p: &Plan, t: &Theme, cx: &mut Context<Self>) -> Option<Div> {
        if p.hints.is_empty() {
            return None;
        }
        let more = p.hints.len().saturating_sub(HINTS_SHOWN);
        Some(
            div()
                .flex_none()
                .py(px(4.))
                .border_b_1()
                .border_color(t.line)
                .flex()
                .flex_col()
                .children(p.hints.iter().take(HINTS_SHOWN).enumerate().map(|(k, h)| {
                    let node = h.node;
                    let hov = t.hov;
                    div()
                        .id(("plan-hint", k))
                        .h(px(22.))
                        .px(px(12.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(12.))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .cursor_pointer()
                        .hover(move |s| s.bg(hov))
                        .child(div().flex_none().size(px(6.)).rounded(px(3.)).bg(t.warn))
                        .child(div().flex_none().text_color(t.tx).child(h.text.clone()))
                        .children(h.fix.clone().map(|f| div().min_w_0().overflow_hidden().text_ellipsis().text_color(t.tx3).child(format!("→ {f}"))))
                        .on_click(cx.listener(move |v, _, window, cx| {
                            window.focus(&v.focus, cx);
                            v.mode = Mode::Tree;
                            v.select(node, cx);
                        }))
                }))
                .when(more > 0, |d| d.child(div().h(px(18.)).px(px(26.)).text_size(px(11.)).text_color(t.tx3).child(format!("+{more}")))),
        )
    }

    fn render_flame(&self, p: &Plan, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let boxes = self.flame.clone();
        let p2 = p.clone();
        let sel = self.selected;
        let theme = t.clone();
        let bounds_cell = self.flame_bounds.clone();
        let h = (self.flame_depth as f32 + 1.) * FLAME_H + 8.;
        div().id("plan-flame").flex_1().min_h_0().overflow_y_scroll().child(
            div()
                .w_full()
                .h(px(h))
                .px(px(8.))
                .pt(px(6.))
                .on_mouse_down(MouseButton::Left, cx.listener(|v, ev: &MouseDownEvent, window, cx| v.flame_click(ev, window, cx)))
                .child(
                    canvas(
                        move |b, _, _| bounds_cell.set(Some(b)),
                        move |b: Bounds<Pixels>, _, window, cx| {
                            let t = &theme;
                            let w = b.size.width.pt();
                            for f in &boxes {
                                let x = b.left() + px(f.x0 * w);
                                let bw = (f.x1 - f.x0) * w - 1.;
                                if bw < 1. {
                                    continue;
                                }
                                let y = b.top() + px(f.depth as f32 * FLAME_H);
                                let n = &p2.nodes[f.node];
                                let c = heat(t, n.share);
                                let (fill, ink) = if c == t.err {
                                    (t.err.opacity(0.85), t.on_err)
                                } else if c == t.warn {
                                    (t.warn.opacity(0.80), t.find_tx)
                                } else {
                                    (t.line2, t.tx2)
                                };
                                let r = Bounds::new(point(x, y), size(px(bw), px(FLAME_H - 2.)));
                                let selected = sel == Some(f.node);
                                window.paint_quad(gpui::quad(
                                    r,
                                    px(0.),
                                    fill,
                                    px(if selected { 1.5 } else { 0. }),
                                    t.acc,
                                    gpui::BorderStyle::Solid,
                                ));
                                // The label, if a few characters fit (11 pt at the face's advance).
                                let fits = ((bw - 8.) / (11. * crate::theme::advance())) as usize;
                                if fits >= 4 {
                                    let v = if p2.analyzed { fmt_ms(n.excl_ms) } else { fmt_cost(n.excl_cost) };
                                    let mut s = format!("{} · {v}", n.title);
                                    if s.chars().count() > fits {
                                        s = s.chars().take(fits.saturating_sub(1)).collect::<String>() + "…";
                                    }
                                    let run = gpui::TextRun { len: s.len(), font: gpui::font(mono()), color: ink, background_color: None, underline: None, strikethrough: None };
                                    let line = window.text_system().shape_line(SharedString::from(s), px(11.), &[run], None);
                                    let _ = line.paint(point(x + px(4.), y + px(1.)), px(FLAME_H - 4.), TextAlign::Left, None, window, cx);
                                }
                            }
                        },
                    )
                    .size_full(),
                ),
        )
    }

    fn render_details(&self, p: &Plan, t: &Theme) -> impl IntoElement + use<> {
        let body = div().flex().flex_col().gap(px(2.)).p(px(12.));
        let Some(i) = self.selected.filter(|&i| i < p.nodes.len()) else {
            return div().id("plan-details").w(px(DETAILS_W)).flex_none().h_full().border_l_1().border_color(t.line).child(body);
        };
        let n = &p.nodes[i];
        let kv = |k: String, v: String, c: Hsla| {
            div()
                .flex()
                .flex_row()
                .gap(px(10.))
                .py(px(2.))
                .text_size(px(11.5))
                .line_height(px(16.))
                .child(div().flex_none().w(px(146.)).text_color(t.tx3).child(k))
                .child(div().flex_1().min_w_0().text_color(c).child(v))
        };
        let mut sum: Vec<(String, String, Hsla)> = Vec::new();
        if p.analyzed && !n.never_executed() {
            sum.push(("self".into(), format!("{} · {:.0}%", fmt_ms(n.excl_ms), n.share * 100.), heat(t, n.share).max_contrast(t)));
            sum.push(("total".into(), fmt_ms(n.incl_ms), t.tx));
            if let (Some(a), Some(e)) = (n.act_rows(), n.est_rows()) {
                let mut s = format!("{} · est {}", fmt_count(a), fmt_count(e));
                if let Some(m) = n.misestimate().filter(|m| m.factor >= 10.) {
                    s.push_str(&format!(" · ×{}", fmt_count(m.factor.round())));
                }
                sum.push(("rows".into(), s, t.tx));
            }
            if let Some(l) = n.loops().filter(|&l| l != 1.) {
                sum.push(("loops".into(), fmt_count(l), t.tx));
            }
            if n.procs > 1. {
                sum.push(("processes".into(), format!("{}", n.procs), t.tx));
            }
        } else if n.never_executed() {
            sum.push(("loops".into(), "never executed".into(), t.tx3));
        } else if n.total_cost().is_some() {
            sum.push(("self cost".into(), format!("{} · {:.0}%", fmt_cost(n.excl_cost), n.share * 100.), t.tx));
            if let Some(e) = n.est_rows() {
                sum.push(("rows".into(), fmt_count(e), t.tx));
            }
        }
        if let (Some(a), Some(b)) = (n.startup_cost(), n.total_cost()) {
            sum.push(("cost".into(), format!("{a:.2}..{b:.2}"), t.tx));
        }
        let b = n.buffers();
        if b.shared_hit + b.shared_read > 0. {
            sum.push(("shared".into(), format!("hit {} · read {}", fmt_count(b.shared_hit), fmt_count(b.shared_read)), t.tx));
        }
        if b.temp_read + b.temp_written > 0. {
            sum.push(("temp".into(), format!("read {} · written {}", fmt_kb(b.temp_read * 8.), fmt_kb(b.temp_written * 8.)), t.warn));
        }
        let hints: Vec<Div> = p
            .hints
            .iter()
            .filter(|h| h.node == i)
            .map(|h| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .p(px(8.))
                    .bg(t.warn_s)
                    .text_size(px(11.5))
                    .line_height(px(16.))
                    .child(div().text_color(t.warn).child(h.text.clone()))
                    .children(h.fix.clone().map(|f| div().text_color(t.tx2).child(format!("→ {f}"))))
            })
            .collect();
        let props = n.props();
        div()
            .id("plan-details")
            .w(px(DETAILS_W))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(t.line)
            .overflow_y_scroll()
            .child(
                body.child(div().text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).text_color(t.tx).line_height(px(18.)).child(n.title.clone()))
                    .children(n.subplan().map(|s| div().text_size(px(11.)).text_color(t.tx3).child(s.to_string())))
                    .child(div().h(px(6.)))
                    .children(hints.into_iter().map(|h| h.mb(px(6.))))
                    .children(sum.into_iter().map(|(k, v, c)| kv(k, v, c)))
                    .child(div().my(px(8.)).h(px(1.)).bg(t.line))
                    .children(props.into_iter().map(|(k, v)| kv(k, v, t.tx2))),
            )
    }
}

trait Contrast {
    fn max_contrast(self, t: &Theme) -> Hsla;
}

impl Contrast for Hsla {
    /// The heat color for text: the plain one reads as body text.
    fn max_contrast(self, t: &Theme) -> Hsla {
        if self == t.tx3 { t.tx } else { self }
    }
}

/// The flame boxes: each node spans its subtree's exclusive time (cost without ANALYZE), its
/// children left to right under it, so the uncovered rest of a box is the node's own time.
fn flame_layout(p: &Plan) -> (Vec<FlameBox>, u16) {
    let n = p.nodes.len();
    let own = |i: usize| if p.analyzed { p.nodes[i].excl_ms } else { p.nodes[i].excl_cost };
    let mut sub: Vec<f64> = (0..n).map(own).collect();
    for i in (0..n).rev() {
        if let Some(q) = p.nodes[i].parent {
            sub[q] += sub[i];
        }
    }
    let roots: Vec<usize> = (0..n).filter(|&i| p.nodes[i].parent.is_none()).collect();
    let total: f64 = roots.iter().map(|&r| sub[r]).sum();
    if total <= 0. {
        return (Vec::new(), 0);
    }
    let mut out = Vec::with_capacity(n);
    let mut depth = 0;
    let mut stack: Vec<(usize, f64)> = Vec::new();
    let mut x = 0.;
    for &r in &roots {
        stack.push((r, x));
        x += sub[r] / total;
    }
    while let Some((i, x0)) = stack.pop() {
        let w = sub[i] / total;
        let d = p.nodes[i].depth;
        depth = depth.max(d);
        out.push(FlameBox { node: i, x0: x0 as f32, x1: (x0 + w) as f32, depth: d });
        let mut cx = x0;
        for &c in &p.nodes[i].children {
            stack.push((c, cx));
            cx += sub[c] / total;
        }
    }
    (out, depth)
}

impl Render for PlanView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let root = div()
            .size_full()
            .flex()
            .flex_row()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .font_family(mono())
            .on_action(cx.listener(|v, _: &SelectPrev, _, cx| v.step(-1, cx)))
            .on_action(cx.listener(|v, _: &SelectNext, _, cx| v.step(1, cx)))
            .on_action(cx.listener(|v, _: &Collapse, _, cx| v.collapse_or_parent(cx)))
            .on_action(cx.listener(|v, _: &Expand, _, cx| v.expand_or_child(cx)));
        let Some(p) = self.plan.clone() else { return root };
        if p.format == Format::Raw {
            // Nothing to parse: the server's text as it came.
            return root.child(
                div()
                    .id("plan-raw")
                    .size_full()
                    .overflow_y_scroll()
                    .p(px(12.))
                    .text_size(px(12.))
                    .line_height(px(18.))
                    .text_color(t.tx2)
                    .children(p.source.lines().map(|l| div().whitespace_nowrap().child(l.to_string())).collect::<Vec<_>>()),
            );
        }
        let focused = self.focus.contains_focused(window, cx);
        let main: gpui::AnyElement = if self.mode == Mode::Flame && self.has_flame() {
            self.render_flame(&p, &t, cx).into_any_element()
        } else {
            let theme = t.clone();
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(self.render_header(&p, &t))
                .child(
                    uniform_list(
                        "plan-tree",
                        self.rows.len(),
                        cx.processor(move |v, range: std::ops::Range<usize>, _window, cx| range.map(|ix| v.render_row(ix, &theme, focused, cx).into_any_element()).collect()),
                    )
                    .track_scroll(&self.scroll)
                    .flex_1(),
                )
                .into_any_element()
        };
        root.child(div().flex_1().min_w_0().h_full().flex().flex_col().children(self.render_hints(&p, &t, cx)).child(main)).child(self.render_details(&p, &t))
    }
}

// ---------------------------------------------------------------------------------------------
// The result bar of a plan: what kind of plan, the totals, tree ⇄ flame, copy as text.

impl crate::app::Workspace {
    pub(crate) fn render_plan_bar(&self, tab: &gpui::Entity<crate::tab::QueryTab>, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (view, rs) = {
            let tr = tab.read(cx);
            (tr.plan.clone(), tr.result().cloned())
        };
        let (plan, mode, has_flame) = {
            let v = view.read(cx);
            (v.plan().cloned(), v.mode, v.has_flame())
        };
        let analyzed = plan.as_ref().is_some_and(|p| p.analyzed);
        let (fg, bg) = crate::result_bar::role_colors(t, crate::result_bar::Role::Ok);
        let rolled_back = rs.as_ref().is_some_and(|r| r.rolled_back() && dataman_core::guard::classify(&r.sql).is_write());
        let mut metrics: Vec<(&str, String)> = Vec::new();
        if let Some(p) = &plan {
            let tt = &p.totals;
            if let Some(ms) = tt.planning_ms {
                metrics.push(("planning", fmt_ms(ms)));
            }
            if let Some(ms) = tt.execution_ms {
                metrics.push(("execution", fmt_ms(ms)));
            }
            if let Some(ms) = tt.trigger_ms() {
                metrics.push(("triggers", fmt_ms(ms)));
            }
            if let Some(j) = &tt.jit {
                metrics.push(("JIT", fmt_ms(j.total_ms)));
            }
            if tt.execution_ms.is_none()
                && let Some(e) = rs.as_ref().and_then(|r| r.status().elapsed)
            {
                metrics.push(("total", crate::app::fmt_dur(e)));
            }
        }
        let nodes = plan.as_ref().map_or(0, |p| p.nodes.len());
        let seg = |id: &'static str, label: &'static str, m: Mode| {
            let v = view.clone();
            ui::segment(t, id, label, mode == m).on_click(move |_, _, cx| v.update(cx, |v, cx| v.set_mode(m, cx)))
        };
        let ghost = |id: &'static str| ui::button(t, id, ui::Btn::Ghost).h(px(24.)).px(px(7.)).gap(px(5.)).text_size(px(11.5));
        div()
            .h(px(crate::theme::metrics::RESULT_BAR_H))
            .flex_none()
            .pl(px(10.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .border_b_1()
            .border_color(t.line)
            .text_color(t.tx2)
            .whitespace_nowrap()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .child(ui::pill("■", if analyzed { "ANALYZED" } else { "PLAN" }, fg, bg))
                    .when(nodes > 0, |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_baseline()
                                .gap(px(5.))
                                .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(t.tx).child(nodes.to_string()))
                                .child(div().text_color(t.tx3).child(if nodes == 1 { "node" } else { "nodes" })),
                        )
                    })
                    .children(metrics.into_iter().map(|(k, v)| {
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(5.))
                            .child(div().text_color(t.tx4).child("·"))
                            .child(div().text_color(t.tx3).child(k))
                            .child(div().text_color(t.tx).child(v))
                    }))
                    .when(rolled_back, |d| {
                        d.child(div().h(px(18.)).px(px(6.)).flex().items_center().bg(t.info_s).text_color(t.info).text_size(px(10.5)).child("rolled back"))
                    }),
            )
            .when(has_flame, |d| d.child(ui::segmented(t).h(px(24.)).child(seg("plan-tree-mode", "Tree", Mode::Tree)).child(seg("plan-flame-mode", "Flame", Mode::Flame))))
            .child(ui::vsep(t, 16.))
            .child(
                ghost("plan-copy")
                    .child(crate::assets::icon(crate::assets::Icon::Copy).size(px(13.)).text_color(t.tx2))
                    .child("Copy as text")
                    .on_click(cx.listener(|w, _, _, cx| w.copy_plan_text(cx))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(s: &str) -> Plan {
        Plan::parse(s)
    }

    #[test]
    fn flame_spans_nest_and_sum() {
        let p = plan(r#"[{"Plan":{"Node Type":"Hash Join","Actual Total Time":10.0,"Actual Loops":1,"Plans":[
            {"Node Type":"Seq Scan","Parent Relationship":"Outer","Relation Name":"a","Actual Total Time":6.0,"Actual Loops":1},
            {"Node Type":"Hash","Parent Relationship":"Inner","Actual Total Time":3.0,"Actual Loops":1,"Plans":[
              {"Node Type":"Seq Scan","Parent Relationship":"Outer","Relation Name":"b","Actual Total Time":2.0,"Actual Loops":1}]}]}}]"#);
        let (boxes, depth) = flame_layout(&p);
        assert_eq!(depth, 2);
        let get = |i: usize| boxes.iter().find(|b| b.node == i).copied().unwrap();
        assert_eq!((get(0).x0, get(0).x1), (0., 1.));
        // a: 6 of 10, from the left; the hash after it, b under the hash.
        assert!((get(1).x1 - 0.6).abs() < 1e-6);
        assert!((get(2).x0 - 0.6).abs() < 1e-6 && (get(2).x1 - 0.9).abs() < 1e-6);
        assert!((get(3).x0 - 0.6).abs() < 1e-6 && (get(3).x1 - 0.8).abs() < 1e-6);
        // No numbers (a DataFusion tree): no flame.
        assert!(flame_layout(&plan("A: x\n  B: y")).0.is_empty());
    }

    #[test]
    fn guides_follow_the_siblings() {
        let p = plan(r#"[{"Plan":{"Node Type":"Append","Plans":[
            {"Node Type":"Hash","Parent Relationship":"Member","Plans":[{"Node Type":"Result","Parent Relationship":"Outer"}]},
            {"Node Type":"Result","Parent Relationship":"Member"}]}}]"#);
        // Hash is not the last child: the line under it goes on past its own child.
        assert_eq!(PlanView::guides(&p, 1), (vec![], false));
        assert_eq!(PlanView::guides(&p, 2), (vec![true], true));
        assert_eq!(PlanView::guides(&p, 3), (vec![], true));
    }
}
