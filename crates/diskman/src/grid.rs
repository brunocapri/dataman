//! The result grid: a canvas that draws only the visible cells of a [`ResultSet`].
//!
//! The cost of a frame depends on the window size, not on the result: a million rows scroll
//! like ten. Cells are formatted and shaped only when visible; the fetcher is told how far the
//! user has scrolled so it keeps reading ahead. Per row the grid paints at most one
//! background quad (every other row) plus its text runs; column borders are one quad per
//! column, not per cell (Spec, performance block).
//!
//! Owners can decorate the grid without touching its drawing code: see [`Decorations`] (row and
//! cell tints, changed flags, struck rows, gutter marks, override text, virtual rows after the
//! result) and [`CellOverlay`] (a view placed over one cell, e.g. an inline editor).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, AnyView, App, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, Entity, EventEmitter, FocusHandle, Focusable,
    Font, FontStyle, FontWeight, Hsla, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    Point, ScrollWheelEvent, ShapedLine, SharedString, StrikethroughStyle, Subscription, Task, TextAlign, TextRun, Window, actions,
    anchored, canvas, deferred, div, fill, font, point, prelude::*, size,
};
use crate::theme::{Pt, px};
use diskman_core::find::{CellHits, Matcher, nearest_index, next_index, prev_index, search_rows};
use diskman_core::fknav::{FkJump, ForeignKeys};
use diskman_core::result::{CopyFormat, Phase, ResultSet, SortDir, Snapshot};
use diskman_core::value::Kind;

use crate::assets::Icon;
use crate::find::{self, Count, FindBar, FindBarEvent};
use crate::grid_menu::{self, Entry as MenuEntry, Item as MenuItem, Press, Release};
use crate::motion::{FindPop, PopFrame, PopStyle};
use crate::theme::{ActiveTheme, Theme};
use crate::ui;

actions!(
    grid,
    [
        MoveUp,
        MoveDown,
        MoveLeft,
        MoveRight,
        SelectUp,
        SelectDown,
        SelectLeft,
        SelectRight,
        PageUp,
        PageDown,
        Top,
        Bottom,
        /// ⌘← / ⌘→: the row's first / last column (as a text's line start / end); with ⇧ the
        /// selection grows to it.
        RowStart,
        RowEnd,
        SelectRowStart,
        SelectRowEnd,
        SelectAll,
        CopyTsv,
        CopyCsv,
        CopyTsvWithHeader,
        FetchMore,
        FetchAll,
        ToggleViewer,
        EditCell,
        InsertRow,
        DeleteRows,
        /// ⌥↓ on a reference (a foreign-key value): the "Go to …" menu under the cell.
        OpenCellMenu,
        MenuUp,
        MenuDown,
        MenuConfirm,
        MenuClose,
    ]
);

/// The grid's bindings. The inline cell editor lives inside the grid's key context, so they
/// are off while it is open: there the keys type (`[`, `]`, ⇧arrows…).
pub const GRID_KEYS: &str = "ResultGrid && !GridCellEditor";

/// The key context of the open cell menu (it holds the focus while open; the grid keeps
/// drawing as focused).
pub const MENU_CONTEXT: &str = "GridMenu";

pub fn bind_keys(cx: &mut App) {
    let c = Some(GRID_KEYS);
    cx.bind_keys([
        KeyBinding::new("up", MoveUp, c),
        KeyBinding::new("down", MoveDown, c),
        KeyBinding::new("left", MoveLeft, c),
        KeyBinding::new("right", MoveRight, c),
        KeyBinding::new("shift-up", SelectUp, c),
        KeyBinding::new("shift-down", SelectDown, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("pageup", PageUp, c),
        KeyBinding::new("pagedown", PageDown, c),
        KeyBinding::new("cmd-up", Top, c),
        // Deeper than the workspace's keys: on the grid they are the row's ends.
        KeyBinding::new("cmd-left", RowStart, c),
        KeyBinding::new("cmd-right", RowEnd, c),
        KeyBinding::new("cmd-shift-left", SelectRowStart, c),
        KeyBinding::new("cmd-shift-right", SelectRowEnd, c),
        KeyBinding::new("home", Top, c),
        KeyBinding::new("end", Bottom, c),
        KeyBinding::new("cmd-a", SelectAll, c),
        // Copy, fetch, the viewer and ⌘I are remappable commands (`keymap`).
        KeyBinding::new("enter", EditCell, c),
        KeyBinding::new("backspace", DeleteRows, c),
        KeyBinding::new("delete", DeleteRows, c),
        KeyBinding::new("alt-down", OpenCellMenu, c),
        KeyBinding::new("cmd-f", find::Find, c),
        KeyBinding::new("cmd-g", find::FindNext, c),
    ]);
    // While the find bar is open: ⇧⌘G is "previous match" (else "focus the grid") and esc closes
    // the bar (else "back to the editor").
    let f = Some("ResultGrid && find && !GridCellEditor");
    cx.bind_keys([KeyBinding::new("cmd-shift-g", find::FindPrevious, f), KeyBinding::new("escape", find::CloseFind, f)]);
    let m = Some(MENU_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", MenuUp, m),
        KeyBinding::new("down", MenuDown, m),
        KeyBinding::new("enter", MenuConfirm, m),
        KeyBinding::new("space", MenuConfirm, m),
        KeyBinding::new("escape", MenuClose, m),
        KeyBinding::new("alt-down", MenuClose, m),
        KeyBinding::new("alt-up", MenuClose, m),
    ]);
}

pub enum GridEvent {
    /// ⇧Space: open or close the cell viewer.
    ToggleViewer,
    /// ↩ or a double click on a cell: edit it (display row, result column).
    EditCell { row: usize, col: usize },
    /// ⌘I: a new row at the end.
    InsertRow,
    /// ⌫: mark the selected display rows for deletion (or unmark them).
    DeleteRows(Vec<usize>),
    /// "Go to …" on a reference (the menu, ⌘-click): open the referenced row in a new tab.
    FollowFk(FkJump),
    /// A table's data view: sort on the server by result column `col` (`None`: no sort).
    ServerSort { col: usize, dir: Option<SortDir> },
    /// A table's data view: the filter popover for result column `col`, its top-left corner at
    /// `at` (window coordinates).
    Filter { col: usize, at: (f32, f32) },
    /// A table's data view: rows whose column equals this value (`None`: IS NULL).
    FilterValue { col: usize, value: Option<String> },
}

/// A table's data view, as the grid needs it: header clicks sort on the server (`sort` is the
/// current one, by result column) and row numbers count from the page's first row.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableMode {
    pub sort: Option<(usize, SortDir)>,
    pub offset: usize,
}

/// Which columns show (in the result's order), hidden ones left out. Indexes are result
/// columns; the grid works on display columns and maps them through `disp` / `pos`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ColLayout {
    hidden: Vec<bool>,
    /// Display → result column.
    disp: Vec<usize>,
    /// Result → display column (`usize::MAX` when hidden).
    pos: Vec<usize>,
}

impl ColLayout {
    /// The identity layout for `n` columns (kept as is when it already has `n`).
    pub fn ensure(&mut self, n: usize) {
        if self.hidden.len() == n && self.pos.len() == n {
            return;
        }
        *self = ColLayout { hidden: vec![false; n], ..Default::default() };
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let n = self.hidden.len();
        self.disp = (0..n).filter(|&c| !self.hidden[c]).collect();
        self.pos = vec![usize::MAX; n];
        for (d, &c) in self.disp.iter().enumerate() {
            self.pos[c] = d;
        }
    }

    /// Display columns.
    pub fn shown(&self) -> usize {
        self.disp.len()
    }

    pub fn result_col(&self, d: usize) -> Option<usize> {
        self.disp.get(d).copied()
    }

    pub fn display_col(&self, c: usize) -> Option<usize> {
        self.pos.get(c).copied().filter(|&d| d != usize::MAX)
    }

    pub fn hidden_count(&self) -> usize {
        self.hidden.iter().filter(|h| **h).count()
    }

    /// Hides a column; the last one shown stays.
    pub fn hide(&mut self, c: usize) -> bool {
        if c >= self.hidden.len() || self.hidden[c] || self.shown() <= 1 {
            return false;
        }
        self.hidden[c] = true;
        self.rebuild();
        true
    }

    pub fn show_all(&mut self) {
        self.hidden.iter_mut().for_each(|h| *h = false);
        self.rebuild();
    }
}

const ROW_H: f32 = 22.;
const HEADER_H: f32 = 28.;
const PAD: f32 = 8.;
const BAR: f32 = 10.;
const MIN_W: f32 = 48.;
const MAX_AUTO_W: f32 = 420.;
const GUTTER_MIN: f32 = 60.;
/// Grab zone on each side of a header border for resizing.
const RESIZE_GRAB: f32 = 4.;
/// Selections up to this many cells get a sum and an average in the result bar.
const SUMMARY_MAX_CELLS: usize = 5_000;

