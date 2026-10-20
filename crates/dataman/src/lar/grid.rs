use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, FocusHandle, Focusable, Font, FontStyle, FontWeight,
    Hsla, KeyBinding, MouseButton, MouseDownEvent, Pixels, ScrollWheelEvent, ShapedLine, SharedString, Task,
    TextAlign, TextRun, Window, actions, canvas, div, fill, font, point, prelude::*, size,
};
use crate::theme::{Pt, px};

use crate::theme::{ActiveTheme, Theme};
use crate::ui;

pub const ROW_H: f32 = 22.;
const HEADER_H: f32 = 28.;
const PAD: f32 = 8.;
const BAR: f32 = 10.;
const GUTTER: f32 = 60.;
pub const FADE: Duration = Duration::from_millis(1200);
pub const GONE: Duration = Duration::from_millis(600);
const TICK: Duration = Duration::from_millis(33);

actions!(lar_grid, [Up, Down, CopyRow, Top]);

pub fn bind_keys(cx: &mut App) {
    let c = Some("LarGrid");
    cx.bind_keys([
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("cmd-c", CopyRow, c),
        KeyBinding::new("cmd-up", Top, c),
    ]);
}

#[derive(Clone, Debug)]
pub struct GCol {
    pub name: SharedString,
    pub width: f32,
    pub numeric: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowStyle {
    Plain,
    LiveNew,
    LiveChanged,
    LiveGone,
    Stale,
}

#[derive(Clone, Debug)]
pub struct GRow {
    pub id: u64,
    pub cells: Vec<Option<SharedString>>,
    pub changed: Vec<bool>,
    pub style: RowStyle,
    pub at: Option<Instant>,
}

impl GRow {
    fn deadline(&self) -> Option<Instant> {
        match (self.style, self.at) {
            (RowStyle::LiveGone, Some(a)) => Some(a + GONE),
            (RowStyle::LiveNew | RowStyle::LiveChanged, Some(a)) => Some(a + FADE),
            _ => None,
        }
    }

    fn patch(&mut self, p: RowPatch) {
        if let Some(c) = p.cells {
            self.cells = c;
        }
        self.style = p.style;
        self.changed = p.changed;
        self.at = p.at;
    }
}

#[derive(Clone, Debug)]
pub struct RowPatch {
    pub cells: Option<Vec<Option<SharedString>>>,
    pub style: RowStyle,
    pub changed: Vec<bool>,
    pub at: Option<Instant>,
}

pub fn fade_alpha(at: Instant, now: Instant) -> f32 {
    let t = now.saturating_duration_since(at).as_secs_f32() / FADE.as_secs_f32();
    if t >= 1. {
        0.
    } else {
        (1. - t).powf(1.6)
    }
}

pub fn fade_deadline(rows: &[GRow]) -> Option<Instant> {
    rows.iter().filter_map(GRow::deadline).max()
}

pub fn patch_rows(rows: &mut Vec<GRow>, mut patches: std::collections::HashMap<u64, RowPatch>, top: Vec<GRow>) -> Option<Instant> {
    let mut until = top.iter().filter_map(GRow::deadline).max();
    if !patches.is_empty() {
        for r in rows.iter_mut() {
            if let Some(p) = patches.remove(&r.id) {
                r.patch(p);
                until = until.max(r.deadline());
                if patches.is_empty() {
                    break;
                }
            }
        }
    }
    if !top.is_empty() {
        let mut v = top;
        v.append(rows);
        *rows = v;
    }
    until
}

pub fn guess_width(name: &str, sample: impl Iterator<Item = usize>) -> f32 {
    let chars = sample.take(400).max().unwrap_or(4).max(name.chars().count());
    (chars as f32 * char_w() + 2. * PAD + 2.).clamp(64., 360.)
}

/// One character of the 12 pt cells, a hair over the UI font's advance (7.25 at 0.6 em).
fn char_w() -> f32 {
    12. * crate::theme::advance() + 0.05
}

pub fn columns_for(names: &[String], rows: &[Vec<Option<String>>]) -> Vec<GCol> {
    names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let vals = rows.iter().take(400).filter_map(|r| r.get(i).and_then(|v| v.as_deref()));
            let numeric = {
                let mut any = false;
                let mut all = true;
                for v in vals.clone() {
                    any = true;
                    if v.parse::<f64>().is_err() {
                        all = false;
                        break;
                    }
                }
                any && all
            };
            GCol { name: n.clone().into(), width: guess_width(n, vals.map(|v| v.chars().count().min(48))), numeric }
        })
        .collect()
}

