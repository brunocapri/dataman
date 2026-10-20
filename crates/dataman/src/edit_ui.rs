use std::sync::Arc;

use gpui::{
    App, ClipboardItem, Context, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    KeyBinding, MouseButton, Subscription, Task, Window, actions, div, prelude::*,
};
use crate::theme::px;
use dataman_core::edit::{
    ApplyReport, CellValue, ChangeSet, ConflictKind, EditTarget, Editability, Outcome, PlannedChange, preview,
};
use dataman_core::result::{Phase, ResultSet, Snapshot};
use dataman_core::sql::lexer::{TokenKind, tokenize};
use dataman_core::value::Kind;
use dataman_core::Connection;

use crate::grid::{CellDecor, CellOverlay, Decorations, GridEvent, OverlayRow, ResultGrid, RowDecor, Tone, VirtualRow};
use crate::input::{InputEvent, TextInput};
use crate::theme::{ActiveTheme, Theme};
use crate::ui::{self, Btn};
use crate::value_edit::{ValueEditor, ValueEvent};

actions!(cell_editor, [CancelCell, NextCell, PrevCell, CellUp, CellDown, ToggleNull, TypeSpace]);

const CONTEXT: &str = "GridCellEditor";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("escape", CancelCell, c),
        KeyBinding::new("tab", NextCell, c),
        KeyBinding::new("shift-tab", PrevCell, c),
        KeyBinding::new("up", CellUp, c),
        KeyBinding::new("down", CellDown, c),
        KeyBinding::new("cmd-shift-n", ToggleNull, c),
        // The grid binds these; inside the field they type.
        KeyBinding::new("shift-space", TypeSpace, c),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowRef {
    Stored(usize),
    New(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Stay,
    Up,
    Down,
    Next,
    Prev,
}

pub fn typing_filter(kind: Kind, type_name: &str) -> Option<fn(&str) -> bool> {
    match kind {
        Kind::Int => Some(int_prefix),
        Kind::Float | Kind::Float4 | Kind::Numeric => Some(number_prefix),
        Kind::Other if matches!(type_name.to_ascii_lowercase().as_str(), "uuid" | "uniqueidentifier") => Some(uuid_prefix),
        _ => None,
    }
}

fn int_prefix(s: &str) -> bool {
    let d = s.strip_prefix(['-', '+']).unwrap_or(s);
    d.bytes().all(|b| b.is_ascii_digit())
}

fn number_prefix(s: &str) -> bool {
    let d = s.strip_prefix(['-', '+']).unwrap_or(s);
    let word = d.to_ascii_lowercase();
    if !word.is_empty() && ["nan", "infinity"].iter().any(|w| w.starts_with(&word)) {
        return true;
    }
    let (mant, exp) = match d.find(['e', 'E']) {
        Some(i) => (&d[..i], Some(&d[i + 1..])),
        None => (d, None),
    };
    let mant_ok = mant.bytes().filter(|&b| b == b'.').count() <= 1 && mant.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    let exp_ok = exp.is_none_or(|e| mant.bytes().any(|b| b.is_ascii_digit()) && int_prefix(e));
    mant_ok && exp_ok
}

fn uuid_prefix(s: &str) -> bool {
    s.len() <= 38 && s.bytes().all(|b| b.is_ascii_hexdigit() || matches!(b, b'-' | b'{' | b'}'))
}

pub fn step(row: usize, col: usize, rows: usize, cols: usize, m: Move) -> (usize, usize) {
    let (rows, cols) = (rows.max(1), cols.max(1));
    match m {
        Move::Stay => (row, col),
        Move::Up => (row.saturating_sub(1), col),
        Move::Down => ((row + 1).min(rows - 1), col),
        Move::Next if col + 1 < cols => (row, col + 1),
        Move::Next if row + 1 < rows => (row + 1, 0),
        Move::Next => (row, col),
        Move::Prev if col > 0 => (row, col - 1),
        Move::Prev if row > 0 => (row - 1, cols - 1),
        Move::Prev => (row, col),
    }
}

#[derive(Clone, Debug, Default)]
pub struct EditModel {
    pub changes: ChangeSet,
    ncols: usize,
}

impl EditModel {
    pub fn new(ncols: usize) -> Self {
        EditModel { changes: ChangeSet::new(), ncols }
    }

    pub fn set(&mut self, at: RowRef, col: usize, orig: CellValue, new: Option<CellValue>) {
        match at {
            RowRef::Stored(r) => {
                if let Some(v) = new {
                    self.changes.edit(r, col, orig, v);
                } else {
                    self.changes.revert_cell(r, col);
                }
            }
            RowRef::New(i) => self.changes.set_inserted(i, col, new),
        }
    }

    pub fn current(&self, at: RowRef, col: usize, stored: impl FnOnce() -> CellValue) -> Option<CellValue> {
        match at {
            RowRef::Stored(r) => Some(self.changes.edited(r, col).map(|e| e.new.clone()).unwrap_or_else(stored)),
            RowRef::New(i) => self.changes.inserted().get(i).and_then(|v| v.get(col).cloned().flatten()),
        }
    }

    pub fn insert(&mut self) -> usize {
        self.changes.insert_row(self.ncols)
    }

    pub fn toggle_delete(&mut self, rows: &[RowRef]) {
        let stored: Vec<usize> = rows.iter().filter_map(|r| if let RowRef::Stored(s) = r { Some(*s) } else { None }).collect();
        let mut news: Vec<usize> = rows.iter().filter_map(|r| if let RowRef::New(i) = r { Some(*i) } else { None }).collect();
        news.sort_unstable();
        for i in news.into_iter().rev() {
            self.changes.remove_inserted(i);
        }
        if stored.is_empty() {
            return;
        }
        let all = stored.iter().all(|&r| self.changes.is_deleted(r));
        for r in stored {
            if all {
                self.changes.undelete_row(r);
            } else {
                self.changes.delete_row(r);
            }
        }
    }

    pub fn undo_row(&mut self, at: RowRef) {
        match at {
            RowRef::Stored(r) => {
                self.changes.undelete_row(r);
                let cols: Vec<usize> = self.changes.edited_rows().find(|(x, _)| *x == r).map(|(_, e)| e.keys().copied().collect()).unwrap_or_default();
                for c in cols {
                    self.changes.revert_cell(r, c);
                }
            }
            RowRef::New(i) => self.changes.remove_inserted(i),
        }
    }

    pub fn pending(&self) -> usize {
        let (u, i, d) = self.changes.counts();
        u + i + d
    }

    pub fn summary(&self) -> String {
        let (u, i, d) = self.changes.counts();
        let mut parts = Vec::new();
        let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
        if i > 0 {
            parts.push(plural(i, "insert"));
        }
        if u > 0 {
            parts.push(plural(u, "update"));
        }
        if d > 0 {
            parts.push(plural(d, "delete"));
        }
        parts.join(" · ")
    }

    pub fn decorations(&self, editable: &[bool]) -> Decorations {
        let mut d = Decorations::default();
        for (row, edits) in self.changes.edited_rows() {
            for (&col, e) in edits {
                d.cells.insert(
                    (row, col),
                    CellDecor {
                        tint: Some(Tone::Warn),
                        text: Some(e.new.as_deref().map(str::to_string)),
                        old: Some(e.old.as_deref().map(str::to_string)),
                        ghost: false,
                        color: None,
                    },
                );
            }
        }
        for row in self.changes.deleted_rows() {
            d.rows.insert(row, RowDecor { tint: Some(Tone::Err), strike: true, mark: Some(("−".into(), Tone::Err)), ..Default::default() });
        }
        for vals in self.changes.inserted() {
            let cells = (0..self.ncols)
                .map(|c| match vals.get(c).cloned().flatten() {
                    None if !editable.get(c).copied().unwrap_or(false) => CellDecor { text: Some(Some("—".into())), ghost: true, ..Default::default() },
                    None => CellDecor { text: Some(Some("default".into())), ghost: true, ..Default::default() },
                    Some(CellValue::Null) => CellDecor { text: Some(None), ..Default::default() },
                    Some(CellValue::Text(t)) => CellDecor { text: Some(Some(t)), ..Default::default() },
                })
                .collect();
            d.virtual_rows.push(VirtualRow {
                label: "+".into(),
                row: RowDecor { tint: Some(Tone::Ok), mark: Some(("+".into(), Tone::Ok)), ..Default::default() },
                cells,
            });
        }
        d
    }
}

pub fn find_display(rows: usize, source_of: impl Fn(usize) -> usize, source: usize, hint: usize) -> Option<usize> {
    if hint < rows && source_of(hint) == source {
        return Some(hint);
    }
    (0..rows).find(|&d| source_of(d) == source)
}

pub fn typed_value(text: &str, null: bool, was_default: bool) -> Option<CellValue> {
    if null {
        Some(CellValue::Null)
    } else if was_default && text.is_empty() {
        None
    } else {
        Some(CellValue::Text(text.to_string()))
    }
}

pub enum CellEditorEvent {
    Commit(Move),
    Cancel,
}

pub struct CellEditor {
    pub input: Entity<TextInput>,
    pub null: bool,
    pub was_default: bool,
    hidden_text: Option<String>,
}

impl EventEmitter<CellEditorEvent> for CellEditor {}

impl CellEditor {
    pub(crate) fn new(value: Option<CellValue>, accept: Option<fn(&str) -> bool>, cx: &mut Context<Self>) -> Self {
        let (text, null, was_default) = match &value {
            None => (String::new(), false, true),
            Some(CellValue::Null) => (String::new(), true, false),
            Some(CellValue::Text(t)) => (t.clone(), false, false),
        };
        let input = cx.new(|cx| {
            let mut i = TextInput::new(cx, if was_default { "default" } else { "" }).bare().accepting(accept);
            i.set_text(text, cx);
            i.select_all(cx);
            i
        });
        cx.subscribe(&input, |e: &mut CellEditor, _, ev: &InputEvent, cx| match ev {
            InputEvent::Submit => cx.emit(CellEditorEvent::Commit(Move::Down)),
            InputEvent::Changed => {
                if e.null {
                    e.null = false;
                    e.hidden_text = None;
                }
                e.was_default = false;
                cx.notify();
            }
        })
        .detach();
        CellEditor { input, null, was_default, hidden_text: None }
    }

    pub(crate) fn value(&self, cx: &App) -> Option<CellValue> {
        typed_value(self.input.read(cx).text(), self.null, self.was_default)
    }

    fn toggle_null(&mut self, cx: &mut Context<Self>) {
        self.null = !self.null;
        self.was_default = false;
        if self.null {
            let text = self.input.read(cx).text().to_string();
            self.hidden_text = Some(text);
            self.input.update(cx, |i, cx| i.set_text("", cx));
        } else if let Some(t) = self.hidden_text.take() {
            self.input.update(cx, |i, cx| i.set_text(t, cx));
        }
        cx.notify();
    }
}

impl Render for CellEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let null = self.null;
        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(|_, _: &CancelCell, _, cx| cx.emit(CellEditorEvent::Cancel)))
            .on_action(cx.listener(|_, _: &NextCell, _, cx| cx.emit(CellEditorEvent::Commit(Move::Next))))
            .on_action(cx.listener(|_, _: &PrevCell, _, cx| cx.emit(CellEditorEvent::Commit(Move::Prev))))
            .on_action(cx.listener(|_, _: &CellUp, _, cx| cx.emit(CellEditorEvent::Commit(Move::Up))))
            .on_action(cx.listener(|_, _: &CellDown, _, cx| cx.emit(CellEditorEvent::Commit(Move::Down))))
            .on_action(cx.listener(|e, _: &ToggleNull, _, cx| e.toggle_null(cx)))
            .on_action(cx.listener(|e, _: &TypeSpace, window, cx| e.input.update(cx, |i, cx| i.replace_text_in_range(None, " ", window, cx))))
            // A click (even a double or triple one, to select a word or everything) stays in
            // the field: it must not bubble to the grid underneath and reopen the editor.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .size_full()
            .pl(px(8.))
            .pr(px(3.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .bg(t.bg)
            .border_2()
            .border_color(t.acc)
            .text_size(px(12.))
            .text_color(t.tx)
            .child(
                // The field is always there (it keeps the focus); NULL is drawn over it.
                div()
                    .flex_1()
                    .min_w_0()
                    .relative()
                    .child(self.input.clone())
                    .when(null, |d| d.child(div().absolute().left_0().top_0().bottom_0().flex().items_center().italic().text_color(t.null).child("NULL"))),
            )
            .child(
                div()
                    .id("cell-null")
                    .flex_none()
                    .h(px(15.))
                    .px(px(4.))
                    .flex()
                    .items_center()
                    .text_size(px(9.5))
                    .when(null, |d| d.bg(t.acc).text_color(t.on_acc))
                    .when(!null, |d| d.bg(t.hov).text_color(t.tx3).border_1().border_color(t.line2))
                    .child("NULL")
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(|e, _, _, cx| e.toggle_null(cx))),
            )
    }
}