fn type_width(k: Kind) -> f32 {
    match k {
        Kind::Bool => 72.,
        Kind::Int => 84.,
        Kind::Float | Kind::Float4 | Kind::Numeric => 96.,
        Kind::Date => 110.,
        Kind::Time => 130.,
        Kind::Timestamp | Kind::TimestampTz => 206.,
        Kind::Json => 290.,
        Kind::Text | Kind::Bytes => 200.,
        Kind::Other => 160.,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellPos {
    row: usize,
    col: usize,
}

#[derive(Clone, Copy, Debug)]
struct Selection {
    anchor: CellPos,
    head: CellPos,
    /// Whole rows (clicked in the row-number gutter).
    rows: bool,
}

impl Selection {
    fn rows(&self) -> std::ops::RangeInclusive<usize> {
        self.anchor.row.min(self.head.row)..=self.anchor.row.max(self.head.row)
    }
    fn cols(&self, ncols: usize) -> std::ops::RangeInclusive<usize> {
        if self.rows {
            0..=ncols.saturating_sub(1)
        } else {
            self.anchor.col.min(self.head.col)..=self.anchor.col.max(self.head.col)
        }
    }
    /// One clicked value (not a range, not whole rows): drawn with rounded corners.
    fn single(&self) -> bool {
        !self.rows && self.anchor == self.head
    }
}

/// Corner radius of the active cell's ring (always) and of a single selected cell's fill; a
/// range's fill stays square, so its cells join into one block.
const CELL_RADIUS: f32 = 4.;

#[derive(Clone, Copy, Debug)]
enum Drag {
    Cells,
    Rows,
    Resize { col: usize, start_x: f32, start_w: f32 },
    /// `grab` = offset of the pointer inside the thumb.
    VThumb { grab: f32 },
    HThumb { grab: f32 },
    /// Pressed on a header cell (display column); a release without movement sorts.
    Header { col: usize, x: f32 },
}

/// Layout of the last frame, in window coordinates.
#[derive(Clone, Copy, Debug, Default)]
struct Geometry {
    bounds: Bounds<Pixels>,
    gutter: f32,
    /// Cells area (without header, gutter, scrollbars).
    body_w: f32,
    body_h: f32,
    content_w: f32,
    /// f64: at millions of rows an f32 pixel offset loses whole pixels.
    content_h: f64,
}

pub struct ResultGrid {
    focus: FocusHandle,
    rs: Option<Arc<ResultSet>>,
    rt: Option<tokio::runtime::Handle>,
    widths: Vec<f32>,
    /// Horizontal and vertical offsets in pixels (vertical in f64, see `content_h`).
    scroll: (f32, f64),
    geo: Geometry,
    sel: Option<Selection>,
    drag: Option<Drag>,
    char_w: f32,
    /// The `char_w` the column widths were made for: after a font switch the next frame scales
    /// them (auto and resized alike) to the new face, so each keeps its characters.
    widths_at: f32,
    sorting: bool,
    /// The tab asked the server to cancel (the message row says so).
    pub cancelling: bool,
    summary: Option<(SummaryKey, SharedString)>,
    /// Extra paint from the owner (edits, live rows, diffs); `None` costs nothing.
    decor: Option<Arc<Decorations>>,
    overlay: Option<CellOverlay>,
    /// Result rows at the last frame: when more arrive, a selection on virtual rows follows them.
    seen_rows: usize,
    _watch: Option<Task<()>>,
    /// The references of the current result (`diskman_core::fknav`), once resolved; `fk_cols`
    /// flags them per column for painting.
    fk: Option<Arc<ForeignKeys>>,
    fk_cols: Arc<Vec<bool>>,
    /// A plain click on a cell whose release may open the cell menu (cleared by a drag).
    press: Option<Press>,
    menu: Option<CellMenu>,
    menu_focus: FocusHandle,
    _menu_blur: Option<Subscription>,
    /// The find indicator on a cell: blue on a clicked value, yellow on the row an FK "Go to"
    /// landed on (`motion::FindPop`, evaluated in `prepare`).
    pop: Option<FindPop<CellPos>>,
    /// Pop on this column (by name) of the first row once rows arrive: the key of the row an FK
    /// "Go to" opened this tab on. Survives `set_result` (the tab may run after this is set).
    land: Option<String>,
    /// When cells last went to the clipboard: the result bar's Copy button says "Copied" for a
    /// moment (`crate::motion::copied`).
    pub(crate) copied_at: Option<Instant>,
    _copied: Option<Task<()>>,
    /// ⌘F: the find bar and the matching cells of the loaded rows.
    find: Option<GridFind>,
    /// Hidden and moved columns (display ↔ result column).
    layout: ColLayout,
    /// The layout, widths and horizontal scroll of the result before this one: a result with
    /// the same columns (the next page of a table, a re-run) takes them over.
    kept: Option<(Vec<String>, ColLayout, Vec<f32>, f32, f32)>,
    /// A table's data view (server-side sort, page offset).
    table: Option<TableMode>,
    /// Widths the owner wants for a fresh result (`prefer_widths`).
    preferred: Option<Vec<f32>>,
    /// A cell or row selection dragged near or past an edge: where the pointer was last seen
    /// (window coordinates, also outside the grid and the window), and the timer that keeps
    /// scrolling while it stays there.
    auto: Option<AutoScroll>,
}

/// Drag-select autoscroll (Excel, Numbers): the grid scrolls towards the pointer while a
/// selection is dragged into the last `EDGE_ZONE` pt before an edge of the cells or past it,
/// faster the farther it is, and the selection follows the cell under the pointer.
struct AutoScroll {
    at: Point<Pixels>,
    last: Instant,
    _tick: Option<Task<()>>,
}

/// The band inside each edge of the cells where a dragged selection starts scrolling.
const EDGE_ZONE: f32 = 24.;
/// The autoscroll timer's period.
const AUTO_TICK: Duration = Duration::from_millis(16);

/// Autoscroll speed (pt/s) for a pointer `d` pt into the edge band (`d` grows past the edge):
/// slow in the band, a few hundred at the edge, thousands well past it.
pub(crate) fn edge_speed(d: f32) -> f32 {
    if d <= 0. {
        return 0.;
    }
    (60. + 8. * d + 0.08 * d * d).min(8000.)
}

/// The signed autoscroll speed along one axis for pointer `p` and the cells' span `lo..hi`
/// (negative towards `lo`).
pub(crate) fn axis_speed(p: f32, lo: f32, hi: f32) -> f32 {
    let zone = EDGE_ZONE.min((hi - lo).max(0.) / 4.);
    if p > hi - zone {
        edge_speed(p - (hi - zone))
    } else if p < lo + zone {
        -edge_speed(lo + zone - p)
    } else {
        0.
    }
}

/// What a result tab keeps of the grid while another result is on screen.
pub struct GridView {
    rs: Option<Arc<ResultSet>>,
    widths: Vec<f32>,
    widths_at: f32,
    scroll: (f32, f64),
    sel: Option<Selection>,
    fk: Option<Arc<ForeignKeys>>,
    fk_cols: Arc<Vec<bool>>,
    layout: ColLayout,
}

impl GridView {
    /// A result not shown yet.
    pub fn fresh(rs: Arc<ResultSet>) -> Self {
        GridView { rs: Some(rs), ..Self::empty() }
    }

    /// No result (the messages tab is on screen).
    pub fn empty() -> Self {
        GridView {
            rs: None,
            widths: Vec::new(),
            widths_at: 0.,
            scroll: (0., 0.),
            sel: None,
            fk: None,
            fk_cols: Arc::new(Vec::new()),
            layout: ColLayout::default(),
        }
    }

    /// References resolved while the result was off screen.
    pub fn set_foreign_keys(&mut self, fk: Arc<ForeignKeys>) {
        self.fk_cols = Arc::new(fk.flags());
        self.fk = Some(fk);
    }
}

/// The grid's place in the back / forward history (`ResultGrid::spot`).
#[derive(Clone, Debug)]
pub struct GridSpot {
    rs: Option<std::sync::Weak<ResultSet>>,
    sel: Option<Selection>,
    scroll: (f32, f64),
}

/// Matching cells kept (and tinted); the count says `+` past it.
const FIND_CAP: usize = 100_000;
/// Searches up to this many cells run inline; bigger ones on the background executor.
const FIND_INLINE_CELLS: usize = 8_000;
const FIND_POP_DELAY: Duration = Duration::from_millis(150);

/// The grid's find bar and what it found. The cells come from `diskman_core::find::search_rows`
/// over the rows loaded so far: new rows are searched as they arrive (appended, when the order
/// has not changed), a new query or order searches again. Paint only looks up the visible rows.
struct GridFind {
    bar: Entity<FindBar>,
    open: bool,
    matcher: Option<Arc<Matcher>>,
    invalid: bool,
    /// (display row, column), row by row.
    hits: Vec<(u32, u32)>,
    capped: bool,
    cur: Option<usize>,
    /// What `hits` covers: the result, its display rows searched so far, its order then.
    covers: Option<(Arc<ResultSet>, usize, Option<(usize, SortDir)>)>,
    /// The search in flight and its stop flag (set when the query changes).
    task: Option<Task<()>>,
    cancel: Arc<AtomicBool>,
    /// The next fresh result lands on its first match from `origin` (a new query).
    land: bool,
    origin: (u32, u32),
    pop_task: Option<Task<()>>,
    _sub: Subscription,
}

impl GridFind {
    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.task = None;
    }
}

/// The open cell menu ("Go to …", and the copy items on a right click).
struct CellMenu {
    cell: CellPos,
    entries: Vec<MenuEntry>,
    /// Top-left corner, window coordinates.
    origin: (f32, f32),
    /// The highlighted entry (keyboard and hover share it).
    hi: Option<usize>,
    /// The cell's rectangle in window coordinates: a click there is the grid's (it closes the
    /// menu instead of reopening it).
    cell_rect: (f32, f32, f32, f32),
}

type SummaryKey = (usize, usize, usize, usize, Option<(usize, SortDir)>);

impl Focusable for ResultGrid {
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

impl ResultGrid {
    pub fn new(cx: &mut Context<Self>) -> Self {
        ResultGrid {
            focus: cx.focus_handle(),
            rs: None,
            rt: None,
            widths: Vec::new(),
            scroll: (0., 0.),
            geo: Geometry::default(),
            sel: None,
            drag: None,
            auto: None,
            char_w: 7.8,
            widths_at: 0.,
            sorting: false,
            cancelling: false,
            summary: None,
            decor: None,
            overlay: None,
            seen_rows: 0,
            _watch: None,
            fk: None,
            fk_cols: Arc::new(Vec::new()),
            press: None,
            menu: None,
            menu_focus: cx.focus_handle(),
            _menu_blur: None,
            pop: None,
            land: None,
            copied_at: None,
            _copied: None,
            find: None,
            layout: ColLayout::default(),
            kept: None,
            table: None,
            preferred: None,
        }
    }

    /// A table's data view (or not): header clicks sort on the server, rows count from the
    /// page's offset.
    pub fn set_table_mode(&mut self, m: Option<TableMode>, cx: &mut Context<Self>) {
        if self.table != m {
            self.table = m;
            cx.notify();
        }
    }

    pub fn table_mode(&self) -> Option<&TableMode> {
        self.table.as_ref()
    }

    /// Result columns in display order (hidden ones left out).
    pub fn display_order(&self) -> Vec<usize> {
        (0..self.dcols()).map(|d| self.rc(d)).collect()
    }

    /// Hides result column `c`, or shows every column again (header menu).
    pub fn hide_column(&mut self, c: usize, cx: &mut Context<Self>) {
        self.change_layout(|l| {
            l.hide(c);
        }, cx);
    }

    pub fn show_all_columns(&mut self, cx: &mut Context<Self>) {
        self.change_layout(ColLayout::show_all, cx);
    }

    /// Changes the layout, keeping the selection on the same result columns (or dropping it
    /// when its column went away).
    fn change_layout(&mut self, f: impl FnOnce(&mut ColLayout), cx: &mut Context<Self>) {
        let n = self.ncols();
        self.layout.ensure(n);
        let sel = self.sel.map(|s| (s.anchor.row, self.rc(s.anchor.col), s.head.row, self.rc(s.head.col), s.rows));
        f(&mut self.layout);
        self.sel = sel.and_then(|(ar, ac, hr, hc, rows)| {
            let (a, h) = if rows { (Some(0), Some(self.dcols().saturating_sub(1))) } else { (self.dc(ac), self.dc(hc)) };
            Some(Selection { anchor: CellPos { row: ar, col: a? }, head: CellPos { row: hr, col: h? }, rows })
        });
        self.menu = None;
        self.clamp_scroll();
        // An open find bar searches again: a hidden column's matches go, a shown one's come
        // back, and the order follows the columns on screen.
        if let Some(fd) = self.find.as_mut().filter(|f| f.open) {
            fd.stop();
            fd.covers = None;
            fd.hits.clear();
            fd.capped = false;
            fd.cur = None;
            fd.land = false;
            self.find_sync(cx);
            self.push_find_count(cx);
        }
        cx.notify();
    }

    fn layout_ok(&self) -> bool {
        self.layout.pos.len() == self.ncols()
    }

    /// Display columns.
    fn dcols(&self) -> usize {
        if self.layout_ok() { self.layout.shown() } else { self.ncols() }
    }

    /// Display → result column.
    fn rc(&self, d: usize) -> usize {
        if self.layout_ok() { self.layout.result_col(d).unwrap_or(d) } else { d }
    }

    /// Result → display column (`None` when hidden).
    fn dc(&self, c: usize) -> Option<usize> {
        if self.layout_ok() { self.layout.display_col(c) } else { (c < self.ncols()).then_some(c) }
    }

    /// Width of display column `d`.
    fn dwidth(&self, d: usize) -> f32 {
        self.widths.get(self.rc(d)).copied().unwrap_or(MIN_W)
    }

    /// Left edge of display column `d` in the content (no scroll).
    fn content_x(&self, d: usize) -> f32 {
        (0..d.min(self.dcols())).map(|i| self.dwidth(i)).sum()
    }

    /// Left edge of display column `d` relative to the cells area, as drawn now (the horizontal
    /// scroll moves every column).
    fn col_left(&self, d: usize) -> f32 {
        self.content_x(d) - self.scroll.0
    }

    /// Demo and checks: a cell's rectangle in window coordinates, by display row and result
    /// column (`None` when the column is hidden).
    pub fn cell_bounds(&self, row: usize, col: usize) -> Option<(f32, f32, f32, f32)> {
        let d = self.dc(col)?;
        Some(self.cell_rect(CellPos { row, col: d }))
    }

    /// The active row (the head of the selection), if any.
    pub fn active_row(&self) -> Option<usize> {
        self.sel.map(|s| s.head.row)
    }

    /// The result about to arrive is the row an FK "Go to" asked for: pop on its `key` column
    /// in the first row when it shows up.
    pub fn land_on(&mut self, key: Option<String>) {
        self.land = key;
    }

    /// Demo and live checks: the cell a pop is running on now, and its style.
    pub fn demo_pop(&self) -> Option<((usize, usize), PopStyle)> {
        self.live_pop().map(|(c, style, _)| ((c.row, c.col), style))
    }

    /// The references of `rs` (resolved by the tab when it arrived). Ignored when the grid has
    /// moved on to another result since.
    pub fn set_foreign_keys(&mut self, rs: &Arc<ResultSet>, fk: Arc<ForeignKeys>, cx: &mut Context<Self>) {
        if !self.rs.as_ref().is_some_and(|r| Arc::ptr_eq(r, rs)) {
            return;
        }
        self.fk_cols = Arc::new(fk.flags());
        self.fk = Some(fk);
        cx.notify();
    }

    /// Whether result column `col` holds references (demo, live checks).
    pub fn is_fk_column(&self, col: usize) -> bool {
        self.fk_cols.get(col).copied().unwrap_or(false)
    }

    /// Demo and live checks: the window position of a cell's center (last frame's layout).
    pub fn demo_cell_point(&self, row: usize, col: usize) -> Point<Pixels> {
        let r = self.cell_rect(CellPos { row, col: self.dc(col).unwrap_or(col) });
        point(px(r.0 + r.2.min(120.) / 2.), px(r.1 + r.3 / 2.))
    }

    /// Demo and checks: the window position of a header cell's centre (result column).
    pub fn demo_header_point(&self, col: usize) -> Point<Pixels> {
        let d = self.dc(col).unwrap_or(col);
        let g = &self.geo;
        let x = g.bounds.origin.x.pt() + g.gutter + self.col_left(d) + self.dwidth(d).min(120.) / 2.;
        point(px(x), px(g.bounds.origin.y.pt() + HEADER_H / 2.))
    }

    /// Demo and live checks: the open cell menu as (cell, item labels, highlighted entry,
    /// top-left corner); separators are `—`.
    pub fn demo_menu(&self) -> Option<((usize, usize), Vec<String>, Option<usize>, (f32, f32))> {
        let m = self.menu.as_ref()?;
        let labels = m
            .entries
            .iter()
            .map(|e| match e {
                MenuEntry::Sep => "—".to_string(),
                MenuEntry::Item(it) => it.look().1.to_string(),
            })
            .collect();
        Some(((m.cell.row, self.rc(m.cell.col)), labels, m.hi, m.origin))
    }

    /// Demo and live checks: (anchor, head) of the selection, by result column.
    pub fn demo_selection(&self) -> Option<((usize, usize), (usize, usize))> {
        self.sel.map(|s| ((s.anchor.row, self.rc(s.anchor.col)), (s.head.row, self.rc(s.head.col))))
    }

    /// Opens the header menu of result column `col` (demo).
    pub fn demo_header_menu(&mut self, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = self.dc(col) {
            let p = self.demo_header_point(col);
            self.header_menu(d, p, window, cx);
        }
    }

    /// Demo and live checks: a view is laid over a cell (the inline editor).
    pub fn demo_editing(&self) -> bool {
        self.overlay.is_some()
    }

    pub fn menu_focus_handle(&self) -> &FocusHandle {
        &self.menu_focus
    }

    /// Shows a new result (or none). Redraws whenever the result changes.
    pub fn set_result(&mut self, rs: Option<Arc<ResultSet>>, rt: tokio::runtime::Handle, cx: &mut Context<Self>) {
        // What the user made of the columns carries over to a result with the same ones.
        if let Some(cols) = self.rs.as_ref().and_then(|r| r.columns())
            && self.widths.len() == cols.len()
        {
            let names = cols.iter().map(|c| c.name.clone()).collect();
            self.kept = Some((names, std::mem::take(&mut self.layout), std::mem::take(&mut self.widths), self.scroll.0, self.widths_at));
        }
        self.rs = rs.clone();
        self.rt = Some(rt);
        self.widths.clear();
        self.layout = ColLayout::default();
        self.scroll = (0., 0.);
        self.sel = None;
        self.drag = None;
        self.auto = None;
        self.sorting = false;
        self.cancelling = false;
        self.summary = None;
        self.decor = None;
        self.overlay = None;
        self.seen_rows = 0;
        self.fk = None;
        self.fk_cols = Arc::new(Vec::new());
        self.press = None;
        self.menu = None;
        self.pop = None;
        // An open find bar searches the new result (on its first frame).
        if let Some(f) = &mut self.find {
            f.stop();
            f.covers = None;
            f.hits = Vec::new();
            f.capped = false;
            f.cur = None;
        }
        self._watch = rs.map(|rs| {
            let mut rx = rs.subscribe();
            drop(rs);
            cx.spawn(async move |this, cx| {
                while rx.changed().await.is_ok() {
                    let alive = this.update(cx, |g, cx| {
                        // New rows or a new order: an open find bar searches them, a sort
                        // takes them in once they stop coming.
                        g.find_sync(cx);
                        g.resort_settled(cx);
                        cx.notify()
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            })
        });
        self.find_sync(cx);
        cx.notify();
    }

    /// A fresh read of the same list (the activity monitor's refresh): with the same columns, the
    /// widths, layout and scroll carry over and the selection moves to display row `row` (or
    /// goes); the caller sorts `rs` as the old one was (`ResultSet::sort_state`). Otherwise it
    /// is `set_result`.
    pub fn replace_result(&mut self, rs: Arc<ResultSet>, row: Option<usize>, rt: tokio::runtime::Handle, cx: &mut Context<Self>) {
        let same = match (self.rs.as_ref().and_then(|r| r.columns()), rs.columns()) {
            (Some(a), Some(b)) => a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.name == y.name),
            _ => false,
        };
        let col = self.sel.map_or(0, |s| s.head.col);
        let keep = (same && self.widths.len() == rs.columns().map_or(0, |c| c.len()))
            .then(|| (std::mem::take(&mut self.widths), std::mem::take(&mut self.layout), self.scroll));
        let rows = rs.rows();
        self.set_result(Some(rs), rt, cx);
        if let Some((widths, layout, scroll)) = keep {
            self.kept = None;
            self.widths = widths;
            self.layout = layout;
            self.scroll = scroll;
            self.seen_rows = rows;
        }
        if let Some(r) = row.filter(|r| *r < rows) {
            let c = CellPos { row: r, col };
            self.sel = Some(Selection { anchor: c, head: c, rows: false });
        }
        self.clamp_scroll();
    }

    /// Column widths for the next result whose widths are not known yet (one per column; a
    /// list of another length is ignored).
    pub fn prefer_widths(&mut self, widths: Vec<f32>) {
        self.preferred = Some(widths);
    }

    /// The per-result part of the grid's state (the result tabs of a script keep one each): the
    /// result, column widths, scroll, selection and its references. The sort lives in the result.
    pub fn take_view(&mut self) -> GridView {
        // A fresh result with the same columns (the next page of a table) starts from these.
        if let Some(cols) = self.rs.as_ref().and_then(|r| r.columns())
            && self.widths.len() == cols.len()
        {
            let names = cols.iter().map(|c| c.name.clone()).collect();
            self.kept = Some((names, self.layout.clone(), self.widths.clone(), self.scroll.0, self.widths_at));
        }
        GridView {
            rs: self.rs.clone(),
            widths: std::mem::take(&mut self.widths),
            widths_at: self.widths_at,
            scroll: self.scroll,
            sel: self.sel,
            fk: self.fk.clone(),
            fk_cols: self.fk_cols.clone(),
            layout: std::mem::take(&mut self.layout),
        }
    }

    /// Shows a result with the state it had when it was last on screen (`take_view`).
    pub fn show_view(&mut self, v: GridView, rt: Option<tokio::runtime::Handle>, cx: &mut Context<Self>) {
        match rt.or_else(|| self.rt.clone()) {
            Some(rt) => self.set_result(v.rs, rt, cx),
            None => return,
        }
        self.widths = v.widths;
        self.widths_at = v.widths_at;
        self.layout = v.layout;
        self.scroll = v.scroll;
        self.sel = v.sel;
        self.fk = v.fk;
        self.fk_cols = v.fk_cols;
        self.seen_rows = self.rs.as_ref().map_or(0, |r| r.rows());
        cx.notify();
    }

    /// Installs (or clears) the owner's decorations. Cheap: the grid repaints only the
    /// visible rows, looking each one up by storage row.
    pub fn set_decorations(&mut self, d: Option<Arc<Decorations>>, cx: &mut Context<Self>) {
        self.follow_now();
        self.decor = d;
        let n = self.rows().max(1) - 1;
        if let Some(s) = &mut self.sel {
            // A selection on virtual rows that went away.
            s.anchor.row = s.anchor.row.min(n);
            s.head.row = s.head.row.min(n);
        }
        cx.notify();
    }

    /// Places a view over one cell (an inline editor), or removes it.
    pub fn set_overlay(&mut self, o: Option<CellOverlay>, cx: &mut Context<Self>) {
        self.overlay = o;
        cx.notify();
    }

    /// Rows arrived while rows are being inserted (virtual rows after the result): a selection
    /// on those rows moves down with them instead of landing on a real row.
    fn follow_rows(&mut self, data_rows: usize) {
        if data_rows == self.seen_rows {
            return;
        }
        if self.virtual_rows() > 0
            && let Some(s) = &mut self.sel
        {
            s.anchor.row = follow_growth(s.anchor.row, self.seen_rows, data_rows);
            s.head.row = follow_growth(s.head.row, self.seen_rows, data_rows);
        }
        self.seen_rows = data_rows;
    }

    fn follow_now(&mut self) {
        let n = self.rs.as_ref().map_or(0, |r| r.rows());
        self.follow_rows(n);
    }

    /// Where the grid stands (result, selection, scroll), for back / forward (`nav`).
    pub fn spot(&self) -> GridSpot {
        GridSpot { rs: self.rs.as_ref().map(Arc::downgrade), sel: self.sel, scroll: self.scroll }
    }

    /// Back to a `spot` taken on the result still on screen; another result leaves the grid be.
    pub fn set_spot(&mut self, s: &GridSpot, cx: &mut Context<Self>) {
        let same = match (&s.rs, &self.rs) {
            (Some(w), Some(rs)) => std::ptr::eq(w.as_ptr(), Arc::as_ptr(rs)),
            _ => false,
        };
        if !same {
            return;
        }
        let (rows, cols) = (self.rows(), self.dcols());
        self.sel = s.sel.filter(|_| rows > 0 && cols > 0).map(|mut sel| {
            for c in [&mut sel.anchor, &mut sel.head] {
                c.row = c.row.min(rows - 1);
                c.col = c.col.min(cols - 1);
            }
            sel
        });
        self.scroll = s.scroll;
        cx.notify();
    }

    /// Moves the active cell (and reveals it). `row` is a display row of the result as it is now,
    /// `col` a result column (a hidden one keeps the column of the selection).
    pub fn select_cell(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        self.follow_now();
        let (rows, ncols) = (self.rows(), self.dcols());
        if rows == 0 || ncols == 0 {
            return;
        }
        let d = self.dc(col.min(self.ncols().saturating_sub(1))).or(self.sel.map(|s| s.head.col)).unwrap_or(0);
        let c = CellPos { row: row.min(rows - 1), col: d.min(ncols - 1) };
        self.sel = Some(Selection { anchor: c, head: c, rows: false });
        self.reveal(c);
        cx.notify();
    }

    /// The selection as (display rows, result columns in display order), for exporting it.
    pub fn selection(&self) -> Option<(Vec<usize>, Vec<usize>)> {
        let s = self.sel?;
        Some((s.rows().collect(), s.cols(self.dcols()).map(|d| self.rc(d)).collect()))
    }

    /// The selected display rows (whole rows or the rows of a cell range), against the result
    /// as it is now.
    pub fn selected_rows(&mut self) -> Vec<usize> {
        self.follow_now();
        self.sel.map(|s| s.rows().collect()).unwrap_or_default()
    }

    /// Scrolls sideways to an absolute pixel offset (demo).
    pub fn set_scroll_x(&mut self, x: f32, cx: &mut Context<Self>) {
        self.scroll.0 = x;
        self.clamp_scroll();
        cx.notify();
    }

    /// Scrolls to an absolute pixel offset (benchmarks).
    pub fn set_scroll_y(&mut self, y: f32, cx: &mut Context<Self>) {
        self.scroll.1 = y as f64;
        self.clamp_scroll();
        cx.notify();
    }

    pub fn max_scroll_y(&self) -> f32 {
        self.max_y() as f32
    }

    fn max_y(&self) -> f64 {
        (self.geo.content_h - self.geo.body_h as f64).max(0.)
    }

    /// Display rows: the result's, then the virtual rows of the decorations.
    fn rows(&self) -> usize {
        self.rs.as_ref().map(|r| r.rows()).unwrap_or(0) + self.virtual_rows()
    }

    fn virtual_rows(&self) -> usize {
        self.decor.as_ref().map_or(0, |d| d.virtual_rows.len())
    }

    fn ncols(&self) -> usize {
        self.rs.as_ref().and_then(|r| r.columns()).map(|c| c.len()).unwrap_or(0)
    }

    fn clamp_scroll(&mut self) {
        let max_x = (self.geo.content_w - self.geo.body_w).max(0.);
        let max_y = self.max_y();
        self.scroll.0 = self.scroll.0.clamp(0., max_x);
        self.scroll.1 = self.scroll.1.clamp(0., max_y);
    }

    /// Initial widths by type (Spec: int 84 · numeric 96 · text 200 · timestamptz 206 · jsonb
    /// 290 · bool 72), widened to fit the header. Never scans the result.
    fn ensure_widths(&mut self, snap: &Snapshot) {
        let n = snap.columns.len();
        if self.widths.len() == n {
            self.layout.ensure(n);
            return;
        }
        // The same columns as the result before (next page, re-run): its layout and widths.
        if let Some((names, layout, widths, sx, at)) = self.kept.take()
            && names.len() == n
            && names.iter().zip(snap.columns.iter()).all(|(a, b)| *a == b.name)
        {
            self.layout = layout;
            self.widths = widths;
            self.widths_at = at;
            self.scroll.0 = sx;
            return;
        }
        self.layout.ensure(n);
        self.widths_at = self.char_w;
        if let Some(w) = self.preferred.as_ref().filter(|w| w.len() == n) {
            self.widths = w.iter().map(|w| w.max(MIN_W)).collect();
            return;
        }
        let (cw, cw_type) = (self.char_w, self.char_w * 10.5 / 12.);
        // The Spec's widths are for a 0.6 em face (7.2 pt at 12 pt): a narrower one scales them.
        let per_type = cw / 7.2;
        self.widths = (0..n)
            .map(|c| {
                let col = &snap.columns[c];
                let by_type = (type_width(col.kind) * per_type).max(MIN_W);
                let head = col.name.chars().count() as f32 * cw + col.type_name.chars().count() as f32 * cw_type + 6. + 2. * PAD + 16.;
                by_type.max(head).min(MAX_AUTO_W)
            })
            .collect();
    }

    /// The UI font changed since the widths were made: scale them (and the horizontal scroll)
    /// by the new advance, so every column shows as many characters as before.
    fn follow_font(&mut self) {
        let (now, at) = (self.char_w, self.widths_at);
        if self.widths.is_empty() || at <= 0. || (now - at).abs() < 0.01 {
            if at <= 0. {
                self.widths_at = now;
            }
            return;
        }
        let k = now / at;
        for w in &mut self.widths {
            *w = (*w * k).round().max(MIN_W);
        }
        self.scroll.0 *= k;
        self.widths_at = now;
    }

    /// Demo and checks: the column widths as laid out, and the advance they were made for.
    pub fn demo_widths(&self) -> (Vec<f32>, f32) {
        (self.widths.clone(), self.char_w)
    }

    /// Display column under `x` (relative to the cells area, scrolled); past the last column,
    /// the last one.
    fn hit_col(&self, x: f32) -> Option<usize> {
        let n = self.dcols();
        if n == 0 || self.widths.is_empty() {
            return None;
        }
        let x = x + self.scroll.0;
        let mut acc = 0.;
        for d in 0..n {
            acc += self.dwidth(d);
            if x < acc {
                return Some(d);
            }
        }
        Some(n - 1)
    }

    /// Cell under a window position (cells area only); the column is a display column.
    fn hit_cell(&self, p: Point<Pixels>) -> Option<CellPos> {
        let g = &self.geo;
        let x = p.x.pt() - g.bounds.origin.x.pt() - g.gutter;
        let y = (p.y.pt() - g.bounds.origin.y.pt() - HEADER_H) as f64 + self.scroll.1;
        let rows = self.rows();
        if rows == 0 {
            return None;
        }
        let row = ((y / ROW_H as f64).floor().max(0.) as usize).min(rows - 1);
        Some(CellPos { row, col: self.hit_col(x)? })
    }

    fn local(&self, p: Point<Pixels>) -> (f32, f32) {
        (p.x.pt() - self.geo.bounds.origin.x.pt(), p.y.pt() - self.geo.bounds.origin.y.pt())
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.is_some() {
            self.close_menu(true, window, cx);
        }
        let d = ev.delta.pixel_delta(px(ROW_H));
        let (mut dx, dy) = (d.x.pt(), d.y.pt());
        let mut dy = dy;
        if ev.modifiers.shift && dx == 0. {
            dx = dy;
            dy = 0.;
        }
        self.scroll.0 -= dx;
        self.scroll.1 -= dy as f64;
        self.clamp_scroll();
        cx.notify();
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.follow_now();
        // Any press closes the cell menu.
        self.menu = None;
        self.press = None;
        let g = self.geo;
        let (x, y) = self.local(ev.position);
        let w = g.bounds.size.width.pt();
        let h = g.bounds.size.height.pt();
        // Scrollbars.
        if x >= w - BAR && y >= HEADER_H && g.content_h > g.body_h as f64 {
            let (top, len) = self.v_thumb();
            let ty = y - HEADER_H;
            if ty >= top && ty <= top + len {
                self.drag = Some(Drag::VThumb { grab: ty - top });
            } else {
                // Page towards the click.
                let page = g.body_h - ROW_H;
                self.scroll.1 += if ty < top { -page as f64 } else { page as f64 };
                self.clamp_scroll();
            }
            cx.notify();
            return;
        }
        if y >= h - BAR && x >= g.gutter && g.content_w > g.body_w {
            let (left, len) = self.h_thumb();
            let tx = x - g.gutter;
            if tx >= left && tx <= left + len {
                self.drag = Some(Drag::HThumb { grab: tx - left });
            } else {
                self.scroll.0 += if tx < left { -g.body_w } else { g.body_w };
                self.clamp_scroll();
            }
            cx.notify();
            return;
        }
        if y < HEADER_H {
            if x < g.gutter {
                return;
            }
            let bx = x - g.gutter;
            // ⌃-click is the Mac's right click.
            if ev.modifiers.control && !ev.modifiers.platform {
                if let Some(d) = self.hit_col(bx) {
                    self.header_menu(d, ev.position, window, cx);
                }
                return;
            }
            // Resize handle: near a column's right border, as drawn.
            for d in 0..self.dcols() {
                let right = self.col_left(d) + self.dwidth(d);
                if (bx - right).abs() <= RESIZE_GRAB {
                    self.drag = Some(Drag::Resize { col: self.rc(d), start_x: x, start_w: self.dwidth(d) });
                    return;
                }
            }
            if let Some(d) = self.hit_col(bx) {
                self.drag = Some(Drag::Header { col: d, x });
            }
            return;
        }
        let Some(cell) = self.hit_cell(ev.position) else { return };
        // ⌃-click is the Mac's right click.
        if ev.modifiers.control && !ev.modifiers.platform && x >= g.gutter {
            self.context_menu(cell, ev.position, window, cx);
            return;
        }
        if x < g.gutter {
            let anchor = match (ev.modifiers.shift, self.sel) {
                (true, Some(s)) => s.anchor,
                _ => CellPos { row: cell.row, col: 0 },
            };
            self.sel = Some(Selection { anchor, head: CellPos { row: cell.row, col: self.dcols().saturating_sub(1) }, rows: true });
            self.drag = Some(Drag::Rows);
        } else {
            let anchor = match (ev.modifiers.shift, self.sel) {
                (true, Some(s)) => s.anchor,
                _ => cell,
            };
            // A plain click that selects a value pops it (Xcode's find indicator, in the
            // selection blue). Not a click on the value already selected, not when extending
            // (⇧, ⌘ = straight to the reference), not the second click of a double click (it
            // opens the editor over the cell): those keep or replace the selection quietly.
            let fresh = ev.click_count == 1
                && !ev.modifiers.shift
                && !ev.modifiers.platform
                && !self.sel.is_some_and(|s| s.single() && s.head == cell);
            if fresh {
                self.pop = FindPop::start(cell, PopStyle::Select, cx);
            } else if self.pop.as_ref().is_some_and(|p| p.style == PopStyle::Select) {
                self.pop = None;
            }
            self.sel = Some(Selection { anchor, head: cell, rows: false });
            self.drag = Some(Drag::Cells);
            self.press = grid_menu::press(ev.click_count, ev.modifiers.shift, ev.modifiers.platform, false, cell.row, cell.col);
            if ev.click_count == 2 && !ev.modifiers.shift {
                self.drag = None;
                cx.emit(GridEvent::EditCell { row: cell.row, col: self.rc(cell.col) });
            }
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.drag else { return };
        if ev.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            self.press = None;
            return;
        }
        let g = self.geo;
        let (x, y) = self.local(ev.position);
        match drag {
            // The window-wide listener of `paint_frame` follows these (also past the grid).
            Drag::Cells | Drag::Rows => return,
            Drag::Resize { col, start_x, start_w } => {
                if let Some(w) = self.widths.get_mut(col) {
                    *w = (start_w + x - start_x).max(MIN_W);
                }
            }
            Drag::VThumb { grab } => {
                let (_, len) = self.v_thumb();
                let track = g.body_h - len;
                if track > 0. {
                    self.scroll.1 = ((y - HEADER_H - grab) / track) as f64 * self.max_y();
                }
            }
            Drag::HThumb { grab } => {
                let (_, len) = self.h_thumb();
                let track = g.body_w - len;
                if track > 0. {
                    self.scroll.0 = (x - g.gutter - grab) / track * (g.content_w - g.body_w);
                }
            }
            Drag::Header { x: x0, .. } => {
                // Moved: no sort (columns do not move).
                if (x - x0).abs() > 3. {
                    self.drag = Some(Drag::Header { col: usize::MAX, x: x0 });
                }
            }
        }
        self.clamp_scroll();
        cx.notify();
    }

    /// A dragged cell or row selection follows the pointer at window position `p` (inside the
    /// grid or not): the head moves to the cell under it (clamped to the cells on screen), and
    /// near or past an edge the grid scrolls on its own until the pointer comes back or the
    /// button is released.
    fn drag_to(&mut self, p: Point<Pixels>, cx: &mut Context<Self>) {
        if !matches!(self.drag, Some(Drag::Cells | Drag::Rows)) {
            self.auto = None;
            return;
        }
        let moving = self.auto_velocity(p) != (0., 0.);
        match &mut self.auto {
            Some(a) => a.at = p,
            None => self.auto = Some(AutoScroll { at: p, last: Instant::now(), _tick: None }),
        }
        if moving {
            if self.auto.as_ref().is_some_and(|a| a._tick.is_none()) {
                let task = cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(AUTO_TICK).await;
                        match this.update(cx, |g, cx| g.auto_tick(cx)) {
                            Ok(true) => {}
                            _ => break,
                        }
                    }
                });
                if let Some(a) = &mut self.auto {
                    a.last = Instant::now();
                    a._tick = Some(task);
                }
            }
        } else if let Some(a) = &mut self.auto {
            a._tick = None;
        }
        self.extend_drag(p);
        cx.notify();
    }

    /// The autoscroll speed (pt/s, x and y) for the pointer at window position `p`: zero on an
    /// axis that cannot scroll that way any more; a row drag scrolls only vertically.
    fn auto_velocity(&self, p: Point<Pixels>) -> (f32, f32) {
        let g = self.geo;
        let (x, y) = self.local(p);
        let rows = matches!(self.drag, Some(Drag::Rows));
        let max_x = (g.content_w - g.body_w).max(0.);
        let mut vx = if rows { 0. } else { axis_speed(x, g.gutter, g.gutter + g.body_w) };
        let mut vy = axis_speed(y, HEADER_H, HEADER_H + g.body_h);
        if (vx < 0. && self.scroll.0 <= 0.) || (vx > 0. && self.scroll.0 >= max_x) {
            vx = 0.;
        }
        if (vy < 0. && self.scroll.1 <= 0.) || (vy > 0. && self.scroll.1 >= self.max_y()) {
            vy = 0.;
        }
        (vx, vy)
    }

    /// One autoscroll step: scrolls by the speed × the time since the last one and moves the
    /// selection's head; false once there is nothing left to do (the timer stops).
    fn auto_tick(&mut self, cx: &mut Context<Self>) -> bool {
        if !matches!(self.drag, Some(Drag::Cells | Drag::Rows)) {
            self.auto = None;
            return false;
        }
        let Some((at, last)) = self.auto.as_ref().map(|a| (a.at, a.last)) else { return false };
        let (vx, vy) = self.auto_velocity(at);
        if (vx, vy) == (0., 0.) {
            if let Some(a) = &mut self.auto {
                a._tick = None;
            }
            return false;
        }
        let now = Instant::now();
        // A stalled frame does not jump: at most 50 ms of travel per step.
        let dt = (now - last).as_secs_f32().min(0.05);
        self.scroll.0 += vx * dt;
        self.scroll.1 += (vy * dt) as f64;
        self.clamp_scroll();
        if let Some(a) = &mut self.auto {
            a.last = now;
        }
        self.extend_drag(at);
        cx.notify();
        true
    }

    /// Moves the dragged selection's head to the cell under `p`, clamped to the cells on screen.
    fn extend_drag(&mut self, p: Point<Pixels>) {
        let g = self.geo;
        let o = g.bounds.origin;
        let right = (g.gutter + g.body_w - 1.).max(g.gutter + 1.);
        let bottom = (HEADER_H + g.body_h - 1.).max(HEADER_H + 1.);
        let p = point(p.x.clamp(o.x + px(g.gutter + 1.), o.x + px(right)), p.y.clamp(o.y + px(HEADER_H + 1.), o.y + px(bottom)));
        if let (Some(cell), Some(sel)) = (self.hit_cell(p), self.sel.as_mut()) {
            sel.head = if sel.rows { CellPos { row: cell.row, col: sel.head.col } } else { cell };
            // A drag selects a range: its release opens nothing.
            if self.press.is_some_and(|pr| (pr.row, pr.col) != (cell.row, cell.col)) {
                self.press = None;
            }
        }
    }

    /// Demo and checks: the cells area (x, y, w, h) in window coordinates, as last laid out.
    pub fn demo_cells_rect(&self) -> (f32, f32, f32, f32) {
        let g = self.geo;
        (g.bounds.origin.x.pt() + g.gutter, g.bounds.origin.y.pt() + HEADER_H, g.body_w, g.body_h)
    }

    /// Demo and checks: a row's height on screen (window points, zoomed) and the measured
    /// advance in the UI's points (the same at any zoom).
    pub fn demo_row_metrics(&self) -> (f32, f32) {
        (px(ROW_H).as_f32(), self.char_w)
    }

    /// Demo and checks: the scroll offsets (x, y) in points.
    pub fn demo_scroll(&self) -> (f32, f64) {
        self.scroll
    }

    /// Demo and checks: whether the drag autoscroll timer runs.
    pub fn demo_autoscrolling(&self) -> bool {
        self.auto.as_ref().is_some_and(|a| a._tick.is_some())
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.auto = None;
        match self.drag {
            Some(Drag::Header { col, .. }) if col != usize::MAX => {
                let c = self.rc(col);
                self.toggle_sort(c, cx);
            }
            _ => {}
        }
        self.drag = None;
        // The release of a ⌘-click on a reference: straight there, no menu.
        if let (Some(p), Some(s)) = (self.press.take(), self.sel) {
            let at = (s.head.row, s.head.col);
            let jumps = if (p.row, p.col) == at && s.single() { self.jumps_at(s.head) } else { Vec::new() };
            if let Release::Go = grid_menu::release(Some(p), at, s.single(), jumps.len()) {
                cx.emit(GridEvent::FollowFk(jumps[0].clone()));
            }
        }
        cx.notify();
    }

    // The cell menu.

    /// Where the references of a cell lead: one jump per target. None for NULL values, rows
    /// being inserted and cells with a pending edit (the stored value is not what is shown).
    fn jumps_at(&self, c: CellPos) -> Vec<FkJump> {
        let (Some(fk), Some(rs)) = (&self.fk, &self.rs) else { return Vec::new() };
        let col = self.rc(c.col);
        let targets = fk.targets(col);
        if targets.is_empty() {
            return Vec::new();
        }
        let snap = rs.snapshot();
        if c.row >= snap.rows() {
            return Vec::new();
        }
        if let Some(d) = &self.decor
            && d.cells.get(&(snap.source_row(c.row), col)).is_some_and(|x| x.text.is_some())
        {
            return Vec::new();
        }
        targets
            .iter()
            .filter_map(|t| {
                let values: Vec<_> = t.from_cols.iter().map(|&i| (snap.columns[i].kind, snap.cell(c.row, i))).collect();
                fk.jump(t, &values)
            })
            .collect()
    }

    /// A cell's rectangle in window coordinates (from the last frame's layout).
    fn cell_rect(&self, c: CellPos) -> (f32, f32, f32, f32) {
        let g = &self.geo;
        let x = g.bounds.origin.x.pt() + g.gutter + self.col_left(c.col);
        let y = g.bounds.origin.y.pt() + HEADER_H + (c.row as f64 * ROW_H as f64 - self.scroll.1) as f32;
        (x, y, self.dwidth(c.col), ROW_H)
    }

    /// Opens the cell menu: under the cell (a click, ⌥↓) or at the pointer (right click). From
    /// the keyboard the first item is highlighted; ↩ takes the first one anyway.
    fn open_menu(&mut self, cell: CellPos, entries: Vec<MenuEntry>, at: Option<Point<Pixels>>, keyboard: bool, window: &mut Window, cx: &mut Context<Self>) {
        if entries.is_empty() {
            return;
        }
        let vp = window.viewport_size();
        let vp = (vp.width.pt(), vp.height.pt());
        let h = grid_menu::menu_h(&entries);
        let r = self.cell_rect(cell);
        let origin = match at {
            Some(p) => crate::tree_menu::place((p.x.pt(), p.y.pt()), p.y.pt(), grid_menu::MENU_W, h, vp),
            None => {
                // A cell scrolled half out of view on the left: the menu starts at the body.
                let left = r.0.max(self.geo.bounds.origin.x.pt() + self.geo.gutter);
                crate::tree_menu::place((left, r.1 + ROW_H + grid_menu::GAP), r.1 - grid_menu::GAP, grid_menu::MENU_W, h, vp)
            }
        };
        let enabled: Vec<bool> = entries.iter().map(MenuEntry::enabled).collect();
        let hi = if keyboard { crate::tree_menu::step(&enabled, None, 1) } else { None };
        self.menu = Some(CellMenu { cell, entries, origin, hi, cell_rect: r });
        window.focus(&self.menu_focus, cx);
        // Opened from a mouse-down (right click, ⌃-click): the grid takes the focus on that
        // press after this handler, so the menu takes it back once the event is done.
        cx.defer_in(window, |g, window, cx| {
            if g.menu.is_some() {
                window.focus(&g.menu_focus, cx);
            }
        });
        if self._menu_blur.is_none() {
            // The focus went elsewhere (a shortcut, another window): the menu goes away.
            self._menu_blur = Some(cx.on_focus_out(&self.menu_focus, window, |g, _, _, cx| {
                if g.menu.take().is_some() {
                    cx.notify();
                }
            }));
        }
        cx.notify();
    }

    /// Right click (or ⌃-click) on a cell: inside the selection it keeps it (macOS), elsewhere
    /// it selects the cell; the menu has the cell's "Go to …" items and the copy items.
    fn context_menu(&mut self, cell: CellPos, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let inside = self.sel.is_some_and(|s| s.rows().contains(&cell.row) && s.cols(self.dcols()).contains(&cell.col));
        if !inside {
            self.sel = Some(Selection { anchor: cell, head: cell, rows: false });
        }
        self.press = None;
        self.drag = None;
        let jumps = self.jumps_at(cell);
        let mut entries = grid_menu::entries(jumps, true);
        // A table's data view: "Filter by value" on a stored value (not a pending edit or a new row).
        if self.table.is_some()
            && let Some(rs) = &self.rs
            && cell.row < rs.rows()
        {
            let col = self.rc(cell.col);
            let snap = rs.snapshot();
            let edited = self.decor.as_ref().is_some_and(|d| d.cells.get(&(snap.source_row(cell.row), col)).is_some_and(|x| x.text.is_some()));
            if !edited {
                let value = diskman_core::sqlgen::cell_text_utc(snap.columns[col].kind, snap.cell(cell.row, col));
                entries = grid_menu::with_filter(entries, col, value);
            }
        }
        self.open_menu(cell, entries, Some(at), false, window, cx);
    }

    /// Right click on a header: sort and filter (data view), hide, show all.
    fn header_menu(&mut self, d: usize, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let c = self.rc(d);
        let entries = grid_menu::header_entries(c, self.table.is_some(), self.layout.hidden_count(), self.dcols() > 1);
        self.press = None;
        self.drag = None;
        // The header cell stands for the menu's cell (a click there only closes it).
        let cell = CellPos { row: usize::MAX, col: d };
        let vp = window.viewport_size();
        let vp = (vp.width.pt(), vp.height.pt());
        let h = grid_menu::menu_h(&entries);
        let origin = crate::tree_menu::place((at.x.pt(), at.y.pt()), at.y.pt(), grid_menu::MENU_W, h, vp);
        let g = &self.geo;
        let r = (g.bounds.origin.x.pt() + g.gutter + self.col_left(d), g.bounds.origin.y.pt(), self.dwidth(d), HEADER_H);
        self.menu = Some(CellMenu { cell, entries, origin, hi: None, cell_rect: r });
        window.focus(&self.menu_focus, cx);
        cx.defer_in(window, |g, window, cx| {
            if g.menu.is_some() {
                window.focus(&g.menu_focus, cx);
            }
        });
        if self._menu_blur.is_none() {
            self._menu_blur = Some(cx.on_focus_out(&self.menu_focus, window, |g, _, _, cx| {
                if g.menu.take().is_some() {
                    cx.notify();
                }
            }));
        }
        cx.notify();
    }

    fn on_right_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.follow_now();
        self.menu = None;
        let g = self.geo;
        let (x, y) = self.local(ev.position);
        let (w, h) = (g.bounds.size.width.pt(), g.bounds.size.height.pt());
        if y < HEADER_H && x >= g.gutter && x < w - BAR {
            if let Some(d) = self.hit_col(x - g.gutter) {
                self.header_menu(d, ev.position, window, cx);
            }
            return;
        }
        // Cells only, and not the empty space under the last row.
        let below = (y - HEADER_H) as f64 + self.scroll.1 >= self.rows() as f64 * ROW_H as f64;
        if y < HEADER_H || x < g.gutter || x >= w - BAR || y >= h - BAR || below {
            return;
        }
        if let Some(cell) = self.hit_cell(ev.position) {
            self.context_menu(cell, ev.position, window, cx);
        }
    }

    /// ⌥↓: the menu of the active cell, when it holds a reference.
    fn open_menu_from_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.follow_now();
        let Some(s) = self.sel.filter(Selection::single) else { return };
        let jumps = self.jumps_at(s.head);
        if jumps.is_empty() {
            return;
        }
        self.reveal(s.head);
        self.open_menu(s.head, grid_menu::entries(jumps, false), None, true, window, cx);
    }

    /// Closes the menu; `refocus` gives the keys back to the grid.
    fn close_menu(&mut self, refocus: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        if refocus {
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }

    fn menu_move(&mut self, dir: isize, cx: &mut Context<Self>) {
        let Some(m) = self.menu.as_mut() else { return };
        let enabled: Vec<bool> = m.entries.iter().map(MenuEntry::enabled).collect();
        m.hi = crate::tree_menu::step(&enabled, m.hi, dir);
        cx.notify();
    }

    /// Runs entry `ix` (or the highlighted one, or the first).
    fn menu_run(&mut self, ix: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.menu.take() else { return };
        window.focus(&self.focus, cx);
        let enabled: Vec<bool> = m.entries.iter().map(MenuEntry::enabled).collect();
        let ix = ix.or(m.hi).or_else(|| crate::tree_menu::step(&enabled, None, 1));
        match ix.and_then(|i| m.entries.get(i)) {
            Some(MenuEntry::Item(MenuItem::Go(j))) => cx.emit(GridEvent::FollowFk(j.clone())),
            Some(MenuEntry::Item(MenuItem::CopyValue)) => {
                if let Some(rs) = &self.rs
                    && m.cell.row < rs.rows()
                {
                    let text = rs.snapshot().text(m.cell.row, self.rc(m.cell.col)).map(|v| v.into_owned()).unwrap_or_default();
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
            Some(MenuEntry::Item(MenuItem::CopyTsv)) => self.copy(CopyFormat::Tsv, false, cx),
            Some(MenuEntry::Item(MenuItem::CopyWithHeaders)) => self.copy(CopyFormat::Tsv, true, cx),
            Some(MenuEntry::Item(MenuItem::FilterValue(col, v))) => cx.emit(GridEvent::FilterValue { col: *col, value: v.clone() }),
            Some(MenuEntry::Item(MenuItem::SortAsc(col))) => self.sort_by_menu(*col, SortDir::Asc, cx),
            Some(MenuEntry::Item(MenuItem::SortDesc(col))) => self.sort_by_menu(*col, SortDir::Desc, cx),
            Some(MenuEntry::Item(MenuItem::Filter(col))) => {
                // Under the header cell.
                let r = m.cell_rect;
                cx.emit(GridEvent::Filter { col: *col, at: (r.0.max(self.geo.bounds.origin.x.pt() + self.geo.gutter), r.1 + r.3 + 2.) });
            }
            Some(MenuEntry::Item(MenuItem::Hide(col))) => self.hide_column(*col, cx),
            Some(MenuEntry::Item(MenuItem::ShowAll(_))) => self.show_all_columns(cx),
            _ => {}
        }
        cx.notify();
    }

    /// Runs a menu entry by its label (demo, checks).
    pub fn demo_menu_run(&mut self, label: &str, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.menu.as_ref().and_then(|m| {
            m.entries.iter().position(|e| matches!(e, MenuEntry::Item(it) if it.look().1.as_ref() == label))
        });
        if ix.is_some() {
            self.menu_run(ix, window, cx);
        }
    }

    fn render_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let m = self.menu.as_ref()?;
        let t = cx.theme();
        let first_go = m.entries.iter().position(|e| matches!(e, MenuEntry::Item(MenuItem::Go(_))));
        let items: Vec<AnyElement> = m
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| match e {
                MenuEntry::Sep => ui::menu_sep(&t).into_any_element(),
                MenuEntry::Item(it) => {
                    let (ic, label, detail): (Icon, SharedString, Option<SharedString>) = it.look();
                    let key: Option<SharedString> = match it {
                        MenuItem::Go(_) => (first_go == Some(i)).then_some("⌘click".into()),
                        MenuItem::CopyTsv => crate::keymap::key("copy_tsv").map(Into::into),
                        MenuItem::CopyWithHeaders => crate::keymap::key("copy_with_headers").map(Into::into),
                        _ => None,
                    };
                    grid_menu::row(&t, ("grid-menu-item", i), ic, label, detail, key, m.hi == Some(i))
                        .on_hover(cx.listener(move |g, h: &bool, _, cx| {
                            if let (true, Some(m)) = (*h, g.menu.as_mut())
                                && m.hi != Some(i)
                            {
                                m.hi = Some(i);
                                cx.notify();
                            }
                        }))
                        .on_click(cx.listener(move |g, _: &ClickEvent, window, cx| g.menu_run(Some(i), window, cx)))
                        .into_any_element()
                }
            })
            .collect();
        let cell = m.cell_rect;
        let pop = ui::popover(&t)
            .id("grid-menu")
            .w(px(grid_menu::MENU_W))
            .text_size(px(12.))
            .font_family(crate::theme::mono())
            .track_focus(&self.menu_focus)
            .key_context(MENU_CONTEXT)
            .on_action(cx.listener(|g, _: &MenuUp, _, cx| g.menu_move(-1, cx)))
            .on_action(cx.listener(|g, _: &MenuDown, _, cx| g.menu_move(1, cx)))
            .on_action(cx.listener(|g, _: &MenuConfirm, window, cx| g.menu_run(None, window, cx)))
            .on_action(cx.listener(|g, _: &MenuClose, window, cx| g.close_menu(true, window, cx)))
            // Typing closes it (keys the grid binds, like ←→ or ⌘C, reach the grid and close it there).
            .on_key_down(cx.listener(|g, _: &KeyDownEvent, window, cx| g.close_menu(true, window, cx)))
            .on_mouse_down_out(cx.listener(move |g, ev: &MouseDownEvent, window, cx| {
                let (x, y) = (ev.position.x.pt(), ev.position.y.pt());
                // On its own cell the grid decides (a plain click there only closes it).
                if x >= cell.0 && x < cell.0 + cell.2 && y >= cell.1 && y < cell.1 + cell.3 {
                    return;
                }
                g.close_menu(true, window, cx);
            }))
            .children(items);
        Some(deferred(anchored().position(point(px(m.origin.0), px(m.origin.1))).child(pop)).priority(2).into_any_element())
    }

    /// Cycles a column through ascending → descending → original order, over the rows already
    /// loaded (`sort_local`); the query does not run again.
    pub fn toggle_sort(&mut self, col: usize, cx: &mut Context<Self>) {
        // A table's data view sorts on the server (right across its pages).
        if let Some(t) = &self.table {
            let dir = match t.sort {
                Some((c, SortDir::Asc)) if c == col => Some(SortDir::Desc),
                Some((c, SortDir::Desc)) if c == col => None,
                _ => Some(SortDir::Asc),
            };
            cx.emit(GridEvent::ServerSort { col, dir });
            return;
        }
        let Some(rs) = self.rs.clone() else { return };
        let next = match rs.sort_state() {
            Some((c, SortDir::Asc)) if c == col => Some((col, SortDir::Desc)),
            Some((c, SortDir::Desc)) if c == col => None,
            _ => Some((col, SortDir::Asc)),
        };
        self.sort_local(next, cx);
    }

    /// The header menu's Sort ascending / descending.
    fn sort_by_menu(&mut self, col: usize, dir: SortDir, cx: &mut Context<Self>) {
        if self.table.is_some() {
            cx.emit(GridEvent::ServerSort { col, dir: Some(dir) });
        } else {
            self.sort_local(Some((col, dir)), cx);
        }
    }

    /// Sorts the result here (the query does not run again), on the core's threads; `None`
    /// brings back the server's order. A result not read to its end reads the rest first (a sort
    /// is of the whole result, as Fetch all): rows still arriving show after the sorted ones until
    /// it ends, when the sort is applied again (`resort_settled`).
    pub fn sort_local(&mut self, by: Option<(usize, SortDir)>, cx: &mut Context<Self>) {
        let (Some(rs), Some(rt)) = (self.rs.clone(), self.rt.clone()) else { return };
        if self.sorting {
            return;
        }
        if by.is_some() && !rs.phase().is_finished() {
            rs.fetch_all();
        }
        self.sorting = true;
        let job = rt.spawn_blocking(move || rs.sort(by));
        cx.spawn(async move |this, cx| {
            let _ = job.await;
            let _ = this.update(cx, |g, cx| {
                g.sorting = false;
                g.resort_settled(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// A sorted result that took more rows and stopped taking them (paused or finished): the
    /// sort covers them too.
    fn resort_settled(&mut self, cx: &mut Context<Self>) {
        let Some(rs) = &self.rs else { return };
        if self.sorting || self.table.is_some() || rs.phase() == Phase::Running {
            return;
        }
        let (Some(by), Some(n)) = (rs.sort_state(), rs.sorted_rows()) else { return };
        if n < rs.rows() {
            self.sort_local(Some(by), cx);
        }
    }

    fn v_thumb(&self) -> (f32, f32) {
        let g = &self.geo;
        if g.content_h <= g.body_h as f64 || g.content_h <= 0. {
            return (0., g.body_h);
        }
        let len = ((g.body_h as f64 * g.body_h as f64 / g.content_h) as f32).max(24.).min(g.body_h);
        let top = (self.scroll.1 / self.max_y()) as f32 * (g.body_h - len);
        (top, len)
    }

    fn h_thumb(&self) -> (f32, f32) {
        let g = &self.geo;
        if g.content_w <= g.body_w || g.content_w <= 0. {
            return (0., g.body_w);
        }
        let len = (g.body_w * g.body_w / g.content_w).max(24.).min(g.body_w);
        let left = self.scroll.0 / (g.content_w - g.body_w) * (g.body_w - len);
        (left, len)
    }

    // Keyboard.

    /// Moves the active cell by rows and display columns.
    pub fn move_by(&mut self, dr: isize, dc: isize, extend: bool, cx: &mut Context<Self>) {
        self.follow_now();
        let (rows, ncols) = (self.rows(), self.dcols());
        if rows == 0 || ncols == 0 {
            return;
        }
        let cur = self.sel.map(|s| s.head).unwrap_or(CellPos { row: 0, col: 0 });
        let head = CellPos {
            row: (cur.row as isize + dr).clamp(0, rows as isize - 1) as usize,
            col: (cur.col as isize + dc).clamp(0, ncols as isize - 1) as usize,
        };
        let anchor = match (extend, self.sel) {
            (true, Some(s)) => s.anchor,
            _ => head,
        };
        self.sel = Some(Selection { anchor, head, rows: false });
        self.reveal(head);
        cx.notify();
    }

    fn reveal(&mut self, c: CellPos) {
        let g = self.geo;
        let top = c.row as f64 * ROW_H as f64;
        if top < self.scroll.1 {
            self.scroll.1 = top;
        } else if top + ROW_H as f64 > self.scroll.1 + g.body_h as f64 {
            self.scroll.1 = top + ROW_H as f64 - g.body_h as f64;
        }
        if c.col < self.dcols() {
            let x = self.content_x(c.col);
            let w = self.dwidth(c.col);
            if x < self.scroll.0 {
                self.scroll.0 = x;
            } else if x + w > self.scroll.0 + g.body_w {
                self.scroll.0 = (x + w - g.body_w).min(x);
            }
        }
        self.clamp_scroll();
    }

    fn page(&self) -> isize {
        ((self.geo.body_h / ROW_H) as isize - 1).max(1)
    }

    pub fn copy(&mut self, fmt: CopyFormat, header: bool, cx: &mut Context<Self>) {
        let (Some(rs), Some(sel)) = (self.rs.as_ref(), self.sel) else { return };
        let cols: Vec<usize> = sel.cols(self.dcols()).map(|d| self.rc(d)).collect();
        let real = rs.rows();
        let text = rs.copy(sel.rows().filter(|r| *r < real), &cols, fmt, header);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.mark_copied(cx);
    }

    /// Something of this result went to the clipboard: "Copied" in the result bar for a
    /// moment, then the Copy button again (redrawn through its fades and at its end only).
    pub fn mark_copied(&mut self, cx: &mut Context<Self>) {
        let start = Instant::now();
        self.copied_at = Some(start);
        let redraws = crate::motion::copied_redraws(cx.reduce_motion());
        self._copied = Some(cx.spawn(async move |g, cx| {
            for at in redraws {
                cx.background_executor().timer(at.saturating_sub(start.elapsed())).await;
                if g.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        }));
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        let (rows, ncols) = (self.rows(), self.dcols());
        if rows == 0 || ncols == 0 {
            return;
        }
        self.sel = Some(Selection {
            anchor: CellPos { row: 0, col: 0 },
            head: CellPos { row: rows - 1, col: ncols - 1 },
            rows: true,
        });
        cx.notify();
    }

    /// Selects one cell (demo screenshots) or a range of rows in one column (a result column).
    pub fn select_cells(&mut self, rows: std::ops::RangeInclusive<usize>, col: usize, cx: &mut Context<Self>) {
        let col = self.dc(col).unwrap_or(col);
        self.sel = Some(Selection {
            anchor: CellPos { row: *rows.start(), col },
            head: CellPos { row: *rows.end(), col },
            rows: false,
        });
        cx.notify();
    }

    /// The cell the viewer shows (the head of a cell selection): display row, result column.
    pub fn active_cell(&self) -> Option<(usize, usize)> {
        let s = self.sel?;
        (!s.rows || s.anchor.row == s.head.row).then_some((s.head.row, self.rc(s.head.col)))
    }

    pub fn result(&self) -> Option<&Arc<ResultSet>> {
        self.rs.as_ref()
    }

    pub fn is_sorting(&self) -> bool {
        self.sorting
    }

    /// `sel 5 cells · Σ 2,184.20 · avg 436.84` for the result bar; cached per selection.
    pub fn selection_summary(&mut self) -> Option<SharedString> {
        let (rs, sel) = (self.rs.as_ref()?, self.sel?);
        let rows = sel.rows();
        let cols: Vec<usize> = sel.cols(self.dcols()).map(|d| self.rc(d)).collect();
        let n = (rows.end() - rows.start() + 1) * cols.len();
        if n < 2 {
            return None;
        }
        let key = (*rows.start(), *rows.end(), cols.first().copied().unwrap_or(0), cols.len(), rs.sort_state());
        if let Some((k, s)) = &self.summary
            && *k == key
        {
            return Some(s.clone());
        }
        let snap = rs.snapshot();
        let mut text = format!("sel {} cells", ui_count(n));
        let numeric = cols.iter().all(|&c| snap.columns.get(c).is_some_and(|c| c.kind.is_numeric()));
        if numeric && n <= SUMMARY_MAX_CELLS && *rows.end() < snap.rows() {
            let (mut sum, mut count) = (0f64, 0usize);
            for r in rows.clone() {
                for &c in &cols {
                    if let Some(v) = snap.text(r, c).and_then(|t| t.parse::<f64>().ok()) {
                        sum += v;
                        count += 1;
                    }
                }
            }
            if count > 0 {
                text.push_str(&format!(" · Σ {} · avg {}", fmt_num(sum), fmt_num(sum / count as f64)));
            }
        }
        let text: SharedString = text.into();
        self.summary = Some((key, text.clone()));
        Some(text)
    }

    /// This frame of the running pop, asking for the next frame while it runs. Dropped when it
    /// is over, when its cell is gone, and for a click's pop when its cell is no longer the one
    /// selected value (a drag made a range, the keys moved on).
    fn pop_frame(&mut self, window: &mut Window) -> Option<(CellPos, PopStyle, PopFrame)> {
        match self.live_pop() {
            Some(f) => {
                window.request_animation_frame();
                Some(f)
            }
            None => {
                self.pop = None;
                None
            }
        }
    }

    fn live_pop(&self) -> Option<(CellPos, PopStyle, PopFrame)> {
        let p = self.pop.as_ref()?;
        let alive = p.target.row < self.rows()
            && p.target.col < self.dcols()
            && (p.style == PopStyle::Find || self.sel.is_some_and(|s| s.single() && s.head == p.target));
        p.frame(std::time::Instant::now()).filter(|_| alive).map(|f| (p.target, p.style, f))
    }

    // Find.

    fn find_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<FindBar> {
        if let Some(f) = &self.find {
            return f.bar.clone();
        }
        let bar = cx.new(|cx| FindBar::new(false, cx));
        let sub = cx.subscribe_in(&bar, window, Self::on_find_event);
        self.find = Some(GridFind {
            bar: bar.clone(),
            open: false,
            matcher: None,
            invalid: false,
            hits: Vec::new(),
            capped: false,
            cur: None,
            covers: None,
            task: None,
            cancel: Arc::new(AtomicBool::new(false)),
            land: false,
            origin: (0, 0),
            pop_task: None,
            _sub: sub,
        });
        bar
    }

    /// ⌘F on the grid: the bar opens on its field; the first match from the active cell becomes
    /// the active cell.
    pub fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ncols() == 0 {
            return;
        }
        let bar = self.find_bar(window, cx);
        bar.update(cx, |b, cx| b.focus_query(window, cx));
        self.menu = None;
        let origin = self.sel.map_or((0, 0), |s| (s.head.row as u32, s.head.col as u32));
        if let Some(f) = &mut self.find {
            f.open = true;
            f.origin = origin;
        }
        self.restart_find(cx);
    }

    pub fn close_find(&mut self, cx: &mut Context<Self>) {
        if let Some(f) = &mut self.find {
            f.open = false;
            f.stop();
            f.covers = None;
            f.hits = Vec::new();
            f.capped = false;
            f.cur = None;
            f.pop_task = None;
        }
        cx.notify();
    }

    /// The find bar's field has the focus (the grid still draws as focused under it).
    pub fn find_has_focus(&self, window: &Window, cx: &App) -> bool {
        self.find.as_ref().is_some_and(|f| f.open && f.bar.read(cx).has_focus(window, cx))
    }

    /// A new query or toggle (or the bar opening): compile it, stop the search in flight and
    /// search again.
    fn restart_find(&mut self, cx: &mut Context<Self>) {
        let Some(f) = &mut self.find else { return };
        let (q, opts) = {
            let b = f.bar.read(cx);
            (b.query(cx), b.options())
        };
        (f.matcher, f.invalid) = match Matcher::new(&q, opts) {
            Ok(m) => (m.map(Arc::new), false),
            Err(_) => (None, true),
        };
        f.stop();
        f.covers = None;
        f.hits = Vec::new();
        f.capped = false;
        f.cur = None;
        f.land = true;
        self.find_sync(cx);
        self.push_find_count(cx);
        cx.notify();
    }

    /// Brings the matches up to date with the result: nothing (nothing changed), the rows that
    /// arrived since (same order: appended), or everything (another result or order). Runs when
    /// the result changes, never per frame; one search at a time (the next one starts when it
    /// lands). Small searches run inline, big ones on the background executor.
    fn find_sync(&mut self, cx: &mut Context<Self>) {
        let Some(rs) = self.rs.clone() else { return };
        let Some(f) = self.find.as_mut().filter(|f| f.open && f.task.is_none()) else { return };
        let Some(m) = f.matcher.clone() else { return };
        let snap = rs.snapshot();
        let (rows, sort) = (snap.rows(), rs.sort_state());
        let from = match &f.covers {
            Some((r, n, s)) if Arc::ptr_eq(r, &rs) && *s == sort => {
                if *n >= rows || f.capped {
                    return;
                }
                *n
            }
            _ => {
                f.hits.clear();
                f.capped = false;
                0
            }
        };
        let cap = FIND_CAP.saturating_sub(f.hits.len());
        let cancel = Arc::new(AtomicBool::new(false));
        f.cancel = cancel.clone();
        if (rows - from) * snap.columns.len().max(1) <= FIND_INLINE_CELLS {
            let mut h = CellHits::default();
            search_rows(&snap, from..rows, &m, cap, &cancel, &mut h);
            self.take_hits(h, (rs, rows, sort), cx);
            return;
        }
        let bg = cx.background_spawn(async move {
            let mut h = CellHits::default();
            search_rows(&snap, from..rows, &m, cap, &cancel, &mut h).then_some(h)
        });
        f.task = Some(cx.spawn(async move |this, cx| {
            let found = bg.await;
            let _ = this.update(cx, |g, cx| {
                if let Some(f) = &mut g.find {
                    f.task = None;
                }
                if let Some(h) = found {
                    g.take_hits(h, (rs, rows, sort), cx);
                    // Rows that arrived meanwhile.
                    g.find_sync(cx);
                }
                g.push_find_count(cx);
                cx.notify();
            });
        }));
        self.push_find_count(cx);
    }

    /// A search landed: its cells join the list. After a new query, the first match from where
    /// the search started becomes the active cell (it pops once typing pauses).
    fn take_hits(&mut self, h: CellHits, covers: (Arc<ResultSet>, usize, Option<(usize, SortDir)>), cx: &mut Context<Self>) {
        let at = self.sel.map(|s| (s.head.row as u32, s.head.col as u32));
        let pos = self.find_pos();
        let Some(f) = &mut self.find else { return };
        // Hidden columns have no match to land on; the rest go in display order (the new rows
        // come after the old ones, so sorting the chunk keeps the list sorted).
        let mut cells: Vec<(u32, u32)> = h.cells.into_iter().filter(|&(_, c)| pos.get(c as usize).is_some_and(|&d| d != u32::MAX)).collect();
        cells.sort_by_key(|&(r, c)| (r, pos[c as usize]));
        f.hits.extend(cells);
        f.capped |= h.capped;
        f.covers = Some(covers);
        let key = |&(r, c): &(u32, u32)| (r, pos.get(c as usize).copied().unwrap_or(u32::MAX));
        if f.land && !f.hits.is_empty() {
            f.land = false;
            if let Some(i) = nearest_index(&f.hits, key, f.origin) {
                self.select_hit(i, false, cx);
            }
        } else {
            f.cur = nearest_index(&f.hits, key, at.unwrap_or((0, 0)));
        }
        self.push_find_count(cx);
        cx.notify();
    }

    /// Result column → its display position, the order of the find's matches (`u32::MAX`:
    /// hidden).
    fn find_pos(&self) -> Vec<u32> {
        (0..self.ncols()).map(|c| self.dc(c).map_or(u32::MAX, |d| d as u32)).collect()
    }

    fn push_find_count(&self, cx: &mut Context<Self>) {
        let Some(f) = &self.find else { return };
        let c = Count {
            cur: f.cur,
            total: f.hits.len(),
            capped: f.capped,
            busy: f.task.is_some(),
            invalid: f.invalid,
            query: f.matcher.is_some() || f.invalid,
        };
        f.bar.update(cx, |b, cx| b.set_count(c, cx));
    }

    /// Match `i` becomes the active cell (revealed) and pops, now or once typing pauses.
    fn select_hit(&mut self, i: usize, pop_now: bool, cx: &mut Context<Self>) {
        let Some(f) = &mut self.find else { return };
        let Some(&(row, col)) = f.hits.get(i) else { return };
        f.cur = Some(i);
        f.pop_task = None;
        let col = col as usize;
        // The pop is drawn on a display column (none on a hidden one).
        let shown = match self.dc(col) {
            Some(d) => d,
            None => usize::MAX,
        };
        let pop_cell = CellPos { row: row as usize, col: shown };
        let cell = CellPos { row: row as usize, col };
        let Some(f) = &mut self.find else { return };
        if !pop_now {
            let cell = pop_cell;
            f.pop_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(FIND_POP_DELAY).await;
                let _ = this.update(cx, |g, cx| {
                    if g.find.as_ref().is_some_and(|f| f.open && f.cur == Some(i)) {
                        g.pop = FindPop::start(cell, PopStyle::Find, cx);
                        cx.notify();
                    }
                });
            }));
        }
        self.select_cell(cell.row, cell.col, cx);
        if pop_now {
            self.pop = FindPop::start(pop_cell, PopStyle::Find, cx);
        }
        self.push_find_count(cx);
    }

    /// ⌘G / ⇧⌘G (↩ / ⇧↩ in the field): the next or previous matching cell from the active one,
    /// around the ends. With the bar closed ⌘G brings it back on the last query.
    fn find_step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.find.as_ref().is_some_and(|f| f.open) {
            if !self.find.as_ref().is_some_and(|f| f.matcher.is_some()) {
                return self.open_find(window, cx);
            }
            if let Some(f) = &mut self.find {
                f.open = true;
                f.land = false;
            }
            self.find_sync(cx);
        }
        let at = self.sel.map(|s| (s.head.row as u32, s.head.col as u32));
        let pos = self.find_pos();
        let Some(f) = &self.find else { return };
        let key = |&(r, c): &(u32, u32)| (r, pos.get(c as usize).copied().unwrap_or(u32::MAX));
        let ix = match (at, forward) {
            (Some(a), true) => next_index(&f.hits, key, a),
            (Some(a), false) => prev_index(&f.hits, key, a),
            (None, true) => (!f.hits.is_empty()).then_some(0),
            (None, false) => f.hits.len().checked_sub(1),
        };
        if let Some(i) = ix {
            self.select_hit(i, true, cx);
        }
    }

    fn on_find_event(&mut self, _: &Entity<FindBar>, ev: &FindBarEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            FindBarEvent::Changed => self.restart_find(cx),
            FindBarEvent::Next => self.find_step(true, window, cx),
            FindBarEvent::Prev => self.find_step(false, window, cx),
            FindBarEvent::Close => {
                self.close_find(cx);
                window.focus(&self.focus, cx);
            }
            // The grid has no replace (editing values is the grid edit's job).
            FindBarEvent::Replace | FindBarEvent::ReplaceAll => {}
        }
    }

    /// Demo and checks: whether the bar is open, its count (`3/17`), the matching cells found so
    /// far, the rows they cover and whether a search is running.
    pub fn demo_find(&self, cx: &App) -> Option<(bool, String, Vec<(u32, u32)>, usize, bool)> {
        let f = self.find.as_ref()?;
        Some((f.open, f.bar.read(cx).count().label(), f.hits.clone(), f.covers.as_ref().map_or(0, |c| c.1), f.task.is_some()))
    }

    /// Layout and text for one frame.
    fn prepare(&mut self, bounds: Bounds<Pixels>, window: &mut Window, t: &Theme, cx: &mut Context<Self>) -> Option<Frame> {
        let _trace = crate::app::TraceRender::new("grid prepare");
        let rs = self.rs.clone()?;
        let snap = rs.snapshot();
        let ts = window.text_system().clone();
        let size12 = px(12.);
        // Advance of one monospace character, measured on shaped text.
        let probe = ts.shape_line("0000000000".into(), size12, &[run(10, mono(), t.tx)], None);
        self.char_w = probe.width.pt() / 10.;
        self.ensure_widths(&snap);
        self.follow_font();
        let data_rows = snap.rows();
        self.follow_rows(data_rows);
        let decor = self.decor.clone().filter(|d| !d.is_empty());
        let rows = data_rows + decor.as_ref().map_or(0, |d| d.virtual_rows.len());
        let phase = rs.phase();
        let status = rs.status();
        // An FK "Go to" opened this tab: the key of the row it landed on pops (yellow) as soon
        // as the row is here.
        if let Some(key) = &self.land {
            if data_rows > 0 {
                if let Some(col) = snap.columns.iter().position(|c| &c.name == key).and_then(|c| self.dc(c)) {
                    self.pop = FindPop::start(CellPos { row: 0, col }, PopStyle::Find, cx);
                }
                self.land = None;
            } else if phase.is_finished() {
                self.land = None;
            }
        }
        let pop = self.pop_frame(window);
        // A message row after the last row: paused, cancelled, or an error after rows.
        let message = match phase {
            Phase::Paused => Some((format!("┄┄ {} rows · paused", ui_count(data_rows)), t.info, t.info_s)),
            Phase::Cancelled if data_rows > 0 => Some((
                format!("⊘ cancelled{}", status.elapsed.map(|e| format!(" at {}", fmt_clock(e))).unwrap_or_default()),
                t.warn,
                t.warn_s,
            )),
            Phase::Failed if data_rows > 0 => Some((
                format!(
                    "✕ {}",
                    status.error.as_ref().map(|e| format!("{} {}", e.code.clone().unwrap_or_default(), e.message)).unwrap_or_default()
                ),
                t.err,
                t.err_s,
            )),
            _ if self.cancelling && data_rows > 0 && !phase.is_finished() => Some(("◌ cancel requested".to_string(), t.warn, t.warn_s)),
            _ => None,
        };
        // A table's page numbers its rows from the page's first one.
        let offset = self.table.as_ref().map_or(0, |t| t.offset);
        let num_w = self.char_w * 11. / 12.;
        let digits = ((rows + offset).max(1) as f64).log10().floor() as usize + 1;
        let gutter = (digits as f32 * num_w + 26.).max(GUTTER_MIN);
        let w = bounds.size.width.pt();
        let h = bounds.size.height.pt();
        let content_w: f32 = (0..self.dcols()).map(|d| self.dwidth(d)).sum();
        let content_h = (rows + message.is_some() as usize) as f64 * ROW_H as f64;
        self.geo = Geometry {
            bounds,
            gutter,
            body_w: (w - gutter - BAR).max(0.),
            body_h: (h - HEADER_H - BAR).max(0.),
            content_w,
            content_h,
        };
        self.clamp_scroll();
        let (sx, sy) = self.scroll;
        let first = (sy / ROW_H as f64).floor() as usize;
        // Row positions relative to the viewport, computed in f64 then narrowed.
        let ry = |r: usize| (r as f64 * ROW_H as f64 - sy) as f32;
        let visible = (self.geo.body_h / ROW_H).ceil() as usize + 1;
        let last = (first + visible).min(rows);
        // Keep the fetcher ahead of what is on screen.
        rs.request_rows(last.min(data_rows));

        // Visible columns: moved by the scroll, left out while entirely off screen.
        let mut cols: Vec<FCol> = Vec::new();
        let mut x = 0.;
        for d in 0..self.dcols() {
            let (c, wd) = (self.rc(d), self.dwidth(d));
            if x - sx + wd > 0. && x - sx < self.geo.body_w {
                cols.push(FCol { d, c, x: x - sx, w: wd });
            }
            x += wd;
        }

        let normal = mono();
        let italic = Font { style: FontStyle::Italic, ..mono() };
        let bold = Font { weight: FontWeight::SEMIBOLD, ..mono() };
        let shape = |text: String, f: &Font, size: Pixels, color: Hsla| -> ShapedLine {
            let len = text.len();
            ts.shape_line(SharedString::from(text), size, &[run(len, f.clone(), color)], None)
        };
        let max_chars = |wd: f32| ((wd - 2. * PAD) / self.char_w).floor().max(1.) as usize;
        let fit = |s: &str, n: usize| -> String {
            // One line, at most n characters.
            let mut out = String::with_capacity(s.len().min(n * 4));
            for (i, ch) in s.chars().enumerate() {
                if i + 1 == n && s.chars().nth(n).is_some() {
                    out.push('…');
                    break;
                }
                if i == n {
                    break;
                }
                out.push(match ch {
                    '\n' | '\r' | '\t' => ' ',
                    c => c,
                });
            }
            out
        };

        // Matching cells of an open find bar: the visible rows' slice of the list (binary search),
        // on visible columns only.
        let mut found = Vec::new();
        if let Some(f) = self.find.as_ref().filter(|f| f.open && !f.hits.is_empty()) {
            let lo = f.hits.partition_point(|&(r, _)| (r as usize) < first);
            let hi = f.hits.partition_point(|&(r, _)| (r as usize) < last).max(lo);
            // The active cell (the current match) keeps the plain selection.
            let current = self.active_cell().map(|(r, c)| (r as u32, c as u32));
            for &(r, c) in f.hits[lo..hi].iter().filter(|h| Some(**h) != current) {
                if let Some(fc) = cols.iter().find(|fc| fc.c == c as usize) {
                    found.push((fc.x, ry(r as usize), fc.w));
                }
            }
        }
        let active = self.active_cell();
        // References are drawn in the accent's text color (one flag per column, no extra paint).
        let fk_cols = self.fk_cols.clone();
        let mut cells = Vec::with_capacity((last - first) * cols.len());
        let mut nulls = Vec::new();
        let mut row_marks: Vec<RowPaint> = Vec::new();
        let mut cell_marks: Vec<CellPaint> = Vec::new();
        let mut pop_text: Option<(ShapedLine, bool)> = None;
        for r in first..last {
            let y = ry(r);
            // Decorations of this row: by storage row, or a virtual row after the result.
            let (rdec, vrow, src) = match &decor {
                None => (None, None, r),
                Some(d) if r >= data_rows => {
                    let v = &d.virtual_rows[r - data_rows];
                    (Some(&v.row), Some(v), r)
                }
                Some(d) => {
                    let s = snap.source_row(r);
                    (d.rows.get(&s), None, s)
                }
            };
            if let Some(rd) = rdec {
                row_marks.push(RowPaint {
                    y,
                    tint: rd.tint,
                    edge: rd.edge,
                    mark: rd.mark.as_ref().map(|(m, tone)| shape(m.to_string(), &bold, px(11.), tone_strong(t, *tone))),
                });
            }
            for fc in &cols {
                let (c, cx_, wd) = (fc.c, fc.x, fc.w);
                let kind = snap.columns[c].kind;
                let cdec = match (&decor, vrow) {
                    (_, Some(v)) => v.cells.get(c),
                    (Some(d), None) if !d.cells.is_empty() => d.cells.get(&(src, c)),
                    _ => None,
                };
                if rdec.is_some() || cdec.is_some() || vrow.is_some() {
                    if let Some(tint) = cdec.and_then(|cd| cd.tint) {
                        cell_marks.push(CellPaint { x: cx_, y, w: wd, tint });
                    }
                    let value: Option<String> = match cdec.and_then(|d| d.text.clone()) {
                        Some(over) => over,
                        None if vrow.is_some() => continue,
                        None => snap.text(r, c).map(|v| v.into_owned()),
                    };
                    if value.is_none() {
                        nulls.push((cx_, y, wd));
                    }
                    let line = decorated_line(
                        &ts,
                        t,
                        value.as_deref(),
                        cdec.and_then(|d| d.old.as_ref()).map(|o| o.as_deref()),
                        kind,
                        rdec.is_some_and(|d| d.strike),
                        cdec.is_some_and(|d| d.ghost),
                        cdec.and_then(|d| d.color).map(|c| tone_strong(t, c)),
                        rdec.and_then(|d| d.opacity).unwrap_or(1.),
                        max_chars(wd),
                    );
                    let right = value.is_some() && kind.is_numeric() && cdec.and_then(|d| d.old.as_ref()).is_none();
                    // (A pop on a decorated cell scales its box only.)
                    cells.push(CellText { x: cx_, y, w: wd, line, right, scale: 1. });
                    continue;
                }
                let (text, face, color, right) = match snap.text(r, c) {
                    None => {
                        nulls.push((cx_, y, wd));
                        ("NULL".to_string(), &italic, t.null, false)
                    }
                    Some(v) if v.is_empty() => {
                        // Empty is drawn as nothing; `""` shows only on the active cell.
                        if active == Some((r, c)) {
                            ("\"\"".to_string(), &normal, t.tx4, false)
                        } else {
                            continue;
                        }
                    }
                    Some(v) => {
                        let color = match kind {
                            _ if fk_cols.get(c).copied().unwrap_or(false) => t.acc_t,
                            Kind::Json => t.tx2,
                            Kind::Bool if v == "true" || v == "t" => t.ok,
                            Kind::Bool => t.tx3,
                            _ => t.tx,
                        };
                        (fit(&v, max_chars(wd)), &normal, color, kind.is_numeric())
                    }
                };
                let mut scale = 1.;
                if let Some((_, style, f)) = pop.filter(|(p, _, _)| p.row == r && p.col == fc.d) {
                    match style {
                        // A click: the value grows with its box (shaped a little larger: one
                        // cell, for a few frames).
                        PopStyle::Select => scale = f.scale,
                        // A find: the yellow box carries its own dark copy of the value.
                        PopStyle::Find => pop_text = Some((shape(text.clone(), face, size12 * f.scale, t.find_tx.opacity(f.strength)), right)),
                    }
                }
                cells.push(CellText { x: cx_, y, w: wd, line: shape(text, face, size12 * scale, color), right, scale });
            }
        }
        let sel_rows = self.sel.map(|s| s.rows());
        let gutter_lines = (first..last)
            .map(|r| {
                let selected = sel_rows.as_ref().is_some_and(|x| x.contains(&r));
                if r >= data_rows
                    && let Some(v) = decor.as_ref().and_then(|d| d.virtual_rows.get(r - data_rows))
                {
                    let color = v.row.mark.as_ref().map_or(t.tx3, |(_, tone)| tone_strong(t, *tone));
                    return (ry(r), shape(v.label.to_string(), &normal, px(11.), color));
                }
                (ry(r), shape(ui_count(r + 1 + offset), &normal, px(11.), if selected { t.tx } else { t.tx4 }))
            })
            .collect();
        // A table's data view shows the server's sort; any other result its own.
        let sort = match &self.table {
            Some(t) => t.sort,
            None => rs.sort_state(),
        };
        let headers = cols
            .iter()
            .map(|fc| {
                let (c, cx_, wd) = (fc.c, fc.x, fc.w);
                let col = &snap.columns[c];
                let arrow = match sort {
                    Some((sc, SortDir::Asc)) if sc == c => Some(shape("▲".into(), &normal, px(9.), t.acc)),
                    Some((sc, SortDir::Desc)) if sc == c => Some(shape("▼".into(), &normal, px(9.), t.acc)),
                    _ => None,
                };
                let room = max_chars(wd).saturating_sub(if arrow.is_some() { 2 } else { 0 });
                let name = fit(&col.name, room);
                let ty_room = ((room.saturating_sub(name.chars().count() + 1)) as f32 * 12. / 10.5) as usize;
                let ty_name = if status.text_mode { format!("{}?", col.type_name) } else { col.type_name.clone() };
                let ty = if ty_room >= 3 { fit(&ty_name, ty_room) } else { String::new() };
                Header {
                    x: cx_,
                    w: wd,
                    name: shape(name, &bold, size12, t.tx),
                    ty: shape(ty, &normal, px(10.5), t.tx3),
                    arrow,
                    right: col.kind.is_numeric(),
                }
            })
            .collect();
        let message = message.map(|(a, color, bg)| (ry(rows), shape(a, &normal, px(11.5), color), bg));
        let hash = shape("#".into(), &normal, px(10.5), t.tx4);
        let pop = pop.map(|(cell, style, f)| GridPop { cell, style, f, text: pop_text });
        // The grab zones on the header's column borders take the column-resize cursor (not
        // while cells or a scrollbar are being dragged).
        let resize_hits = if matches!(self.drag, None | Some(Drag::Resize { .. } | Drag::Header { .. })) {
            let left = bounds.origin.x.pt() + self.geo.gutter;
            let top = bounds.origin.y.pt();
            cols.iter()
                .filter_map(|fc| {
                    let edge = left + fc.x + fc.w;
                    let a = (edge - RESIZE_GRAB).max(left);
                    let w = edge + RESIZE_GRAB - a;
                    (w > 0.).then(|| window.insert_hitbox(Bounds::new(point(px(a), px(top)), gpui::size(px(w), px(HEADER_H))), gpui::HitboxBehavior::Normal))
                })
                .collect()
        } else {
            Vec::new()
        };
        Some(Frame {
            resize_hits,
            cells,
            nulls,
            found,
            gutter_lines,
            headers,
            cols,
            first,
            last,
            message,
            hash,
            row_marks,
            cell_marks,
            pop,
        })
    }
}

/// A visible column of a frame: display and result index, left edge relative to the cells
/// area (as drawn), and width.
#[derive(Clone, Copy, Debug)]
struct FCol {
    d: usize,
    c: usize,
    x: f32,
    w: f32,
}

struct CellText {
    x: f32,
    y: f32,
    w: f32,
    line: ShapedLine,
    right: bool,
    /// ≠ 1 only for the value of a click's pop: shaped larger, drawn scaled about the cell's
    /// centre.
    scale: f32,
}

/// The pop of this frame (see `ResultGrid::pop`).
struct GridPop {
    cell: CellPos,
    style: PopStyle,
    f: PopFrame,
    /// A find's dark copy of the value (shaped at the scaled size), and whether it is
    /// right-aligned.
    text: Option<(ShapedLine, bool)>,
}

struct Header {
    x: f32,
    w: f32,
    name: ShapedLine,
    ty: ShapedLine,
    arrow: Option<ShapedLine>,
    right: bool,
}

struct Frame {
    cells: Vec<CellText>,
    /// NULL cells: (x, y, width), painted on `null_b`.
    nulls: Vec<(f32, f32, f32)>,
    /// Visible cells matching the find bar: (x, y, width), painted on `find_s`.
    found: Vec<(f32, f32, f32)>,
    gutter_lines: Vec<(f32, ShapedLine)>,
    /// One per visible column, in `cols` order.
    headers: Vec<Header>,
    cols: Vec<FCol>,
    /// The header's column-border grab zones (column-resize cursor).
    resize_hits: Vec<gpui::Hitbox>,
    first: usize,
    last: usize,
    /// The row after the last one: (y, message, background).
    message: Option<(f32, ShapedLine, Hsla)>,
    hash: ShapedLine,
    /// Decorated rows (tint, left edge, gutter mark) and cells (tint, changed bar).
    row_marks: Vec<RowPaint>,
    cell_marks: Vec<CellPaint>,
    pop: Option<GridPop>,
}

struct RowPaint {
    y: f32,
    tint: Option<Tone>,
    edge: Option<Tone>,
    mark: Option<ShapedLine>,
}

struct CellPaint {
    x: f32,
    y: f32,
    w: f32,
    tint: Tone,
}

fn paint_frame(grid: &Entity<ResultGrid>, frame: Frame, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    for hb in &frame.resize_hits {
        window.set_cursor_style(CursorStyle::ResizeColumn, hb);
    }
    // A column being resized keeps the cursor wherever the pointer goes.
    if matches!(grid.read(cx).drag, Some(Drag::Resize { .. })) {
        window.set_window_cursor_style(CursorStyle::ResizeColumn);
    }
    // A selection being dragged follows the pointer anywhere in the window and past it (AppKit
    // keeps sending a drag's moves to the window it started in): the grid's own move listener
    // only hears the pointer over the grid.
    if matches!(grid.read(cx).drag, Some(Drag::Cells | Drag::Rows)) {
        let grid = grid.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
            if phase != gpui::DispatchPhase::Bubble {
                return;
            }
            grid.update(cx, |g, cx| {
                if ev.pressed_button != Some(MouseButton::Left) {
                    g.drag = None;
                    g.auto = None;
                    g.press = None;
                    cx.notify();
                    return;
                }
                g.drag_to(ev.position, cx);
            });
        });
    }
    let t = cx.theme();
    let g = grid.read(cx);
    let geo = g.geo;
    let sel = g.sel;
    // The open cell menu and the find bar hold the keys, but the grid still reads as focused
    // under them (the active cell is the current match).
    let focused = g.focus.is_focused(window) || g.menu_focus.is_focused(window) || g.find_has_focus(window, cx);
    let sy = g.scroll.1;
    let ry = |r: usize| (r as f64 * ROW_H as f64 - sy) as f32;
    let (vthumb, hthumb) = (g.v_thumb(), g.h_thumb());
    let ncols = g.dcols();
    let o = bounds.origin;
    let at = |x: f32, y: f32| point(o.x + px(x), o.y + px(y));
    let rect = |x: f32, y: f32, w: f32, h: f32| Bounds::new(at(x, y), size(px(w), px(h)));
    let lh = px(ROW_H);
    let full_w = bounds.size.width.pt();
    let sel_color = if focused { t.sel } else { t.prs };
    // A click's pop sits on the single selected cell (`pop_frame` checked it).
    let click_pop = frame.pop.as_ref().filter(|p| p.style == PopStyle::Select && sel.is_some_and(|s| s.single() && s.head == p.cell));
    // The popping cell at rest, when on screen. A pop's enlarged box may reach past the body
    // into the gutter (a first column is the usual key): it paints under the rows' mask, not
    // the body's.
    let pop_rect = frame.pop.as_ref().and_then(|p| {
        let fc = frame.cols.iter().find(|fc| fc.d == p.cell.col)?;
        let (x, w) = (fc.x, fc.w);
        (frame.first..frame.last).contains(&p.cell.row).then(|| (x, w, rect(geo.gutter + x, HEADER_H + ry(p.cell.row), w - 1., ROW_H)))
    });
    let rows_area = gpui::ContentMask { bounds: rect(0., HEADER_H, full_w, geo.body_h) };
    if let (Some(p), Some((_, _, b))) = (click_pop, pop_rect) {
        // A click's pop: the box springs back from 1.15× and its blue fades to the selection's.
        window.with_content_mask(Some(rows_area.clone()), |window| {
            crate::motion::paint_pop(window, cx, b, CELL_RADIUS, crate::theme::mix(sel_color, t.sel_hi, p.f.strength), p.f.scale, None);
        });
    }

    // No background: the work surface paints the translucent fill once under the grid; only
    // overlays (zebra rows, NULL, selection, marks) are painted here.
    let body = rect(geo.gutter, HEADER_H, geo.body_w, geo.body_h);
    window.with_content_mask(Some(gpui::ContentMask { bounds: body }), |window| {
        // Every other row, and on the translucent dark surface the rows between at the same
        // alpha (`Theme::alt_base`): one quad per row at most.
        let base = t.alt_base.a > 0.;
        for r in frame.first..frame.last {
            if r % 2 == 1 {
                window.paint_quad(fill(rect(geo.gutter, HEADER_H + ry(r), geo.body_w, ROW_H), t.alt));
            } else if base {
                window.paint_quad(fill(rect(geo.gutter, HEADER_H + ry(r), geo.body_w, ROW_H), t.alt_base));
            }
        }
        for rm in &frame.row_marks {
            if let Some(tone) = rm.tint {
                window.paint_quad(fill(rect(geo.gutter, HEADER_H + rm.y, geo.body_w, ROW_H), tone_soft(&t, tone)));
            }
        }
    });
    window.with_content_mask(Some(gpui::ContentMask { bounds: rect(geo.gutter, HEADER_H, geo.body_w, geo.body_h) }), |window| {
        for &(x, y, w) in &frame.nulls {
            window.paint_quad(fill(rect(geo.gutter + x, HEADER_H + y, w, ROW_H), t.null_b));
        }
        for &(x, y, w) in &frame.found {
            window.paint_quad(fill(rect(geo.gutter + x, HEADER_H + y, w - 1., ROW_H), t.find_s));
        }
        for cm in &frame.cell_marks {
            window.paint_quad(fill(rect(geo.gutter + cm.x, HEADER_H + cm.y, cm.w - 1., ROW_H), tone_soft(&t, cm.tint)));
        }
        // Selection.
        if let Some(s) = sel {
            let rows = s.rows();
            let cols = s.cols(ncols);
            for fc in &frame.cols {
                let (x, w) = (fc.x, fc.w);
                if !cols.contains(&fc.d) {
                    continue;
                }
                let r0 = (*rows.start()).max(frame.first);
                let r1 = (*rows.end() + 1).min(frame.last);
                if r0 >= r1 {
                    continue;
                }
                if s.single() {
                    // Same bounds as the active ring, so the rounded fill sits inside it (a
                    // popping one was painted above).
                    if click_pop.is_none() {
                        let b = rect(geo.gutter + x, HEADER_H + ry(r0), w - 1., ROW_H);
                        window.paint_quad(gpui::quad(b, px(CELL_RADIUS), sel_color, px(0.), gpui::transparent_black(), gpui::BorderStyle::Solid));
                    }
                } else {
                    window.paint_quad(fill(rect(geo.gutter + x, HEADER_H + ry(r0), w, (r1 - r0) as f32 * ROW_H), sel_color));
                }
            }
        }
        // Column borders: one quad per column, down to the last row.
        let rows_h = ((geo.content_h - sy) as f32).clamp(0., geo.body_h);
        for fc in &frame.cols {
            window.paint_quad(fill(rect(geo.gutter + fc.x + fc.w - 1., HEADER_H, 1., rows_h), t.grid_line));
        }
        for c in &frame.cells {
            // Clip to the cell: wide glyphs (CJK, emoji) must not spill into the next column.
            let cell = rect(geo.gutter + c.x, HEADER_H + c.y, c.w - 1., ROW_H);
            if c.scale != 1. {
                // The clicked value, growing with its box: laid out as at rest, then scaled
                // about the cell's centre (a right-aligned number keeps hugging the right edge).
                let rest = at(geo.gutter + if c.right { c.x + c.w - PAD } else { c.x + PAD }, HEADER_H + c.y);
                let p = crate::motion::scale_point(rest, cell.center(), c.scale);
                let x = if c.right { p.x - c.line.width } else { p.x };
                window.with_content_mask(Some(gpui::ContentMask { bounds: crate::motion::scale_bounds(cell, c.scale) }), |window| {
                    let _ = c.line.paint(point(x, p.y), lh * c.scale, TextAlign::Left, None, window, cx);
                });
                continue;
            }
            let x = if c.right { c.x + c.w - PAD - c.line.width.pt() } else { c.x + PAD };
            window.with_content_mask(Some(gpui::ContentMask { bounds: cell }), |window| {
                let _ = c.line.paint(at(geo.gutter + x, HEADER_H + c.y), lh, TextAlign::Left, None, window, cx);
            });
        }
        if let Some(s) = sel
            && focused
            && click_pop.is_none()
            && let Some(fc) = frame.cols.iter().find(|fc| fc.d == s.head.col)
            && (!s.rows || s.anchor.row == s.head.row)
        {
            let y = HEADER_H + ry(s.head.row);
            window.paint_quad(gpui::quad(
                rect(geo.gutter + fc.x, y, fc.w - 1., ROW_H),
                px(CELL_RADIUS),
                gpui::transparent_black(),
                px(1.5),
                t.acc,
                gpui::BorderStyle::Solid,
            ));
        }
    });
    window.with_content_mask(Some(gpui::ContentMask { bounds: body }), |window| {
        for rm in &frame.row_marks {
            if let Some(tone) = rm.edge {
                window.paint_quad(fill(rect(geo.gutter, HEADER_H + rm.y, 3., ROW_H), tone_strong(&t, tone)));
            }
        }
    });
    // The active ring pops with its box.
    if let (Some(p), Some((_, _, b)), true) = (click_pop, pop_rect, focused) {
        window.with_content_mask(Some(rows_area.clone()), |window| {
            window.paint_quad(gpui::quad(
                crate::motion::scale_bounds(b, p.f.scale),
                px(CELL_RADIUS * p.f.scale),
                gpui::transparent_black(),
                px(1.5),
                t.acc,
                gpui::BorderStyle::Solid,
            ));
        });
    }
    // The message row spans the whole width (gutter included).
    if let Some((y, msg, bg)) = &frame.message {
        let top = HEADER_H + y;
        if top < HEADER_H + geo.body_h {
            window.with_content_mask(Some(gpui::ContentMask { bounds: rect(0., HEADER_H, full_w, geo.body_h) }), |window| {
                window.paint_quad(fill(rect(geo.gutter, top, full_w - geo.gutter, ROW_H), *bg));
                let _ = msg.paint(at(geo.gutter + 12., top), lh, TextAlign::Left, None, window, cx);
            });
        }
    }
    // Row numbers (the body's mask keeps cells out of the gutter and the header).
    window.with_content_mask(Some(gpui::ContentMask { bounds: rect(0., HEADER_H, geo.gutter, geo.body_h) }), |window| {
        if let Some(s) = sel {
            let rows = s.rows();
            let r0 = (*rows.start()).max(frame.first);
            let r1 = (*rows.end() + 1).min(frame.last);
            if r0 < r1 {
                let c = if focused { t.acc_s } else { t.prs };
                window.paint_quad(fill(rect(0., HEADER_H + ry(r0), geo.gutter, (r1 - r0) as f32 * ROW_H), c));
            }
        }
        for (y, line) in &frame.gutter_lines {
            let x = geo.gutter - 10. - line.width.pt();
            let _ = line.paint(at(x, HEADER_H + y), lh, TextAlign::Left, None, window, cx);
        }
        for rm in &frame.row_marks {
            if let Some(m) = &rm.mark {
                let _ = m.paint(at(6., HEADER_H + rm.y), lh, TextAlign::Left, None, window, cx);
            }
        }
    });
    // Header.
    let _ = frame.hash.paint(at(geo.gutter - 10. - frame.hash.width.pt(), 0.), px(HEADER_H), TextAlign::Left, None, window, cx);
    window.with_content_mask(Some(gpui::ContentMask { bounds: rect(geo.gutter, 0., geo.body_w + BAR, HEADER_H) }), |window| {
        let sel_cols = sel.map(|s| s.cols(ncols));
        for (h, fc) in frame.headers.iter().zip(&frame.cols) {
            let x0 = geo.gutter + h.x;
            if sel_cols.as_ref().is_some_and(|r| r.contains(&fc.d)) && sel.is_some_and(|s| !s.rows) {
                window.paint_quad(fill(rect(x0, 0., h.w, HEADER_H - 1.), if focused { t.acc_s } else { t.prs }));
            }
            let hl = px(HEADER_H);
            let label_w = h.name.width.pt() + if h.ty.width.pt() > 0. { 6. + h.ty.width.pt() } else { 0. };
            let arrow_w = h.arrow.as_ref().map_or(0., |a| a.width.pt() + 6.);
            // Numbers: the label hugs the right edge, like the values below it.
            let lx = if h.right { (x0 + h.w - PAD - arrow_w - label_w).max(x0 + PAD) } else { x0 + PAD };
            let _ = h.name.paint(at(lx, 0.), hl, TextAlign::Left, None, window, cx);
            let _ = h.ty.paint(at(lx + h.name.width.pt() + 6., 0.), hl, TextAlign::Left, None, window, cx);
            if let Some(a) = &h.arrow {
                let _ = a.paint(at(x0 + h.w - PAD - a.width.pt(), 0.), hl, TextAlign::Left, None, window, cx);
            }
            window.paint_quad(fill(rect(x0 + h.w - 1., 0., 1., HEADER_H), t.line));
        }
    });
    window.paint_quad(fill(rect(0., HEADER_H - 1., full_w, 1.), t.line2));
    window.paint_quad(fill(rect(geo.gutter - 1., 0., 1., bounds.size.height.pt()), t.line));
    // Scrollbars (6 pt thumbs, line2).
    let thumb = |b: Bounds<Pixels>| fill(b, t.line2);
    if geo.content_h > geo.body_h as f64 {
        window.paint_quad(thumb(rect(geo.gutter + geo.body_w + 2., HEADER_H + vthumb.0, 6., vthumb.1)));
    }
    if geo.content_w > geo.body_w {
        window.paint_quad(thumb(rect(geo.gutter + hthumb.0, HEADER_H + geo.body_h + 2., hthumb.1, 6.)));
    }
    // A find's pop (the row an FK "Go to" landed on), last, over everything: Xcode's yellow
    // box with its dark copy of the value on top, the pair fading out once the box has sprung
    // back (by then the copy sits exactly over the value). On the first row it may rise a few
    // points over the header's bottom line.
    if let (Some(p), Some((x, w, b))) = (frame.pop.as_ref().filter(|p| p.style == PopStyle::Find), pop_rect) {
        let text = p.text.as_ref().map(|(line, right)| {
            // Resting origin: what the scaled line would be at 1×.
            let rest_x = if *right { x + w - PAD - line.width.pt() / p.f.scale } else { x + PAD };
            (line, at(geo.gutter + rest_x, HEADER_H + ry(p.cell.row)), lh)
        });
        let area = gpui::ContentMask { bounds: rect(0., HEADER_H - 4., full_w, geo.body_h + 4.) };
        window.with_content_mask(Some(area), |window| {
            crate::motion::paint_pop(window, cx, b, CELL_RADIUS, t.find.opacity(p.f.strength), p.f.scale, text);
        });
    }
}