#[derive(Clone, Copy, Default)]
struct Geo {
    bounds: Option<Bounds<Pixels>>,
    body_w: f32,
    body_h: f32,
}

pub struct LarGrid {
    focus: FocusHandle,
    pub cols: Vec<GCol>,
    rows: Vec<GRow>,
    scroll: (f32, f32),
    geo: Geo,
    selected: Option<u64>,
    above: usize,
    fade_until: Option<Instant>,
    tick: Option<Task<()>>,
    pub empty: SharedString,
}

impl Focusable for LarGrid {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn mono() -> Font {
    font(crate::theme::mono())
}

fn run(len: usize, f: Font, color: Hsla) -> TextRun {
    TextRun { len, font: f, color, background_color: None, underline: None, strikethrough: None }
}

impl LarGrid {
    pub fn new(cx: &mut Context<Self>) -> Self {
        LarGrid {
            focus: cx.focus_handle(),
            cols: vec![],
            rows: vec![],
            scroll: (0., 0.),
            geo: Geo::default(),
            selected: None,
            above: 0,
            fade_until: None,
            tick: None,
            empty: "[ 0 ROWS ]".into(),
        }
    }

    pub fn selected(&self) -> Option<&GRow> {
        let id = self.selected?;
        self.rows.iter().find(|r| r.id == id)
    }

    pub fn select(&mut self, id: Option<u64>, cx: &mut Context<Self>) {
        self.selected = id;
        if let Some(i) = id.and_then(|id| self.rows.iter().position(|r| r.id == id))
            && self.geo.body_h > 0.
        {
            let top = i as f32 * ROW_H;
            if top < self.scroll.1 || top + ROW_H > self.scroll.1 + self.geo.body_h {
                self.scroll.1 = (top - self.geo.body_h / 3.).max(0.);
                self.clamp();
            }
        }
        cx.notify();
    }

    pub fn first_visible(&self) -> usize {
        (self.scroll.1 / ROW_H).floor() as usize
    }

    pub fn set_data(&mut self, cols: Vec<GCol>, rows: Vec<GRow>, cx: &mut Context<Self>) {
        self.cols = cols;
        self.rows = rows;
        self.scroll = (0., 0.);
        self.above = 0;
        self.selected = None;
        self.fade_until = fade_deadline(&self.rows);
        cx.notify();
    }

    pub fn replace_rows(&mut self, rows: Vec<GRow>, prepended: usize, cx: &mut Context<Self>) {
        let anchor = if self.scroll.1 > 0. {
            let first = self.first_visible();
            self.rows.get(first).map(|r| (r.id, self.scroll.1 - first as f32 * ROW_H))
        } else {
            None
        };
        self.rows = rows;
        if let Some((id, off)) = anchor {
            if let Some(i) = self.rows.iter().position(|r| r.id == id) {
                self.scroll.1 = i as f32 * ROW_H + off;
            }
            self.above += prepended;
        }
        self.clamp();
        if self.scroll.1 <= 0. {
            self.above = 0;
        }
        self.fade_until = fade_deadline(&self.rows);
        cx.notify();
    }

    pub fn patch(&mut self, patches: std::collections::HashMap<u64, RowPatch>, top: Vec<GRow>, cx: &mut Context<Self>) {
        let prepended = top.len();
        let anchor = (self.scroll.1 > 0. && prepended > 0).then(|| {
            let first = self.first_visible();
            (first, self.scroll.1 - first as f32 * ROW_H)
        });
        let until = patch_rows(&mut self.rows, patches, top);
        if let Some((first, off)) = anchor {
            self.scroll.1 = (first + prepended) as f32 * ROW_H + off;
            self.above += prepended;
        }
        self.clamp();
        if self.scroll.1 <= 0. {
            self.above = 0;
        }
        let now = Instant::now();
        self.fade_until = match (self.fade_until.filter(|u| *u > now), until) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        cx.notify();
    }