#[derive(Clone, Debug)]
enum TargetState {
    Unknown,
    Resolving,
    Ready(Box<EditTarget>),
    ReadOnly(String),
}

#[derive(Clone, Debug)]
enum Pending {
    Edit(usize, usize),
    Insert,
    Delete(Vec<usize>),
    Set(usize, usize, CellValue),
}

enum CellEdit {
    Text(Entity<CellEditor>),
    Value(Entity<ValueEditor>),
}

struct Editing {
    at: RowRef,
    row_hint: usize,
    col: usize,
    orig: CellValue,
    editor: CellEdit,
    at_point: (f32, f32),
    _blur: Subscription,
}

/// An enum column's type is its own name: these text types never are enums.
fn plain_text_type(name: &str) -> bool {
    let n = name.trim_end_matches("[]").to_ascii_lowercase();
    let base = n.split('(').next().unwrap_or("").trim();
    matches!(base, "text" | "varchar" | "character varying" | "bpchar" | "character" | "char" | "\"char\"" | "name" | "citext" | "unknown")
}

pub enum EditEvent {
    Changed,
    OpenReview,
    CloseReview,
    Committed { rows: usize },
}

#[derive(Clone, Debug)]
enum Card {
    Checking,
    ReadOnly { reason: String, pick_key: bool },
    PickKey,
    Note(String),
}

pub struct GridEdit {
    grid: Entity<ResultGrid>,
    conn: Option<Arc<Connection>>,
    db: String,
    rs: Option<Arc<ResultSet>>,
    target: TargetState,
    key_override: Option<Vec<String>>,
    pub model: EditModel,
    editing: Option<Editing>,
    pending: Option<Pending>,
    card: Option<Card>,
    pub detect_conflicts: bool,
    pub applying: bool,
    pub report: Option<ApplyReport>,
    pub review_open: bool,
    enums: std::collections::HashMap<usize, Arc<Vec<String>>>,
    _task: Option<Task<()>>,
    _insert_wait: Option<Task<()>>,
}

impl EventEmitter<EditEvent> for GridEdit {}