/// `10k`: how far ⌘↓ fetches (Settings ▸ Results grid).
pub fn fetch_step() -> String {
    let n = diskman_core::result::prefetch();
    if n % 1000 == 0 { format!("{}k", n / 1000) } else { ui_count(n) }
}

/// `1,234,567`.
pub fn ui_count(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `2,184.20`.
fn fmt_num(v: f64) -> String {
    let neg = v < 0.;
    let s = format!("{:.2}", v.abs());
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let int = int.parse::<usize>().map(ui_count).unwrap_or_else(|_| int.to_string());
    format!("{}{int}.{frac}", if neg { "-" } else { "" })
}

/// `00:41.9`.
pub fn fmt_clock(d: std::time::Duration) -> String {
    let s = d.as_secs_f64();
    format!("{:02}:{:04.1}", (s / 60.) as u64, s % 60.)
}

impl EventEmitter<GridEvent> for ResultGrid {}

/// A grid action. It first closes the cell menu: the keys the menu does not take (←→, ⌘C,
/// ⇧arrows…) reach the grid through it, and act on the grid as usual.
fn key<A: gpui::Action>(
    cx: &mut Context<ResultGrid>,
    f: impl Fn(&mut ResultGrid, &mut Context<ResultGrid>) + 'static,
) -> impl Fn(&A, &mut Window, &mut App) + 'static {
    cx.listener(move |g, _: &A, window, cx| {
        if g.menu.is_some() {
            g.close_menu(true, window, cx);
        }
        f(g, cx)
    })
}