    pub fn scroll_to_top(&mut self, cx: &mut Context<Self>) {
        self.scroll.1 = 0.;
        self.above = 0;
        cx.notify();
    }

    fn content_h(&self) -> f32 {
        self.rows.len() as f32 * ROW_H
    }

    fn clamp(&mut self) {
        let max_y = (self.content_h() - self.geo.body_h).max(0.);
        let cw: f32 = self.cols.iter().map(|c| c.width).sum();
        let max_x = (cw - self.geo.body_w).max(0.);
        self.scroll.0 = self.scroll.0.clamp(0., max_x);
        self.scroll.1 = self.scroll.1.clamp(0., max_y);
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let d = ev.delta.pixel_delta(px(ROW_H));
        let (mut dx, mut dy) = (d.x.pt(), d.y.pt());
        if ev.modifiers.shift && dx == 0. {
            dx = dy;
            dy = 0.;
        }
        self.scroll.0 -= dx;
        self.scroll.1 -= dy;
        self.clamp();
        if self.scroll.1 <= 0. {
            self.above = 0;
        }
        cx.notify();
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        let Some(b) = self.geo.bounds else { return };
        let y = (ev.position.y - b.origin.y).pt() - HEADER_H;
        if y < 0. {
            return;
        }
        let r = ((y + self.scroll.1) / ROW_H).floor() as usize;
        let id = self.rows.get(r).map(|r| r.id);
        self.select(id, cx);
    }

    fn move_sel(&mut self, d: isize, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        let cur = self.selected.and_then(|id| self.rows.iter().position(|r| r.id == id));
        let next = match cur {
            Some(i) => (i as isize + d).clamp(0, self.rows.len() as isize - 1) as usize,
            None => 0,
        };
        let top = next as f32 * ROW_H;
        if top < self.scroll.1 {
            self.scroll.1 = top;
        } else if top + ROW_H > self.scroll.1 + self.geo.body_h {
            self.scroll.1 = top + ROW_H - self.geo.body_h;
        }
        self.clamp();
        let id = self.rows[next].id;
        self.select(Some(id), cx);
    }

    fn copy_row(&mut self, cx: &mut Context<Self>) {
        let Some(r) = self.selected() else { return };
        let line: Vec<String> = r.cells.iter().map(|c| c.as_ref().map(|s| s.to_string()).unwrap_or_else(|| "\\N".into())).collect();
        let header: Vec<String> = self.cols.iter().map(|c| c.name.to_string()).collect();
        cx.write_to_clipboard(ClipboardItem::new_string(format!("{}\n{}", header.join("\t"), line.join("\t"))));
    }