impl GridEdit {
    pub fn new(grid: Entity<ResultGrid>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.subscribe_in(&grid, window, |e, _, ev: &GridEvent, window, cx| match ev {
            GridEvent::EditCell { row, col } => e.request(Pending::Edit(*row, *col), window, cx),
            GridEvent::InsertRow => e.request(Pending::Insert, window, cx),
            GridEvent::DeleteRows(rows) => e.request(Pending::Delete(rows.clone()), window, cx),
            _ => {}
        })
        .detach();
        GridEdit {
            grid,
            conn: None,
            db: String::new(),
            rs: None,
            target: TargetState::Unknown,
            key_override: None,
            model: EditModel::default(),
            editing: None,
            pending: None,
            card: None,
            detect_conflicts: true,
            applying: false,
            report: None,
            review_open: false,
            enums: Default::default(),
            _task: None,
            _insert_wait: None,
        }
    }

    #[cfg(feature = "checks")]
    pub fn enum_labels(&self, col: usize) -> Option<&Arc<Vec<String>>> {
        self.enums.get(&col)
    }

    pub fn nullable(&self, col: usize) -> bool {
        match &self.target {
            TargetState::Ready(t) => t.nullable(col),
            _ => true,
        }
    }

    pub fn editable(&self, col: usize) -> Option<bool> {
        match &self.target {
            TargetState::Ready(t) => Some(t.is_editable(col)),
            TargetState::ReadOnly(_) => Some(false),
            _ => None,
        }
    }

    #[cfg(feature = "checks")]
    pub fn current_text(&self, row: usize, col: usize) -> Option<Option<String>> {
        let snap = self.snapshot()?;
        if row >= snap.rows() + self.model.changes.inserted().len() || col >= snap.columns.len() {
            return None;
        }
        let at = self.row_ref(&snap, row);
        let kind = snap.columns[col].kind;
        let v = self.model.current(at, col, || match at {
            RowRef::Stored(s) => CellValue::from_cell(kind, snap.source_cell(s, col)),
            RowRef::New(_) => CellValue::Null,
        });
        Some(v.and_then(|v| v.as_deref().map(str::to_string)))
    }

    pub fn set_cell(&mut self, row: usize, col: usize, v: CellValue, window: &mut Window, cx: &mut Context<Self>) {
        self.request(Pending::Set(row, col, v), window, cx);
    }

    pub fn reset(&mut self, conn: Option<Arc<Connection>>, db: String, rs: Option<Arc<ResultSet>>, cx: &mut Context<Self>) {
        let had = self.model.pending() > 0 || self.review_open;
        self.conn = conn;
        self.db = db;
        self.target = if rs.as_ref().is_some_and(|r| r.is_restored()) { TargetState::ReadOnly("a saved result: run it again to edit".into()) } else { TargetState::Unknown };
        self.rs = rs;
        self.key_override = None;
        self.model = EditModel::default();
        self.editing = None;
        self.pending = None;
        self.card = None;
        self.report = None;
        self.applying = false;
        self.enums.clear();
        self._task = None;
        if had {
            cx.emit(EditEvent::CloseReview);
            cx.emit(EditEvent::Changed);
        }
        cx.notify();
    }

    pub fn pending_count(&self) -> usize {
        self.model.pending()
    }

    pub fn summary(&self) -> String {
        self.model.summary()
    }

    pub fn target_label(&self) -> Option<String> {
        match &self.target {
            TargetState::Ready(t) => Some(t.table.name.clone()),
            _ => None,
        }
    }

    pub fn table_name(&self) -> String {
        match &self.target {
            TargetState::Ready(t) => t.table.name.clone(),
            _ => String::new(),
        }
    }

    fn snapshot(&self) -> Option<Snapshot> {
        self.rs.as_ref().map(|r| r.snapshot())
    }

    fn ncols(&self) -> usize {
        self.rs.as_ref().and_then(|r| r.columns()).map_or(0, |c| c.len())
    }

    fn editable_mask(&self) -> Vec<bool> {
        match &self.target {
            TargetState::Ready(t) => (0..self.ncols()).map(|c| t.is_editable(c)).collect(),
            _ => vec![],
        }
    }

    fn row_ref(&self, snap: &Snapshot, display: usize) -> RowRef {
        if display >= snap.rows() { RowRef::New(display - snap.rows()) } else { RowRef::Stored(snap.source_row(display)) }
    }

    fn display_of(snap: &Snapshot, at: RowRef, hint: usize) -> Option<usize> {
        match at {
            RowRef::New(i) => Some(snap.rows() + i),
            RowRef::Stored(s) => find_display(snap.rows(), |d| snap.source_row(d), s, hint),
        }
    }