impl Render for ResultGrid {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let entity = cx.entity();
        let entity2 = entity.clone();
        let rs = self.rs.clone();
        let find_open = self.find.as_ref().is_some_and(|f| f.open);
        let mut root = div()
            .size_full()
            .relative()
            .key_context(if find_open { "ResultGrid find" } else { "ResultGrid" })
            .track_focus(&self.focus)
            .font_family(crate::theme::mono())
            .text_size(px(12.))
            .on_action(key::<MoveUp>(cx, |g, cx| g.move_by(-1, 0, false, cx)))
            .on_action(key::<MoveDown>(cx, |g, cx| g.move_by(1, 0, false, cx)))
            .on_action(key::<MoveLeft>(cx, |g, cx| g.move_by(0, -1, false, cx)))
            .on_action(key::<MoveRight>(cx, |g, cx| g.move_by(0, 1, false, cx)))
            .on_action(key::<SelectUp>(cx, |g, cx| g.move_by(-1, 0, true, cx)))
            .on_action(key::<SelectDown>(cx, |g, cx| g.move_by(1, 0, true, cx)))
            .on_action(key::<SelectLeft>(cx, |g, cx| g.move_by(0, -1, true, cx)))
            .on_action(key::<SelectRight>(cx, |g, cx| g.move_by(0, 1, true, cx)))
            .on_action(key::<PageUp>(cx, |g, cx| {
                let p = g.page();
                g.move_by(-p, 0, false, cx)
            }))
            .on_action(key::<PageDown>(cx, |g, cx| {
                let p = g.page();
                g.move_by(p, 0, false, cx)
            }))
            .on_action(key::<Top>(cx, |g, cx| g.move_by(isize::MIN / 2, 0, false, cx)))
            .on_action(key::<Bottom>(cx, |g, cx| g.move_by(isize::MAX / 2, 0, false, cx)))
            .on_action(key::<RowStart>(cx, |g, cx| g.move_by(0, isize::MIN / 2, false, cx)))
            .on_action(key::<RowEnd>(cx, |g, cx| g.move_by(0, isize::MAX / 2, false, cx)))
            .on_action(key::<SelectRowStart>(cx, |g, cx| g.move_by(0, isize::MIN / 2, true, cx)))
            .on_action(key::<SelectRowEnd>(cx, |g, cx| g.move_by(0, isize::MAX / 2, true, cx)))
            .on_action(key::<SelectAll>(cx, |g, cx| g.select_all(cx)))
            .on_action(key::<CopyTsv>(cx, |g, cx| g.copy(CopyFormat::Tsv, false, cx)))
            .on_action(key::<CopyCsv>(cx, |g, cx| g.copy(CopyFormat::Csv, false, cx)))
            .on_action(key::<CopyTsvWithHeader>(cx, |g, cx| g.copy(CopyFormat::Tsv, true, cx)))
            .on_action(key::<FetchMore>(cx, |g, _| {
                if let Some(rs) = &g.rs {
                    rs.request_rows(rs.rows());
                }
            }))
            .on_action(key::<FetchAll>(cx, |g, _| {
                if let Some(rs) = &g.rs {
                    rs.fetch_all();
                }
            }))
            .on_action(key::<ToggleViewer>(cx, |_, cx| cx.emit(GridEvent::ToggleViewer)))
            .on_action(key::<EditCell>(cx, |g, cx| {
                g.follow_now();
                if let Some((row, col)) = g.sel.map(|s| (s.head.row, g.rc(s.head.col))) {
                    cx.emit(GridEvent::EditCell { row, col });
                }
            }))
            .on_action(key::<InsertRow>(cx, |_, cx| cx.emit(GridEvent::InsertRow)))
            .on_action(key::<DeleteRows>(cx, |g, cx| {
                let rows = g.selected_rows();
                if !rows.is_empty() {
                    cx.emit(GridEvent::DeleteRows(rows));
                }
            }))
            .on_action(cx.listener(|g, _: &OpenCellMenu, window, cx| g.open_menu_from_keys(window, cx)))
            // ⌘F (and the menu's Replace…: the grid finds only).
            .on_action(cx.listener(|g, _: &find::Find, window, cx| g.open_find(window, cx)))
            .on_action(cx.listener(|g, _: &find::FindReplace, window, cx| g.open_find(window, cx)))
            .on_action(cx.listener(|g, _: &find::FindNext, window, cx| g.find_step(true, window, cx)))
            .on_action(cx.listener(|g, _: &find::FindPrevious, window, cx| g.find_step(false, window, cx)))
            .on_action(cx.listener(|g, _: &find::CloseFind, _, cx| g.close_find(cx)))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            // A release outside the grid ends a drag; it is never a click.
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|g, ev: &MouseUpEvent, window, cx| {
                    g.press = None;
                    g.on_mouse_up(ev, window, cx)
                }),
            );
        let has_cols = rs.as_ref().is_some_and(|r| r.columns().is_some_and(|c| !c.is_empty()));
        if has_cols {
            let theme = t.clone();
            root = root.child(
                canvas(
                    move |bounds, window, cx| entity.update(cx, |g, cx| g.prepare(bounds, window, &theme, cx)),
                    move |bounds, frame, window, cx| {
                        if let Some(frame) = frame {
                            paint_frame(&entity2, frame, bounds, window, cx);
                        }
                    },
                )
                .size_full(),
            );
        }
        // A view over one cell (the inline editor), clipped to the cells area.
        if let Some(o) = &self.overlay
            && has_cols
            && o.col < self.widths.len()
            && let Some(d) = self.dc(o.col)
        {
            let g = self.geo;
            let x = self.col_left(d);
            let row = o.row.display(rs.as_ref().map_or(0, |r| r.rows()));
            let y = (row as f64 * ROW_H as f64 - self.scroll.1) as f32;
            root = root.child(
                div()
                    .absolute()
                    .left(px(g.gutter))
                    .top(px(HEADER_H))
                    .w(px(g.body_w))
                    .h(px(g.body_h))
                    .overflow_hidden()
                    .child(div().absolute().left(px(x)).top(px(y)).w(px(self.dwidth(d).max(o.min_w))).h(px(ROW_H)).child(o.view.clone())),
            );
        }
        if let Some(menu) = self.render_menu(cx) {
            root = root.child(menu);
        }
        // Empty states: a tag (and the affected count of a statement without rows).
        let center = |el: gpui::Div| div().absolute().inset_0().flex().items_center().justify_center().child(el);
        match rs.as_ref().map(|r| (r.phase(), r.status(), r.rows())) {
            None => {
                // The result panel only shows once the tab has run something, so this is a tab
                // whose live query went away (a connection change): no result left to show.
                root = root.child(center(ui::empty_state(&t, "[ NO RESULT ]", 22., None)));
            }
            Some((Phase::Done, _, 0)) if has_cols => {
                root = root.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(HEADER_H))
                        .bottom_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(ui::empty_state(&t, "[ 0 ROWS ]", 28., None)),
                );
            }
            Some((Phase::Done, s, _)) if !has_cols => {
                let verb = rs.as_ref().map(|r| statement_verb(&r.sql)).unwrap_or_default();
                let msg = s.affected.map(|n| format!("{} row{} affected", ui_count(n as usize), if n == 1 { "" } else { "s" }).into());
                root = root.child(center(ui::empty_state(&t, &format!("[ {verb} ]"), 22., msg)));
            }
            Some((Phase::Cancelled, _, 0)) if !has_cols => {
                root = root.child(center(ui::empty_state(&t, "[ CANCELLED ]", 22., None)));
            }
            _ => {}
        }
        // The find bar, docked at the top right of the rows, beside the grid's key context (its
        // field must not get the grid's keys).
        let bar = self
            .find
            .as_ref()
            .filter(|f| f.open && has_cols)
            .map(|f| div().absolute().top(px(HEADER_H + 6.)).right(px(BAR + 6.)).child(f.bar.clone()));
        div().size_full().relative().child(root).children(bar)
    }
}