    fn arm_tick(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let Some(until) = self.fade_until else { return };
        if until <= now {
            self.fade_until = None;
            return;
        }
        if self.tick.is_some() {
            return;
        }
        self.tick = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TICK).await;
            let _ = this.update(cx, |g, cx| {
                g.tick = None;
                g.prune(Instant::now());
                cx.notify();
            });
        }));
    }

    fn prune(&mut self, now: Instant) {
        if !self.rows.iter().any(|r| r.style == RowStyle::LiveGone && r.at.is_some_and(|a| now >= a + GONE)) {
            return;
        }
        let first = self.first_visible();
        let gone_above = self.rows[..first.min(self.rows.len())]
            .iter()
            .filter(|r| r.style == RowStyle::LiveGone && r.at.is_some_and(|a| now >= a + GONE))
            .count();
        self.rows.retain(|r| !(r.style == RowStyle::LiveGone && r.at.is_some_and(|a| now >= a + GONE)));
        if self.scroll.1 > 0. {
            self.scroll.1 = (self.scroll.1 - gone_above as f32 * ROW_H).max(0.);
        }
        self.clamp();
        self.fade_until = fade_deadline(&self.rows);
    }

    fn prepare(&mut self, bounds: Bounds<Pixels>, window: &mut Window, t: &Theme) -> Frame {
        let w = bounds.size.width.pt();
        let h = bounds.size.height.pt();
        self.geo = Geo { bounds: Some(bounds), body_w: (w - GUTTER - BAR).max(0.), body_h: (h - HEADER_H - BAR).max(0.) };
        self.clamp();
        let ts = window.text_system().clone();
        let now = Instant::now();
        let normal = mono();
        let italic = Font { style: FontStyle::Italic, ..mono() };
        let bold = Font { weight: FontWeight::SEMIBOLD, ..mono() };
        let shape = |text: String, f: &Font, sz: f32, color: Hsla| -> ShapedLine {
            let len = text.len();
            ts.shape_line(SharedString::from(text), px(sz), &[run(len, f.clone(), color)], None)
        };
        let shape_runs = |text: String, runs: &[TextRun], sz: f32| -> ShapedLine { ts.shape_line(SharedString::from(text), px(sz), runs, None) };
        let char_w = char_w();
        let fit = |s: &str, n: usize| -> String {
            let n = n.max(1);
            let mut out = String::new();
            for (i, ch) in s.chars().enumerate() {
                if i + 1 == n && s.chars().nth(n).is_some() {
                    out.push('…');
                    break;
                }
                out.push(match ch {
                    '\n' | '\r' | '\t' => ' ',
                    c => c,
                });
            }
            out
        };
        let (sx, sy) = self.scroll;
        let first = (sy / ROW_H).floor() as usize;
        let visible = (self.geo.body_h / ROW_H).ceil() as usize + 1;
        let last = (first + visible).min(self.rows.len());
        let mut cols = Vec::new();
        let mut x = 0.;
        for (i, c) in self.cols.iter().enumerate() {
            if x + c.width > sx && x < sx + self.geo.body_w {
                cols.push((i, x - sx, c.width));
            }
            x += c.width;
        }
        let mut rows = Vec::with_capacity(last.saturating_sub(first));
        for ri in first..last {
            let r = &self.rows[ri];
            let y = ri as f32 * ROW_H - sy;
            let alpha = r.at.map(|a| fade_alpha(a, now)).unwrap_or(0.);
            let selected = self.selected == Some(r.id);
            let (bg, fg, strike, opacity): (Option<Hsla>, Hsla, bool, f32) = match r.style {
                RowStyle::Plain => (None, t.tx, false, 1.),
                RowStyle::LiveNew => ((alpha > 0.).then(|| t.lar_s.opacity(alpha)), t.tx, false, 1.),
                RowStyle::LiveChanged => (None, t.tx, false, 1.),
                RowStyle::LiveGone => (None, t.tx3, true, 0.45),
                RowStyle::Stale => (None, t.tx3, false, 1.),
            };
            let fg = if opacity < 1. { fg.opacity(opacity) } else { fg };
            let mut cells = Vec::new();
            let mut tints = Vec::new();
            for &(c, cx_, wd) in &cols {
                let col = &self.cols[c];
                let changed = r.changed.get(c).copied().unwrap_or(false);
                if changed && r.style == RowStyle::LiveChanged && alpha > 0. {
                    tints.push((cx_, wd, t.lar_s.opacity(alpha), t.lar.opacity(alpha)));
                }
                let room = ((wd - 2. * PAD) / char_w).floor().max(1.) as usize;
                let value = r.cells.get(c).cloned().flatten();
                let line = match value {
                    None => (shape("NULL".into(), &italic, 12., t.null.opacity(opacity)), false),
                    Some(v) => {
                        let s = fit(&v, room);
                        let len = s.len();
                        let runs = [TextRun {
                            len,
                            font: normal.clone(),
                            color: fg,
                            background_color: None,
                            underline: None,
                            strikethrough: strike.then(|| gpui::StrikethroughStyle { thickness: px(1.), color: Some(fg) }),
                        }];
                        (shape_runs(s, &runs, 12.), col.numeric)
                    }
                };
                cells.push((cx_, wd, line.0, line.1, r.cells.get(c).is_some_and(|v| v.is_none())));
            }
            let mark = match r.style {
                RowStyle::LiveNew if alpha > 0. => Some(("+", t.lar)),
                RowStyle::LiveGone => Some(("−", t.err)),
                _ => None,
            };
            let gutter = shape(crate::grid::ui_count(ri + 1), &normal, 11., if selected { t.tx } else { t.tx4 });
            let mark = mark.map(|(g, c)| shape(g.into(), &bold, 12., c));
            let left_bar = (r.style == RowStyle::LiveNew && alpha > 0.).then(|| t.lar.opacity(alpha));
            rows.push(FrameRow { y, bg, selected, cells, tints, gutter, mark, left_bar, alt: ri % 2 == 1 });
        }
        let headers = cols
            .iter()
            .map(|&(c, cx_, wd)| {
                let col = &self.cols[c];
                let room = ((wd - 2. * PAD) / char_w).floor().max(1.) as usize;
                (cx_, wd, shape(fit(&col.name, room), &bold, 12., t.tx), col.numeric)
            })
            .collect();
        let pill = (self.above > 0).then(|| {
            shape(format!("{} new row{} above ↑", crate::grid::ui_count(self.above), if self.above == 1 { "" } else { "s" }), &normal, 11., t.on_lar)
        });
        let hash = shape("#".into(), &normal, 10.5, t.tx4);
        Frame { rows, headers, pill, hash, content_h: self.content_h(), cw: self.cols.iter().map(|c| c.width).sum() }
    }
}