    fn request(&mut self, p: Pending, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rs) = self.rs.clone() else { return };
        if self.applying {
            return;
        }
        if matches!(p, Pending::Insert) && matches!(rs.phase(), Phase::Running) && rs.columns().is_some_and(|c| !c.is_empty()) {
            let watched = rs.clone();
            self._insert_wait = Some(cx.spawn_in(window, async move |this, cx| {
                for _ in 0..200 {
                    cx.background_executor().timer(std::time::Duration::from_millis(50)).await;
                    if watched.phase() != Phase::Running {
                        break;
                    }
                }
                let _ = this.update_in(cx, |e, window, cx| {
                    if !e.rs.as_ref().is_some_and(|r| Arc::ptr_eq(r, &watched)) {
                        return;
                    }
                    if watched.phase() == Phase::Running {
                        e.card = Some(Card::Note("Still running".into()));
                        cx.notify();
                    } else {
                        e.request(Pending::Insert, window, cx);
                    }
                });
            }));
            return;
        }
        if matches!(rs.phase(), Phase::Running) || rs.columns().is_none_or(|c| c.is_empty()) {
            self.card = Some(Card::Note("Still running".into()));
            cx.notify();
            return;
        }
        match &self.target {
            TargetState::Ready(_) => self.run(p, window, cx),
            TargetState::ReadOnly(reason) => {
                self.card = Some(Card::ReadOnly { reason: reason.clone(), pick_key: self.can_pick_key(reason) });
                cx.notify();
            }
            TargetState::Resolving => self.pending = Some(p),
            TargetState::Unknown => {
                self.pending = Some(p);
                self.resolve(window, cx);
            }
        }
    }

    fn can_pick_key(&self, reason: &str) -> bool {
        reason.contains("choose the key") || reason.contains("primary key") || reason.contains("rows cannot be told apart")
    }

    fn resolve(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(conn), Some(rs)) = (self.conn.clone(), self.rs.clone()) else { return };
        self.target = TargetState::Resolving;
        self.card = Some(Card::Checking);
        cx.notify();
        let task = conn.edit_target(&self.db, &rs, self.key_override.clone());
        let candidates: Vec<usize> = rs
            .columns()
            .map(|c| c.iter().enumerate().filter(|(_, c)| c.kind == Kind::Text && !plain_text_type(&c.type_name)).map(|(i, _)| i).collect())
            .unwrap_or_default();
        let db = self.db.clone();
        self._task = Some(cx.spawn_in(window, async move |this, cx| {
            let r = task.await;
            let mut enums = std::collections::HashMap::new();
            if matches!(r, Ok(Editability::Editable(_))) {
                for col in candidates {
                    if let Ok(l) = conn.result_enum_labels(&db, &rs, col).await
                        && !l.is_empty()
                    {
                        enums.insert(col, l);
                    }
                }
            }
            let _ = this.update_in(cx, |e, window, cx| {
                e.enums = enums;
                e.target = match r {
                    Ok(Editability::Editable(t)) => TargetState::Ready(Box::new(t)),
                    Ok(Editability::ReadOnly(why)) => TargetState::ReadOnly(why),
                    Err(err) => TargetState::ReadOnly(format!("the table behind this result could not be read: {err:#}")),
                };
                e.card = None;
                match (&e.target, e.pending.take()) {
                    (TargetState::Ready(_), Some(p)) => e.run(p, window, cx),
                    (TargetState::ReadOnly(why), Some(_)) => {
                        let why = why.clone();
                        e.card = Some(Card::ReadOnly { pick_key: e.can_pick_key(&why), reason: why });
                    }
                    _ => {}
                }
                cx.emit(EditEvent::Changed);
                cx.notify();
            });
        }));
    }

    fn run(&mut self, p: Pending, window: &mut Window, cx: &mut Context<Self>) {
        self.card = None;
        if self.model.ncols != self.ncols() {
            self.model = EditModel::new(self.ncols());
        }
        match p {
            Pending::Edit(row, col) => self.begin_edit(row, col, window, cx),
            Pending::Insert => {
                self.commit_cell(Move::Stay, window, cx);
                let i = self.model.insert();
                self.refresh(cx);
                let Some(snap) = self.snapshot() else { return };
                let first = self.editable_mask().iter().position(|e| *e).unwrap_or(0);
                self.begin_edit(snap.rows() + i, first, window, cx);
            }
            Pending::Delete(rows) => {
                self.cancel_cell(window, cx);
                let Some(snap) = self.snapshot() else { return };
                let refs: Vec<RowRef> = rows.iter().map(|&r| self.row_ref(&snap, r)).collect();
                self.model.toggle_delete(&refs);
                self.refresh(cx);
            }
            Pending::Set(row, col, v) => {
                let TargetState::Ready(target) = &self.target else { return };
                let Some(snap) = self.snapshot() else { return };
                if col >= snap.columns.len() || row >= snap.rows() + self.model.changes.inserted().len() {
                    return;
                }
                if !target.is_editable(col) {
                    let what = if target.generated.get(col).copied().unwrap_or(false) { "is set by the server" } else { "is computed" };
                    self.card = Some(Card::Note(format!("{} {what}", snap.columns[col].name)));
                    cx.notify();
                    return;
                }
                let at = self.row_ref(&snap, row);
                if let RowRef::Stored(s) = at {
                    if self.model.changes.is_deleted(s) {
                        self.card = Some(Card::Note("Marked for deletion".into()));
                        cx.notify();
                        return;
                    }
                    if target.check == dataman_core::edit::ConflictCheck::Reread && target.key.iter().any(|k| k.result_col == col) {
                        self.card = Some(Card::Note(format!("{} is the key", snap.columns[col].name)));
                        cx.notify();
                        return;
                    }
                }
                let orig = match at {
                    RowRef::Stored(s) => CellValue::from_cell(snap.columns[col].kind, snap.source_cell(s, col)),
                    RowRef::New(_) => CellValue::Null,
                };
                self.model.set(at, col, orig, Some(v));
                self.refresh(cx);
            }
        }
    }

    fn begin_edit(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        let TargetState::Ready(target) = &self.target else { return };
        let Some(snap) = self.snapshot() else { return };
        let Some(cols) = self.rs.as_ref().and_then(|r| r.columns()) else { return };
        if col >= cols.len() || row >= snap.rows() + self.model.changes.inserted().len() {
            return;
        }
        if !target.is_editable(col) {
            self.card = Some(Card::Note(format!("{} is computed", cols[col].name)));
            cx.notify();
            return;
        }
        let at = self.row_ref(&snap, row);
        if let RowRef::Stored(s) = at {
            if self.model.changes.is_deleted(s) {
                self.card = Some(Card::Note("Marked for deletion".into()));
                cx.notify();
                return;
            }
            if target.check == dataman_core::edit::ConflictCheck::Reread && target.key.iter().any(|k| k.result_col == col) {
                self.card = Some(Card::Note(format!("{} is the key", cols[col].name)));
                cx.notify();
                return;
            }
        }
        self.commit_cell(Move::Stay, window, cx);
        let kind = cols[col].kind;
        let orig = match at {
            RowRef::Stored(s) => CellValue::from_cell(kind, snap.source_cell(s, col)),
            RowRef::New(_) => CellValue::Null,
        };
        let current = self.model.current(at, col, || orig.clone());
        let text = current.as_ref().and_then(|v| v.as_deref().map(str::to_string));
        if let Some(mode) = crate::value_edit::mode_for(kind, self.enums.get(&col), text.as_deref()) {
            let nullable = self.nullable(col);
            let editor = cx.new(|cx| ValueEditor::new(mode, text.as_deref(), nullable, window, cx));
            cx.subscribe_in(&editor, window, |e, _, ev: &ValueEvent, window, cx| {
                match ev {
                    ValueEvent::Commit => e.commit_cell(Move::Stay, window, cx),
                    ValueEvent::Cancel => e.cancel_cell(window, cx),
                }
                let f = e.grid.read(cx).focus_handle(cx);
                window.focus(&f, cx);
            })
            .detach();
            let focus = editor.focus_handle(cx);
            let blur = cx.on_focus_out(&focus, window, |e, _, window, cx| {
                if e.editing.is_some() {
                    e.commit_cell(Move::Stay, window, cx);
                }
            });
            self.grid.update(cx, |g, cx| g.select_cell(row, col, cx));
            let b = self.grid.read(cx).cell_bounds(row, col).unwrap_or_default();
            self.editing = Some(Editing { at, row_hint: row, col, orig, editor: CellEdit::Value(editor), at_point: (b.0, b.1 + b.3 + 2.), _blur: blur });
            cx.notify();
            return;
        }
        let accept = typing_filter(kind, &cols[col].type_name);
        let editor = cx.new(|cx| CellEditor::new(current, accept, cx));
        cx.subscribe_in(&editor, window, |e, _, ev: &CellEditorEvent, window, cx| match ev {
            CellEditorEvent::Commit(m) => e.commit_cell(*m, window, cx),
            CellEditorEvent::Cancel => e.cancel_cell(window, cx),
        })
        .detach();
        let focus = editor.read(cx).input.focus_handle(cx);
        let blur = cx.on_blur(&focus, window, |e, window, cx| {
            if e.editing.is_some() {
                e.commit_cell(Move::Stay, window, cx);
            }
        });
        let view = editor.clone().into();
        let orow = match at {
            RowRef::New(i) => OverlayRow::Virtual(i),
            RowRef::Stored(_) => OverlayRow::Data(row),
        };
        self.grid.update(cx, |g, cx| {
            g.select_cell(row, col, cx);
            g.set_overlay(Some(CellOverlay { row: orow, col, min_w: 160., view }), cx);
        });
        window.focus(&focus, cx);
        self.editing = Some(Editing { at, row_hint: row, col, orig, editor: CellEdit::Text(editor), at_point: (0., 0.), _blur: blur });
        cx.notify();
    }

    pub fn commit_cell(&mut self, m: Move, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ed) = self.editing.take() else { return };
        let v = match &ed.editor {
            CellEdit::Text(e) => Some(e.read(cx).value(cx)),
            CellEdit::Value(e) => e.read(cx).value(cx).ok().map(Some),
        };
        if let Some(v) = v {
            self.model.set(ed.at, ed.col, ed.orig.clone(), v);
        }
        self.grid.update(cx, |g, cx| g.set_overlay(None, cx));
        self.refresh(cx);
        let snap = self.snapshot();
        let rows = snap.as_ref().map_or(0, |s| s.rows()) + self.model.changes.inserted().len();
        let Some(cur) = snap.as_ref().and_then(|s| Self::display_of(s, ed.at, ed.row_hint)) else {
            let f = self.grid.read(cx).focus_handle(cx);
            window.focus(&f, cx);
            return;
        };
        let (r, c) = step(cur, ed.col, rows, self.ncols(), m);
        match m {
            Move::Next | Move::Prev if (r, c) != (cur, ed.col) => {
                let mask = self.editable_mask();
                let (mut r, mut c) = (r, c);
                for _ in 0..mask.len() {
                    if mask.get(c).copied().unwrap_or(false) {
                        break;
                    }
                    (r, c) = step(r, c, rows, self.ncols(), m);
                }
                self.begin_edit(r, c, window, cx);
            }
            _ => {
                self.grid.update(cx, |g, cx| g.select_cell(r, c, cx));
                let f = self.grid.read(cx).focus_handle(cx);
                if m != Move::Stay || window.focused(cx).is_none() {
                    window.focus(&f, cx);
                }
            }
        }
    }

    pub fn cancel_cell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            self.grid.update(cx, |g, cx| g.set_overlay(None, cx));
            let f = self.grid.read(cx).focus_handle(cx);
            window.focus(&f, cx);
            cx.notify();
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let d = self.model.decorations(&self.editable_mask());
        let d = (!d.is_empty()).then(|| Arc::new(d));
        self.grid.update(cx, |g, cx| g.set_decorations(d, cx));
        if self.model.pending() == 0 && self.report.is_none() && self.review_open {
            cx.emit(EditEvent::CloseReview);
        }
        cx.emit(EditEvent::Changed);
        cx.notify();
    }

    pub fn discard_all(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            self.grid.update(cx, |g, cx| g.set_overlay(None, cx));
        }
        self.model.changes.clear();
        self.report = None;
        self.refresh(cx);
        cx.emit(EditEvent::CloseReview);
    }

    pub fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_cell(window, cx);
        self.model.changes.clear();
        self.report = None;
        self.refresh(cx);
        cx.emit(EditEvent::CloseReview);
    }

    pub fn undo(&mut self, at: RowRef, cx: &mut Context<Self>) {
        self.model.undo_row(at);
        self.report = None;
        self.refresh(cx);
    }

    fn plan_target(&self) -> Option<EditTarget> {
        let TargetState::Ready(t) = &self.target else { return None };
        let mut t = (**t).clone();
        if !self.detect_conflicts {
            // `ChangeSet::plan` leaves out the columns it cannot compare (json and friends):
            // marking them all so gives an UPDATE with no guard and no re-read.
            t.type_names = vec!["json".into(); t.type_names.len()];
        }
        Some(t)
    }

    pub fn plan(&self) -> Result<Vec<PlannedChange>, String> {
        let t = self.plan_target().ok_or_else(|| "nothing to commit".to_string())?;
        let snap = self.snapshot().ok_or_else(|| "no result".to_string())?;
        self.model.changes.plan(&t, &snap)
    }

    pub fn review(&mut self, confirm: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_cell(Move::Stay, window, cx);
        if self.model.pending() == 0 {
            return;
        }
        if confirm || self.review_open {
            if self.review_open && !self.applying {
                self.apply(cx);
            } else {
                cx.emit(EditEvent::OpenReview);
            }
        } else {
            self.apply(cx);
        }
    }

    pub fn apply(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self.conn.clone() else { return };
        if self.applying {
            return;
        }
        let plan = match self.plan() {
            Ok(p) => p,
            Err(e) => {
                self.card = Some(Card::Note(e));
                cx.emit(EditEvent::OpenReview);
                cx.notify();
                return;
            }
        };
        self.applying = true;
        self.report = None;
        cx.notify();
        let rows = plan.len();
        let task = conn.apply_changes(Some(&self.db), plan);
        self._task = Some(cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |e, cx| {
                e.applying = false;
                match r {
                    Ok(rep) if rep.committed => {
                        e.model.changes.clear();
                        e.report = None;
                        e.refresh(cx);
                        cx.emit(EditEvent::CloseReview);
                        cx.emit(EditEvent::Committed { rows });
                    }
                    Ok(rep) => {
                        e.report = Some(rep);
                        cx.emit(EditEvent::OpenReview);
                    }
                    Err(err) => {
                        e.report = Some(ApplyReport { committed: false, outcomes: vec![], error: Some(format!("{err:#}")) });
                        cx.emit(EditEvent::OpenReview);
                    }
                }
                cx.notify();
            });
        }));
    }

    pub fn overwrite(&mut self, cx: &mut Context<Self>) {
        self.detect_conflicts = false;
        self.apply(cx);
    }

    pub fn change_lines(&self) -> Vec<(char, Tone, String, String, RowRef)> {
        let (Some(snap), TargetState::Ready(t)) = (self.snapshot(), &self.target) else { return vec![] };
        let cols = snap.columns.clone();
        let key_text = |row: usize| {
            t.key
                .iter()
                .map(|k| format!("{} {}", k.name, dataman_core::result::format_cell(cols[k.result_col].kind, snap.source_cell(row, k.result_col)).unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let short = |v: Option<&str>| match v {
            None => "NULL".to_string(),
            Some(s) if s.chars().count() > 24 => format!("'{}…'", s.chars().take(22).collect::<String>()),
            Some(s) => format!("'{s}'"),
        };
        let mut out = Vec::new();
        for (row, edits) in self.model.changes.edited_rows() {
            if self.model.changes.is_deleted(row) {
                continue;
            }
            let what = edits.iter().map(|(c, e)| format!("{}  {} → {}", cols[*c].name, short(e.old.as_deref()), short(e.new.as_deref()))).collect::<Vec<_>>().join(" · ");
            out.push(('~', Tone::Warn, key_text(row), what, RowRef::Stored(row)));
        }
        for row in self.model.changes.deleted_rows() {
            let name = (0..cols.len())
                .find(|&c| cols[c].kind == Kind::Text && !t.key.iter().any(|k| k.result_col == c))
                .and_then(|c| dataman_core::result::format_cell(Kind::Text, snap.source_cell(row, c)).map(|s| s.into_owned()));
            out.push(('−', Tone::Err, key_text(row), format!("delete row{}", name.map(|n| format!(" · {n}")).unwrap_or_default()), RowRef::Stored(row)));
        }
        for (i, vals) in self.model.changes.inserted().iter().enumerate() {
            let set: Vec<String> = vals.iter().flatten().map(|v| short(v.as_deref())).collect();
            let what = if set.is_empty() { "insert · all defaults".to_string() } else { format!("insert · {}", set.join(" ")) };
            out.push(('+', Tone::Ok, "new".into(), what, RowRef::New(i)));
        }
        out
    }

    fn pick_key(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.key_override = Some(vec![name]);
        self.card = None;
        self.resolve(window, cx);
    }
}

impl Render for GridEdit {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let popover = self.editing.as_ref().and_then(|ed| match &ed.editor {
            CellEdit::Value(v) => Some(
                gpui::deferred(
                    gpui::anchored().position(gpui::point(px(ed.at_point.0), px(ed.at_point.1))).snap_to_window_with_margin(px(8.)).child(v.clone()),
                )
                .priority(2),
            ),
            CellEdit::Text(_) => None,
        });
        let Some(card) = self.card.clone() else { return div().children(popover).into_any_element() };
        let t = cx.theme();
        let (tag, tag_tone, title, body): (&str, Tone, String, String) = match &card {
            Card::Checking => ("CHECKING", Tone::Muted, "Reading the table and key…".into(), String::new()),
            Card::ReadOnly { reason, .. } => ("READ-ONLY", Tone::Muted, capitalize(reason), String::new()),
            Card::PickKey => ("KEY", Tone::Lar, "Key column".into(), String::new()),
            Card::Note(n) => ("NOTE", Tone::Warn, n.clone(), String::new()),
        };
        let (fg, bg) = match tag_tone {
            Tone::Warn => (t.warn, t.warn_s),
            Tone::Lar => (t.lar_t, t.lar_s),
            _ => (t.tx2, t.hov),
        };
        let mut actions: Vec<gpui::AnyElement> = Vec::new();
        match &card {
            Card::ReadOnly { pick_key: true, .. } => {
                actions.push(
                    ui::button(&t, "ro-pick", Btn::Secondary)
                        .child("Pick key column…")
                        .on_click(cx.listener(|e, _, _, cx| {
                            e.card = Some(Card::PickKey);
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
            Card::PickKey => {
                let cols = self.rs.as_ref().and_then(|r| r.columns()).map(|c| c.iter().map(|c| c.name.clone()).collect::<Vec<_>>()).unwrap_or_default();
                for (i, name) in cols.into_iter().enumerate() {
                    let n2 = name.clone();
                    actions.push(
                        ui::button(&t, ("pk-col", i), Btn::Secondary)
                            .h(px(24.))
                            .child(name)
                            .on_click(cx.listener(move |e, _, window, cx| e.pick_key(n2.clone(), window, cx)))
                            .into_any_element(),
                    );
                }
            }
            _ => {}
        }
        if !matches!(card, Card::Checking) {
            actions.push(
                ui::button(&t, "card-close", Btn::Ghost)
                    .child("Dismiss")
                    .child(ui::key_hint("esc"))
                    .on_click(cx.listener(|e, _, _, cx| {
                        e.card = None;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        div()
            .absolute()
            .top(px(36.))
            .right(px(16.))
            .w(px(340.))
            .flex()
            .flex_col()
            .bg(t.pop)
            .shadow(t.ring())
            .overflow_hidden()
            .occlude()
            .child(
                div()
                    .h(px(32.))
                    .px(px(12.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .bg(t.bg)
                    .border_b_1()
                    .border_color(t.line)
                    .child(div().flex_1().text_size(px(10.5)).text_color(t.tx3).child("GRID EDIT"))
                    .child(div().h(px(18.)).px(px(6.)).flex().items_center().bg(bg).text_color(fg).text_size(px(10.)).child(tag)),
            )
            .child(
                div()
                    .p(px(14.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().text_color(t.tx).text_size(px(12.5)).line_height(px(17.)).child(title))
                    .when(!body.is_empty(), |d| d.child(div().text_color(t.tx2).text_size(px(11.5)).line_height(px(17.)).child(body)))
                    .when(!actions.is_empty(), |d| d.child(div().pt(px(4.)).flex().flex_row().flex_wrap().gap(px(6.)).children(actions))),
            )
            .children(popover)
            .into_any_element()
    }
}

impl GridEdit {
    pub fn dismiss_card(&mut self, cx: &mut Context<Self>) -> bool {
        if self.card.take().is_some() {
            cx.notify();
            return true;
        }
        false
    }

    #[cfg(feature = "checks")]
    pub fn demo_edit(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.request(Pending::Edit(row, col), window, cx);
    }

    /// The last commit's conflicts, when it left any.
    #[cfg(feature = "checks")]
    pub fn demo_conflicts(&self) -> usize {
        self.report.as_ref().map_or(0, |r| r.outcomes.iter().filter(|o| matches!(o.outcome, Outcome::Conflict { .. })).count())
    }

    #[cfg(feature = "checks")]
    pub fn demo_read_only(&self) -> Option<String> {
        match &self.card {
            Some(Card::ReadOnly { reason, .. }) => Some(reason.clone()),
            _ => None,
        }
    }

    #[cfg(feature = "checks")]
    pub fn demo_card(&self) -> Option<String> {
        match &self.card {
            Some(Card::Note(n)) => Some(n.clone()),
            _ => None,
        }
    }

    #[cfg(feature = "checks")]
    pub fn demo_value_editor(&self) -> Option<Entity<ValueEditor>> {
        match self.editing.as_ref().map(|e| &e.editor) {
            Some(CellEdit::Value(v)) => Some(v.clone()),
            _ => None,
        }
    }

    #[cfg(feature = "checks")]
    pub fn demo_cell_editor(&self) -> Option<Entity<CellEditor>> {
        match self.editing.as_ref().map(|e| &e.editor) {
            Some(CellEdit::Text(e)) => Some(e.clone()),
            _ => None,
        }
    }

    #[cfg(feature = "checks")]
    pub fn demo_start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.request(Pending::Edit(3, 4), window, cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_fill(&mut self, keep_editing: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(snap), TargetState::Ready(target)) = (self.snapshot(), &self.target) else { return };
        let own_id = target.check == dataman_core::edit::ConflictCheck::Reread;
        self.cancel_cell(window, cx);
        let cols = snap.columns.clone();
        let col = |n: &str| cols.iter().position(|c| c.name == n);
        let cell = |r: usize, c: usize| CellValue::from_cell(cols[c].kind, snap.source_cell(r, c));
        if let (Some(price), Some(name)) = (col("price"), col("name")) {
            self.model.set(RowRef::Stored(2), price, cell(2, price), Some(CellValue::text("79.00")));
            self.model.set(RowRef::Stored(4), name, cell(4, name), Some(CellValue::text("Echo dock (2026)")));
        }
        self.model.toggle_delete(&[RowRef::Stored(6)]);
        let i = self.model.insert();
        if own_id && let Some(c) = col("id") {
            self.model.set(RowRef::New(i), c, CellValue::Null, Some(CellValue::text("142")));
        }
        for (n, v) in [("sku", "AUD-0142"), ("name", "Nimbus earbuds"), ("category", "audio"), ("price", "159.00"), ("stock", "60"), ("active", "true")] {
            if let Some(c) = col(n) {
                self.model.set(RowRef::New(i), c, CellValue::Null, Some(CellValue::text(v)));
            }
        }
        self.refresh(cx);
        if keep_editing && let Some(price) = col("price") {
            self.begin_edit(3, price, window, cx);
        }
    }

}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

pub struct ReviewPanel {
    pub edit: Entity<GridEdit>,
    focus: FocusHandle,
}

impl Focusable for ReviewPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ReviewPanel {
    pub fn new(edit: Entity<GridEdit>, cx: &mut Context<Self>) -> Self {
        cx.observe(&edit, |_, _, cx| cx.notify()).detach();
        ReviewPanel { edit, focus: cx.focus_handle() }
    }
}

pub fn sql_line(t: &Theme, line: &str) -> gpui::Div {
    let mut row = div().flex().flex_row().flex_wrap();
    for tok in tokenize(line) {
        let text = &line[tok.start..tok.end];
        let color = match tok.kind {
            TokenKind::Keyword => t.kw,
            TokenKind::String | TokenKind::DollarString => t.str,
            TokenKind::Number | TokenKind::Param => t.num,
            TokenKind::LineComment | TokenKind::BlockComment => t.cm,
            TokenKind::Operator | TokenKind::Punct => t.op,
            _ => t.tx,
        };
        row = row.child(div().text_color(color).child(text.replace(' ', "\u{a0}")));
    }
    row
}

/// Only keyword tokens move (never text inside a literal), so the result is the same SQL.
pub fn layout_sql(sql: &str) -> String {
    let toks = tokenize(sql);
    let mut out = String::with_capacity(sql.len() + 64);
    for (i, t) in toks.iter().enumerate() {
        let text = &sql[t.start..t.end];
        let next_kw = toks.get(i + 1).filter(|n| n.kind == TokenKind::Keyword).map(|n| sql[n.start..n.end].to_ascii_uppercase());
        if t.kind == TokenKind::Whitespace && !text.contains('\n') {
            match next_kw.as_deref() {
                Some("WHERE") => out.push_str("\n "),
                Some("AND") => out.push_str("\n   "),
                Some("VALUES") => out.push('\n'),
                _ => out.push_str(text),
            }
            continue;
        }
        out.push_str(text);
    }
    out
}

const REVIEW_LINES: usize = 400;

impl Render for ReviewPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let edit = self.edit.clone();
        let e = edit.read(cx);
        let n = e.pending_count();
        let lines = e.change_lines();
        let plan = e.plan();
        let sql = match &plan {
            Ok(p) => format!("BEGIN;\n{}COMMIT;", layout_sql(&preview(p))),
            Err(err) => format!("-- {err}"),
        };
        let report = e.report.clone();
        let applying = e.applying;
        let detect = e.detect_conflicts;
        let table = e.table_name();
        let pill = match (&report, applying) {
            (_, true) => Some(("committing…", t.acc, t.acc_s)),
            (Some(r), _) if !r.committed => Some(("rolled back", t.err, t.err_s)),
            _ => None,
        };
        let header = div()
            .h(px(40.))
            .flex_none()
            .pl(px(14.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .border_b_1()
            .border_color(t.line)
            .child(ui::caps(&t, "REVIEW"))
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(format!("{n} change{} · {table}", if n == 1 { "" } else { "s" })))
            .children(pill.map(|(label, fg, bg)| div().h(px(18.)).px(px(6.)).flex().items_center().bg(bg).text_color(fg).text_size(px(10.5)).child(label)));
        let changes = div().px(px(14.)).py(px(12.)).flex().flex_col().gap(px(4.)).children(lines.into_iter().enumerate().map(|(i, (g, tone, key, what, at))| {
            let c = crate::grid::tone_strong(&t, tone);
            let edit = edit.clone();
            div()
                .h(px(26.))
                .px(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .bg(t.hov)
                .child(div().w(px(14.)).text_center().text_color(c).child(g.to_string()))
                .child(div().flex_none().text_color(t.tx3).child(key))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx2).child(what))
                .child(
                    div()
                        .id(("undo", i))
                        .text_size(px(11.))
                        .text_color(t.tx3)
                        .hover(|s| s.text_color(gpui::white().opacity(0.9)))
                        .child("undo")
                        .on_click(move |_, _, cx| edit.update(cx, |e, cx| e.undo(at, cx))),
                )
        }));
        let total_lines = sql.lines().count();
        let code = div()
            .mx(px(14.))
            .py(px(10.))
            .bg(t.card)
            .border_1()
            .border_color(t.line)
            .text_size(px(11.5))
            .line_height(px(19.))
            .flex()
            .flex_col()
            .children(sql.lines().take(REVIEW_LINES).enumerate().map(|(i, l)| {
                div()
                    .flex()
                    .flex_row()
                    .child(div().w(px(28.)).flex_none().pr(px(10.)).text_right().text_color(t.tx4).child((i + 1).to_string()))
                    .child(div().min_w_0().overflow_hidden().child(sql_line(&t, l)))
            }))
            .when(total_lines > REVIEW_LINES, |d| d.child(div().pl(px(38.)).text_color(t.tx3).child(format!("… {} more lines", total_lines - REVIEW_LINES))));
        let outcomes = report.as_ref().map(|r| {
            let conflicts = r.outcomes.iter().filter(|o| matches!(o.outcome, Outcome::Conflict { .. })).count();
            let title = if conflicts > 0 {
                format!("{conflicts} of {} change{} hit a conflict", r.outcomes.len(), if r.outcomes.len() == 1 { "" } else { "s" })
            } else {
                "Nothing was committed".to_string()
            };
            let changed = r.outcomes.iter().find_map(|o| match &o.outcome {
                Outcome::Conflict { kind: ConflictKind::Changed { .. }, .. } => Some(()),
                _ => None,
            });
            div()
                .mx(px(14.))
                .p(px(12.))
                .bg(if conflicts > 0 { t.warn_s } else { t.err_s })
                .border_1()
                .border_color(if conflicts > 0 { t.warn } else { t.err })
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(div().text_color(if conflicts > 0 { t.warn } else { t.err }).font_weight(FontWeight::SEMIBOLD).child(title))
                .children(r.error.clone().filter(|e| !r.outcomes.iter().any(|o| matches!(&o.outcome, Outcome::Conflict { message, .. } | Outcome::Failed { message, .. } if message == e))).map(|e| div().text_size(px(11.5)).line_height(px(17.)).text_color(t.tx).child(e)))
                .children(r.outcomes.iter().map(|o| {
                    let (g, c, what): (&str, Hsla, String) = match &o.outcome {
                        Outcome::Applied { rows } => ("✓", t.ok, format!("{rows} row")),
                        Outcome::RolledBack { rows } => ("↺", t.tx3, format!("rolled back ({rows} row)")),
                        Outcome::Conflict { message, .. } => ("⚠", t.warn, message.clone()),
                        Outcome::Failed { message, .. } => ("✕", t.err, message.clone()),
                        Outcome::NotRun => ("·", t.tx4, "not run".into()),
                    };
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.))
                        .text_size(px(11.))
                        .line_height(px(16.))
                        .child(div().w(px(12.)).flex_none().text_color(c).child(g))
                        .child(div().w(px(18.)).flex_none().text_color(t.tx3).child((o.index + 1).to_string()))
                        .child(div().flex_1().min_w_0().text_color(t.tx2).child(what))
                }))
                .when(changed.is_some(), |d| {
                    let e2 = edit.clone();
                    let e3 = edit.clone();
                    d.child(
                        div()
                            .pt(px(4.))
                            .flex()
                            .flex_row()
                            .gap(px(6.))
                            .child(ui::probe("cf-discard", ui::button(&t, "cf-discard", Btn::Ghost).child("Discard mine").on_click(move |_, window, cx| e2.update(cx, |e, cx| e.discard(window, cx)))))
                            .child(ui::probe(
                                "cf-over",
                                ui::button(&t, "cf-over", Btn::Ghost)
                                    .text_color(t.err)
                                    .child("Overwrite")
                                    .on_click(move |_, _, cx| e3.update(cx, |e, cx| e.overwrite(cx))),
                            )),
                    )
                })
        });
        let e_check = edit.clone();
        let sql_copy = sql.clone();
        let sql_open = sql.clone();
        let options = div()
            .p(px(14.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .id("detect-conflicts")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        e_check.update(cx, |e, cx| {
                            e.detect_conflicts = !e.detect_conflicts;
                            cx.notify();
                        })
                    })
                    .child(ui::checkbox(&t, detect))
                    .child(div().text_size(px(11.5)).text_color(t.tx).child("Detect conflicts")),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.))
                    .child(
                        ui::button(&t, "rv-copy", Btn::Ghost)
                            .h(px(24.))
                            .text_size(px(11.))
                            .child("Copy SQL")
                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(sql_copy.clone()))),
                    )
                    .child(
                        ui::button(&t, "rv-open", Btn::Ghost)
                            .h(px(24.))
                            .text_size(px(11.))
                            .child("Open in a new tab")
                            .on_click(cx.listener(move |_, _, window, cx| window.dispatch_action(Box::new(OpenSqlInTab(sql_open.clone())), cx))),
                    ),
            );
        let e_discard = edit.clone();
        let e_commit = edit.clone();
        let footer = div()
            .h(px(56.))
            .flex_none()
            .px(px(14.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .border_t_1()
            .border_color(t.line)
            .child(ui::probe(
                "rv-discard",
                ui::button(&t, "rv-discard", Btn::Ghost).h(px(28.)).text_color(t.err).child("Discard all").when_some(crate::keymap::key("discard_changes"), |d, k| d.child(ui::key_hint(k))).on_click(move |_, window, cx| {
                    e_discard.update(cx, |e, cx| e.discard(window, cx))
                }),
            ))
            .child(div().flex_1())
            .child(ui::probe(
                "rv-commit",
                ui::button(&t, "rv-commit", Btn::Primary)
                    .h(px(28.))
                    .px(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .when(applying || plan.is_err(), |d| d.opacity(0.6))
                    .child(if applying { "Committing…".to_string() } else { format!("Commit {n} change{}", if n == 1 { "" } else { "s" }) })
                    .when_some(crate::keymap::key("review_changes"), |d, k| d.child(div().text_size(px(10.5)).font_weight(FontWeight::NORMAL).opacity(0.75).child(k)))
                    .on_click(move |_, _, cx| e_commit.update(cx, |e, cx| e.apply(cx))),
            ));
        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .text_size(px(12.))
            .child(header)
            .child(
                div()
                    .id("review-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(changes)
                    .children(outcomes.map(|o| o.mb(px(12.))))
                    .child(div().px(px(14.)).pb(px(6.)).child(ui::caps(&t, "SQL")))
                    .child(code)
                    .child(options),
            )
            .child(footer)
    }
}

#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = review, no_json)]
pub struct OpenSqlInTab(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> CellValue {
        CellValue::text(s)
    }

    #[test]
    fn steps_wrap_and_hold() {
        assert_eq!(step(0, 0, 3, 3, Move::Next), (0, 1));
        assert_eq!(step(0, 2, 3, 3, Move::Next), (1, 0));
        assert_eq!(step(2, 2, 3, 3, Move::Next), (2, 2));
        assert_eq!(step(1, 0, 3, 3, Move::Prev), (0, 2));
        assert_eq!(step(0, 0, 3, 3, Move::Prev), (0, 0));
        assert_eq!(step(2, 1, 3, 3, Move::Down), (2, 1));
        assert_eq!(step(0, 1, 3, 3, Move::Up), (0, 1));
    }

    #[test]
    fn edits_count_once_per_row_and_revert() {
        let mut m = EditModel::new(3);
        m.set(RowRef::Stored(4), 1, t("89.00"), Some(t("79.00")));
        m.set(RowRef::Stored(4), 2, t("a"), Some(t("b")));
        assert_eq!(m.pending(), 1);
        assert_eq!(m.summary(), "1 update");
        assert_eq!(m.current(RowRef::Stored(4), 1, || t("89.00")), Some(t("79.00")));
        assert_eq!(m.current(RowRef::Stored(5), 1, || t("x")), Some(t("x")));
        m.set(RowRef::Stored(4), 1, t("89.00"), Some(t("89.00")));
        m.set(RowRef::Stored(4), 2, t("a"), Some(t("a")));
        assert_eq!(m.pending(), 0);
    }

    #[test]
    fn deletes_toggle_and_new_rows_go_away() {
        let mut m = EditModel::new(2);
        m.set(RowRef::Stored(1), 0, t("x"), Some(t("y")));
        m.toggle_delete(&[RowRef::Stored(1), RowRef::Stored(2)]);
        assert_eq!(m.changes.counts(), (0, 0, 2));
        m.toggle_delete(&[RowRef::Stored(1), RowRef::Stored(2)]);
        assert_eq!(m.changes.counts(), (0, 0, 0));
        m.toggle_delete(&[RowRef::Stored(1)]);
        m.toggle_delete(&[RowRef::Stored(1), RowRef::Stored(2)]);
        assert_eq!(m.changes.counts(), (0, 0, 2), "mixed selections mark them all");
        let i = m.insert();
        let j = m.insert();
        assert_eq!((i, j), (0, 1));
        m.toggle_delete(&[RowRef::New(0)]);
        assert_eq!(m.changes.inserted().len(), 1);
        assert_eq!(m.summary(), "1 insert · 2 deletes");
        m.undo_row(RowRef::Stored(1));
        assert_eq!(m.summary(), "1 insert · 1 delete");
    }

    #[test]
    fn decorations_follow_changes() {
        let mut m = EditModel::new(3);
        m.set(RowRef::Stored(7), 1, t("89.00"), Some(t("79.00")));
        m.toggle_delete(&[RowRef::Stored(9)]);
        let i = m.insert();
        m.set(RowRef::New(i), 1, CellValue::Null, Some(t("Nimbus")));
        m.set(RowRef::New(i), 2, CellValue::Null, Some(CellValue::Null));
        let d = m.decorations(&[false, true, true]);
        let c = &d.cells[&(7, 1)];
        assert_eq!(c.text, Some(Some("79.00".into())));
        assert_eq!(c.old, Some(Some("89.00".into())));
        assert_eq!(c.tint, Some(Tone::Warn), "a pending change is its tint, no underline");
        assert!(d.rows[&9].strike);
        assert_eq!(d.virtual_rows.len(), 1);
        let v = &d.virtual_rows[0];
        assert_eq!(v.label.as_ref(), "+");
        assert_eq!(v.cells[0].text, Some(Some("—".into())));
        assert!(v.cells[0].ghost);
        assert_eq!(v.cells[1].text, Some(Some("Nimbus".into())));
        assert_eq!(v.cells[2].text, Some(None));
        assert!(EditModel::new(3).decorations(&[]).is_empty());
    }

    #[test]
    fn review_layout_keeps_literals() {
        let sql = "UPDATE t SET a = 'x WHERE y AND z' WHERE id = 1 AND a IS NOT DISTINCT FROM 'b';\nINSERT INTO t (a) VALUES ('VALUES');\n";
        let out = layout_sql(sql);
        assert_eq!(out, "UPDATE t SET a = 'x WHERE y AND z'\n WHERE id = 1\n   AND a IS NOT DISTINCT FROM 'b';\nINSERT INTO t (a)\nVALUES ('VALUES');\n");
        let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(squash(&out), squash(sql));
    }

    #[test]
    fn rows_are_found_by_identity() {
        assert_eq!(find_display(10, |d| d, 4, 4), Some(4));
        assert_eq!(find_display(10, |d| 9 - d, 4, 4), Some(5));
        assert_eq!(find_display(3, |d| d, 7, 1), None);
        assert_eq!(crate::grid::OverlayRow::Virtual(0).display(1000), 1000);
        assert_eq!(crate::grid::OverlayRow::Virtual(0).display(11_000), 11_000);
        assert_eq!(crate::grid::follow_growth(1000, 1000, 11_000), 11_000, "the new row's selection moves with it");
        assert_eq!(crate::grid::follow_growth(999, 1000, 11_000), 999, "a real row stays");
    }

    #[test]
    fn number_fields_take_numbers() {
        let int = typing_filter(Kind::Int, "int4").unwrap();
        for ok in ["", "-", "+", "42", "-7", "0012"] {
            assert!(int(ok), "{ok}");
        }
        for bad in ["a", "4a", "1.5", "1e3", "--1", " 1", "1-"] {
            assert!(!int(bad), "{bad}");
        }
        let num = typing_filter(Kind::Numeric, "numeric").unwrap();
        for ok in ["", "-", ".", "1.", "-1.25", ".5", "1e", "1e-", "1.5E+10", "N", "nan", "-Inf", "Infinity"] {
            assert!(num(ok), "{ok}");
        }
        for bad in ["abc", "1.2.3", "1,5", "e5", "1e5.0", "1x", "NaNa", "- 1"] {
            assert!(!num(bad), "{bad}");
        }
        let uuid = typing_filter(Kind::Other, "uuid").unwrap();
        assert!(uuid("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"));
        assert!(!uuid("a0eebc99-zz"));
        assert!(typing_filter(Kind::Text, "text").is_none());
        assert!(typing_filter(Kind::Other, "interval").is_none());
    }

    #[test]
    fn typed_values() {
        assert_eq!(typed_value("", true, false), Some(CellValue::Null));
        assert_eq!(typed_value("", false, true), None, "an untouched new cell keeps its default");
        assert_eq!(typed_value("", false, false), Some(t("")), "empty text is not NULL");
        assert_eq!(typed_value("42", false, true), Some(t("42")));
    }
}