/// `UPDATE`, `CREATE TABLE`: the command of a statement, for result labels.
pub use diskman_core::sql::statement_verb;

// ---------------------------------------------------------------------------------------------
// Decorations: extra paint supplied by the grid's owner.
//
// Edits (changed cells, inserted and deleted rows) and results with fading rows all need the
// same few things, so the grid offers them as data instead of hooks into its
// drawing code. Rows are keyed by *storage* row (`Snapshot::source_row`), so sorting does not
// move them; colors are semantic tones resolved against the theme at paint time. With no
// decorations (`None`, or empty) the grid's fast path is unchanged.

/// The color of a decoration, resolved against the active theme when painting (so a theme
/// switch recolors it): a soft background (`*_s`) or the strong color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Tone {
    Ok,
    Warn,
    Err,
    Info,
    Acc,
    Lar,
    Muted,
}

pub fn tone_soft(t: &Theme, tone: Tone) -> Hsla {
    match tone {
        Tone::Ok => t.ok_s,
        Tone::Warn => t.warn_s,
        Tone::Err => t.err_s,
        Tone::Info => t.info_s,
        Tone::Acc => t.acc_s,
        Tone::Lar => t.lar_s,
        Tone::Muted => t.hov,
    }
}

pub fn tone_strong(t: &Theme, tone: Tone) -> Hsla {
    match tone {
        Tone::Ok => t.ok,
        Tone::Warn => t.warn,
        Tone::Err => t.err,
        Tone::Info => t.info,
        Tone::Acc => t.acc,
        Tone::Lar => t.lar,
        Tone::Muted => t.tx3,
    }
}