struct FrameRow {
    y: f32,
    bg: Option<Hsla>,
    selected: bool,
    cells: Vec<(f32, f32, ShapedLine, bool, bool)>,
    tints: Vec<(f32, f32, Hsla, Hsla)>,
    gutter: ShapedLine,
    mark: Option<ShapedLine>,
    left_bar: Option<Hsla>,
    alt: bool,
}

struct Frame {
    rows: Vec<FrameRow>,
    headers: Vec<(f32, f32, ShapedLine, bool)>,
    pill: Option<ShapedLine>,
    hash: ShapedLine,
    content_h: f32,
    cw: f32,
}

fn paint(grid: &Entity<LarGrid>, frame: Frame, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    let t = cx.theme();
    let g = grid.read(cx);
    let focused = g.focus.is_focused(window);
    let (sx, sy) = g.scroll;
    let geo = g.geo;
    let widths: Vec<f32> = g.cols.iter().map(|c| c.width).collect();
    let o = bounds.origin;
    let at = |x: f32, y: f32| point(o.x + px(x), o.y + px(y));
    let rect = |x: f32, y: f32, w: f32, h: f32| Bounds::new(at(x, y), size(px(w), px(h)));
    let full_w = bounds.size.width.pt();
    let lh = px(ROW_H);
    // No background: the work surface paints the fill once (see `grid::paint_frame`).
    let body = rect(GUTTER, HEADER_H, geo.body_w, geo.body_h);
    window.with_content_mask(Some(gpui::ContentMask { bounds: body }), |window| {
        for r in &frame.rows {
            let y = HEADER_H + r.y;
            let zebra = Some(if r.alt { t.alt } else { t.alt_base }).filter(|c| c.a > 0.);
            let bg = if r.selected { Some(if focused { t.sel } else { t.prs }) } else { r.bg.or(zebra) };
            if let Some(bg) = bg {
                window.paint_quad(fill(rect(GUTTER, y, geo.body_w, ROW_H), bg));
            }
            for &(x, w, c, line) in &r.tints {
                window.paint_quad(fill(rect(GUTTER + x, y, w, ROW_H), c));
                window.paint_quad(fill(rect(GUTTER + x, y + ROW_H - 2., w, 2.), line));
            }
            for (x, w, _, _, null) in &r.cells {
                if *null {
                    window.paint_quad(fill(rect(GUTTER + x, y, *w, ROW_H), t.null_b));
                }
            }
            if let Some(c) = r.left_bar {
                window.paint_quad(fill(rect(GUTTER, y, 3., ROW_H), c));
            }
        }
        let rows_h = (frame.content_h - sy).clamp(0., geo.body_h);
        let mut x = -sx;
        for w in &widths {
            x += w;
            if x > 0. && x < geo.body_w + 1. {
                window.paint_quad(fill(rect(GUTTER + x - 1., HEADER_H, 1., rows_h), t.grid_line));
            }
        }
        for r in &frame.rows {
            let y = HEADER_H + r.y;
            for (x, w, line, right, _) in &r.cells {
                let lx = if *right { x + w - PAD - line.width.pt() } else { x + PAD };
                let cell = rect(GUTTER + x, y, w - 1., ROW_H);
                window.with_content_mask(Some(gpui::ContentMask { bounds: cell }), |window| {
                    let _ = line.paint(at(GUTTER + lx, y), lh, TextAlign::Left, None, window, cx);
                });
            }
        }
    });
    window.with_content_mask(Some(gpui::ContentMask { bounds: rect(0., HEADER_H, GUTTER, geo.body_h) }), |window| {
        for r in &frame.rows {
            let y = HEADER_H + r.y;
            if r.selected {
                window.paint_quad(fill(rect(0., y, GUTTER, ROW_H), if focused { t.acc_s } else { t.prs }));
            }
            if let Some(m) = &r.mark {
                let _ = m.paint(at(8., y), lh, TextAlign::Left, None, window, cx);
            }
            let _ = r.gutter.paint(at(GUTTER - 10. - r.gutter.width.pt(), y), lh, TextAlign::Left, None, window, cx);
        }
    });
    let _ = frame.hash.paint(at(GUTTER - 10. - frame.hash.width.pt(), 0.), px(HEADER_H), TextAlign::Left, None, window, cx);
    window.with_content_mask(Some(gpui::ContentMask { bounds: rect(GUTTER, 0., geo.body_w + BAR, HEADER_H) }), |window| {
        for (x, w, name, right) in &frame.headers {
            let x0 = GUTTER + x;
            let lx = if *right { (x0 + w - PAD - name.width.pt()).max(x0 + PAD) } else { x0 + PAD };
            let _ = name.paint(at(lx, 0.), px(HEADER_H), TextAlign::Left, None, window, cx);
            window.paint_quad(fill(rect(x0 + w - 1., 0., 1., HEADER_H), t.line));
        }
    });
    window.paint_quad(fill(rect(0., HEADER_H - 1., full_w, 1.), t.line2));
    window.paint_quad(fill(rect(GUTTER - 1., 0., 1., bounds.size.height.pt()), t.line));
    let thumb = |b: Bounds<Pixels>| fill(b, t.line2);
    if frame.content_h > geo.body_h && frame.content_h > 0. {
        let hh = (geo.body_h * geo.body_h / frame.content_h).max(24.);
        let top = (sy / (frame.content_h - geo.body_h).max(1.)) * (geo.body_h - hh);
        window.paint_quad(thumb(rect(GUTTER + geo.body_w + 2., HEADER_H + top, 6., hh)));
    }
    if frame.cw > geo.body_w && frame.cw > 0. {
        let ww = (geo.body_w * geo.body_w / frame.cw).max(24.);
        let left = (sx / (frame.cw - geo.body_w).max(1.)) * (geo.body_w - ww);
        window.paint_quad(thumb(rect(GUTTER + left, HEADER_H + geo.body_h + 2., ww, 6.)));
    }
    if let Some(p) = &frame.pill {
        let w = p.width.pt() + 20.;
        let x = GUTTER + (geo.body_w - w) / 2.;
        let b = rect(x, HEADER_H + 6., w, 20.);
        window.paint_quad(fill(b, t.lar));
        let _ = p.paint(at(x + 10., HEADER_H + 6.), px(20.), TextAlign::Left, None, window, cx);
    }
}