/// How a whole row is drawn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowDecor {
    /// Soft background over the row (instead of the zebra stripe).
    pub tint: Option<Tone>,
    /// Text struck through in `tx3` (rows to delete, rows gone from B).
    pub strike: bool,
    /// Text opacity, 0–1 (rows fading out).
    pub opacity: Option<f32>,
    /// A short mark at the left of the row-number gutter (`+`, `−`, `~`).
    pub mark: Option<(SharedString, Tone)>,
    /// A 3 pt bar on the row's left edge (live inserts).
    pub edge: Option<Tone>,
}

/// How one cell is drawn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CellDecor {
    pub tint: Option<Tone>,
    /// Text drawn instead of the stored value (`Some(None)` draws NULL).
    pub text: Option<Option<String>>,
    /// A previous value, drawn struck through before an arrow (`89.00 → 79.00`).
    pub old: Option<Option<String>>,
    /// Muted italic (placeholders such as `default`).
    pub ghost: bool,
    /// The text in a role color (a session's state in the activity monitor).
    pub color: Option<Tone>,
}

/// A row drawn after the result's rows (rows being inserted). Its cells show `CellDecor::text`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VirtualRow {
    /// Shown in the row-number gutter instead of a number (`+`).
    pub label: SharedString,
    pub row: RowDecor,
    pub cells: Vec<CellDecor>,
}

/// Everything the owner adds to the grid. Build it when the state changes, not per frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decorations {
    /// By storage row.
    pub rows: HashMap<usize, RowDecor>,
    /// By (storage row, column).
    pub cells: HashMap<(usize, usize), CellDecor>,
    /// After the last row of the result, in order. Display row = result rows + index.
    pub virtual_rows: Vec<VirtualRow>,
}

impl Decorations {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.cells.is_empty() && self.virtual_rows.is_empty()
    }
}

/// A view laid over one cell, at least `min_w` wide.
#[derive(Clone)]
pub struct CellOverlay {
    pub row: OverlayRow,
    pub col: usize,
    pub min_w: f32,
    pub view: AnyView,
}

/// Which row an overlay sits on: a row of the result (display row), or a virtual row by its
/// index among the virtual rows — so it stays on that row when more result rows arrive
/// before it (a paused result fetching its next rows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayRow {
    Data(usize),
    Virtual(usize),
}

impl OverlayRow {
    pub fn display(self, data_rows: usize) -> usize {
        match self {
            OverlayRow::Data(r) => r,
            OverlayRow::Virtual(i) => data_rows + i,
        }
    }
}

/// A display row after the result grew from `old` to `new` rows: rows past the old end were
/// virtual rows (drawn after the result) and move down with them.
pub fn follow_growth(row: usize, old: usize, new: usize) -> usize {
    if row >= old && new > old { row + (new - old) } else { row }
}