impl Render for LarGrid {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        self.arm_tick(cx);
        let entity = cx.entity();
        let entity2 = entity.clone();
        let theme = t.clone();
        let pill_hit = self.above > 0;
        let mut root = div()
            .id("lar-grid")
            .size_full()
            .relative()
            .key_context("LarGrid")
            .track_focus(&self.focus)
            .font_family(crate::theme::mono())
            .text_size(px(12.))
            .on_action(cx.listener(|g, _: &Up, _, cx| g.move_sel(-1, cx)))
            .on_action(cx.listener(|g, _: &Down, _, cx| g.move_sel(1, cx)))
            .on_action(cx.listener(|g, _: &Top, _, cx| g.scroll_to_top(cx)))
            .on_action(cx.listener(|g, _: &CopyRow, _, cx| g.copy_row(cx)))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down));
        if !self.cols.is_empty() {
            root = root.child(
                canvas(
                    move |bounds, window, cx| entity.update(cx, |g, _| g.prepare(bounds, window, &theme)),
                    move |bounds, frame, window, cx| paint(&entity2, frame, bounds, window, cx),
                )
                .size_full(),
            );
        }
        if pill_hit {
            root = root.child(
                div()
                    .id("lar-grid-above")
                    .absolute()
                    .top(px(HEADER_H + 6.))
                    .left(px(GUTTER))
                    .right(px(BAR))
                    .h(px(20.))
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .id("lar-grid-above-hit")
                            .w(px(180.))
                            .h_full()
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, cx.listener(|g, _, _, cx| {
                                cx.stop_propagation();
                                g.scroll_to_top(cx);
                            })),
                    ),
            );
        }
        if self.rows.is_empty() {
            root = root.child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(if self.cols.is_empty() { 0. } else { HEADER_H }))
                    .bottom_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(ui::empty_state(&t, &self.empty, 22., None)),
            );
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(id: u64, cells: Vec<Option<SharedString>>) -> GRow {
        GRow { id, cells, changed: vec![], style: RowStyle::Plain, at: None }
    }

    #[test]
    fn fade_runs_once_and_ends() {
        let t0 = Instant::now();
        assert!((fade_alpha(t0, t0) - 1.).abs() < 1e-6);
        let mid = fade_alpha(t0, t0 + Duration::from_millis(600));
        assert!(mid > 0. && mid < 1.);
        assert!(fade_alpha(t0, t0 + Duration::from_millis(900)) < mid);
        assert_eq!(fade_alpha(t0, t0 + FADE), 0.);
        assert_eq!(fade_alpha(t0, t0 + Duration::from_secs(5)), 0.);
    }

    #[test]
    fn deadline_covers_fades_and_removed_rows() {
        let t0 = Instant::now();
        let mut a = plain(1, vec![]);
        assert_eq!(fade_deadline(std::slice::from_ref(&a)), None);
        a.style = RowStyle::LiveNew;
        a.at = Some(t0);
        let mut b = plain(2, vec![]);
        b.style = RowStyle::LiveGone;
        b.at = Some(t0 + Duration::from_millis(900));
        assert_eq!(fade_deadline(&[a.clone(), b]), Some(t0 + Duration::from_millis(1500)));
        assert_eq!(fade_deadline(&[a]), Some(t0 + FADE));
    }

    #[test]
    fn patches_touch_only_their_rows() {
        let t0 = Instant::now();
        let mut rows: Vec<GRow> = (1..=5).map(|i| plain(i, vec![Some(i.to_string().into())])).collect();
        let mut p = std::collections::HashMap::new();
        p.insert(3, RowPatch { cells: Some(vec![Some("x".into())]), style: RowStyle::LiveChanged, changed: vec![true], at: Some(t0) });
        p.insert(5, RowPatch { cells: None, style: RowStyle::LiveGone, changed: vec![], at: Some(t0) });
        let mut top = plain(9, vec![Some("new".into())]);
        top.style = RowStyle::LiveNew;
        top.at = Some(t0);
        let until = patch_rows(&mut rows, p, vec![top]);
        assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![9, 1, 2, 3, 4, 5]);
        assert_eq!(rows[3].cells[0].as_deref(), Some("x"));
        assert_eq!(rows[3].style, RowStyle::LiveChanged);
        assert_eq!(rows[5].style, RowStyle::LiveGone);
        assert_eq!(rows[5].cells[0].as_deref(), Some("5"), "a removed row keeps its values while it fades");
        assert_eq!(rows[1].style, RowStyle::Plain);
        assert_eq!(until, Some(t0 + FADE));
    }

    #[test]
    fn columns_guess_numbers_and_widths() {
        let names = vec!["id".to_string(), "email".into(), "total".into()];
        let rows = vec![
            vec![Some("1".into()), Some("someone@example.com".into()), Some("0.03".into())],
            vec![Some("22".into()), None, Some("0.02".into())],
        ];
        let c = columns_for(&names, &rows);
        assert!(c[0].numeric && !c[1].numeric && c[2].numeric);
        assert!(c[1].width > c[0].width);
        assert!(c.iter().all(|c| (64. ..=360.).contains(&c.width)));
    }
}