/// One decorated cell as a shaped line: NULL, ghost, struck, faded, or `old → new`, fitted
/// to `max` characters.
#[allow(clippy::too_many_arguments)]
fn decorated_line(
    ts: &gpui::WindowTextSystem,
    t: &Theme,
    value: Option<&str>,
    old: Option<Option<&str>>,
    kind: Kind,
    strike: bool,
    ghost: bool,
    color: Option<Hsla>,
    opacity: f32,
    max: usize,
) -> ShapedLine {
    let normal = mono();
    let italic = Font { style: FontStyle::Italic, ..mono() };
    let fade = |c: Hsla| if opacity < 1. { c.opacity(opacity) } else { c };
    let base = match (value, kind) {
        _ if strike => t.tx3,
        _ if ghost => t.tx3,
        (Some(_), _) if color.is_some() => color.unwrap_or(t.tx),
        (None, _) => t.null,
        (Some(_), Kind::Json) => t.tx2,
        (Some(v), Kind::Bool) if v == "true" || v == "t" => t.ok,
        (Some(_), Kind::Bool) => t.tx3,
        _ => t.tx,
    };
    // (text, font, color, struck)
    let mut pieces: Vec<(String, Font, Hsla, bool)> = Vec::new();
    if let Some(o) = old {
        let (txt, f) = match o {
            Some(v) => (one_line(v), normal.clone()),
            None => ("NULL".to_string(), italic.clone()),
        };
        pieces.push((txt, f, fade(t.tx3), true));
        pieces.push((" → ".into(), normal.clone(), fade(t.tx4), false));
    }
    match value {
        Some(v) => pieces.push((one_line(v), if ghost { italic.clone() } else { normal.clone() }, fade(base), strike)),
        None => pieces.push(("NULL".into(), italic.clone(), fade(base), strike)),
    }
    // Fit to `max` characters, ending with an ellipsis.
    let total: usize = pieces.iter().map(|p| p.0.chars().count()).sum();
    if total > max {
        let mut left = max.saturating_sub(1);
        for p in pieces.iter_mut() {
            let n = p.0.chars().count();
            if n <= left {
                left -= n;
            } else {
                p.0 = p.0.chars().take(left).collect::<String>() + "…";
                left = 0;
            }
        }
        while pieces.last().is_some_and(|p| p.0.is_empty()) {
            pieces.pop();
        }
    }
    let mut text = String::new();
    let mut runs = Vec::new();
    for (txt, f, color, struck) in pieces {
        if txt.is_empty() {
            continue;
        }
        text.push_str(&txt);
        runs.push(TextRun {
            len: txt.len(),
            font: f,
            color,
            background_color: None,
            underline: None,
            strikethrough: struck.then(|| StrikethroughStyle { thickness: px(1.), color: Some(color) }),
        });
    }
    ts.shape_line(SharedString::from(text), px(12.), &runs, None)
}

fn one_line(s: &str) -> String {
    s.chars().map(|c| if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disp(l: &ColLayout) -> Vec<usize> {
        (0..l.shown()).filter_map(|d| l.result_col(d)).collect()
    }

    #[test]
    fn drag_autoscroll_speed_grows_past_the_edge() {
        // Inside, away from the edges: still.
        assert_eq!(axis_speed(300., 0., 600.), 0.);
        // In the band before the right edge: slow, forwards; before the left one: backwards.
        let band = axis_speed(590., 0., 600.);
        assert!(band > 0. && band < 300., "{band}");
        assert!(axis_speed(10., 0., 600.) < 0.);
        // At the edge a few hundred pt/s, farther past it faster, capped.
        let edge = axis_speed(600., 0., 600.);
        let past = axis_speed(700., 0., 600.);
        assert!(edge > band && past > 3. * edge, "{edge} {past}");
        assert_eq!(axis_speed(1e6, 0., 600.), 8000.);
        // A tiny grid keeps a band a quarter of its size.
        assert_eq!(axis_speed(20., 0., 40.), 0.);
    }

    #[test]
    fn hide_columns() {
        let mut l = ColLayout::default();
        l.ensure(5);
        assert_eq!(disp(&l), vec![0, 1, 2, 3, 4]);
        // Hidden ones are left out; the map back says so.
        assert!(l.hide(1));
        assert!(l.hide(4));
        assert_eq!(disp(&l), vec![0, 2, 3]);
        assert_eq!((l.display_col(2), l.display_col(4)), (Some(1), None));
        assert_eq!(l.hidden_count(), 2);
        l.show_all();
        assert_eq!(disp(&l), vec![0, 1, 2, 3, 4]);
        // The last column shown cannot be hidden.
        let mut one = ColLayout::default();
        one.ensure(1);
        assert!(!one.hide(0));
        // Another column count: a fresh layout.
        l.ensure(2);
        assert_eq!(disp(&l), vec![0, 1]);
    }
}
