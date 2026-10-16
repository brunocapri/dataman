//! A single-line text field (forms, filters), after GPUI's `input.rs` example, with password
//! masking and a submit event.
//!
//! Text wider than the field scrolls sideways as NSTextField does: the caret always shows
//! (typing, arrows, clicks, pastes, a drag past either edge scrolling on its own), a line that
//! shrinks leaves no empty room on the right, and a field without the focus shows its start.

use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, DispatchPhase, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine, SharedString, Style, TextRun,
    UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::*, relative, size,
};
use crate::editor::{floor_boundary, word_at, word_left, word_right};
use crate::theme::{Pt, px};

use crate::theme::ActiveTheme;

actions!(
    text_input,
    [
        Backspace, Delete, DeleteToStart, DeleteWordLeft, Left, Right, SelectLeft, SelectRight, SelectAll, Home, End,
        SelectHome, SelectEnd, MoveWordLeft, MoveWordRight, SelectWordLeft, SelectWordRight, Paste, Cut, Copy, Submit,
        Undo, Redo
    ]
);

pub fn bind_keys(cx: &mut App) {
    let c = Some("TextInput");
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("cmd-backspace", DeleteToStart, c),
        KeyBinding::new("alt-backspace", DeleteWordLeft, c),
        KeyBinding::new("left", Left, c),
        KeyBinding::new("right", Right, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("alt-left", MoveWordLeft, c),
        KeyBinding::new("alt-right", MoveWordRight, c),
        KeyBinding::new("alt-shift-left", SelectWordLeft, c),
        KeyBinding::new("alt-shift-right", SelectWordRight, c),
        KeyBinding::new("cmd-a", SelectAll, c),
        KeyBinding::new("cmd-v", Paste, c),
        KeyBinding::new("cmd-c", Copy, c),
        KeyBinding::new("cmd-x", Cut, c),
        KeyBinding::new("home", Home, c),
        KeyBinding::new("end", End, c),
        KeyBinding::new("cmd-left", Home, c),
        KeyBinding::new("cmd-right", End, c),
        KeyBinding::new("cmd-up", Home, c),
        KeyBinding::new("cmd-down", End, c),
        KeyBinding::new("shift-home", SelectHome, c),
        KeyBinding::new("shift-end", SelectEnd, c),
        KeyBinding::new("cmd-shift-left", SelectHome, c),
        KeyBinding::new("cmd-shift-right", SelectEnd, c),
        KeyBinding::new("cmd-shift-up", SelectHome, c),
        KeyBinding::new("cmd-shift-down", SelectEnd, c),
        KeyBinding::new("cmd-z", Undo, c),
        KeyBinding::new("cmd-shift-z", Redo, c),
        KeyBinding::new("enter", Submit, c),
    ]);
}

/// Typing (or deleting) pauses longer than this start a new undo step.
const COALESCE: Duration = Duration::from_millis(1000);

/// How often a drag resting past the field's edge scrolls on.
const AUTOSCROLL_TICK: Duration = Duration::from_millis(50);

/// The scroll that shows the caret: `prev` moved as little as it takes to bring the caret
/// (`caret` from the line's start) inside a field `width` wide, and never past the line's end
/// (`line` wide), so a line that shrank leaves no empty room on the right. A line that fits
/// does not scroll. All in the same unit.
fn scroll_for(prev: f32, caret: f32, line: f32, width: f32) -> f32 {
    if width <= 0. {
        return 0.;
    }
    let max = (line - width).max(0.);
    let s = prev.clamp(0., max);
    let s = if caret < s {
        caret
    } else if caret > s + width {
        caret - width
    } else {
        s
    };
    s.clamp(0., max)
}

/// Which side of `left..right` the pointer's `x` is out on: -1 left, 1 right (`None`: inside).
fn edge_dir(x: f32, left: f32, right: f32) -> Option<isize> {
    if x < left {
        Some(-1)
    } else if x > right {
        Some(1)
    } else {
        None
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EditKind {
    Type,
    Delete,
    Other,
}

#[derive(Clone, Debug)]
struct Edit {
    at: usize,
    old: String,
    new: String,
}

struct Group {
    edits: Vec<Edit>,
    before: Range<usize>,
    after: Range<usize>,
    kind: EditKind,
    time: Instant,
}

/// Undo/redo stacks; consecutive typing (or deleting) coalesces into one step (after `editor.rs`'s
/// `History`, adapted to a plain `String`).
#[derive(Default)]
struct History {
    undo: Vec<Group>,
    redo: Vec<Group>,
    sealed: bool,
}

impl History {
    fn seal(&mut self) {
        self.sealed = true;
    }

    fn push(&mut self, edits: Vec<Edit>, kind: EditKind, before: Range<usize>, after: Range<usize>, now: Instant) {
        self.redo.clear();
        if let [e] = edits.as_slice()
            && !self.sealed
            && kind != EditKind::Other
            && let Some(g) = self.undo.last_mut()
            && g.kind == kind
            && now.duration_since(g.time) < COALESCE
            && let Some(p) = g.edits.last()
            && match kind {
                EditKind::Type => p.at <= e.at && e.at <= p.at + p.new.len(),
                _ => e.at + e.old.len() == p.at || e.at == p.at,
            }
        {
            g.edits.extend(edits);
            g.after = after;
            g.time = now;
            return;
        }
        self.sealed = false;
        self.undo.push(Group { edits, before, after, kind, time: now });
        if self.undo.len() > 500 {
            self.undo.remove(0);
        }
    }

    fn undo(&mut self, content: &mut String) -> Option<Range<usize>> {
        let g = self.undo.pop()?;
        for e in g.edits.iter().rev() {
            content.replace_range(e.at..e.at + e.new.len(), &e.old);
        }
        let sel = g.before.clone();
        self.redo.push(g);
        self.sealed = true;
        Some(sel)
    }

    fn redo(&mut self, content: &mut String) -> Option<Range<usize>> {
        let g = self.redo.pop()?;
        for e in &g.edits {
            content.replace_range(e.at..e.at + e.old.len(), &e.new);
        }
        let sel = g.after.clone();
        self.undo.push(g);
        self.sealed = true;
        Some(sel)
    }
}

pub enum InputEvent {
    Changed,
    Submit,
}

pub struct TextInput {
    focus: FocusHandle,
    content: String,
    placeholder: SharedString,
    masked: bool,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    selecting: bool,
    /// What a drag extends by, set on the press that started it: a word (double click) or the
    /// whole line (triple click); `None` is plain, character by character.
    drag: Option<DragAnchor>,
    history: History,
    /// Draws its own field (30 pt, `field` background, ring); bare inputs live inside a
    /// container that styles them (the explorer filter).
    chrome: bool,
    /// Error ring (a field the server rejected).
    pub error: bool,
    /// How far the text is scrolled left, as last painted (`scroll_for` keeps the caret in
    /// the field).
    scroll: Pixels,
    /// The pointer during a drag-select, and whether the timer that scrolls while it rests
    /// past an edge is running.
    drag_at: Option<Point<Pixels>>,
    autoscrolling: bool,
}

#[derive(Clone, Debug)]
enum DragAnchor {
    Word(Range<usize>),
    Line,
}

impl EventEmitter<InputEvent> for TextInput {}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TextInput {
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>) -> Self {
        TextInput {
            focus: cx.focus_handle(),
            content: String::new(),
            placeholder: placeholder.into(),
            masked: false,
            selected: 0..0,
            reversed: false,
            marked: None,
            last_layout: None,
            last_bounds: None,
            selecting: false,
            drag: None,
            history: History::default(),
            chrome: true,
            error: false,
            scroll: Pixels::ZERO,
            drag_at: None,
            autoscrolling: false,
        }
    }

    /// Without its own field chrome.
    pub fn bare(mut self) -> Self {
        self.chrome = false;
        self
    }

    pub fn masked(mut self) -> Self {
        self.masked = true;
        self
    }

    pub fn set_placeholder(&mut self, p: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = p.into();
        cx.notify();
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    /// Demo and checks: the selection, byte offsets into `text()`.
    pub fn demo_selected(&self) -> Range<usize> {
        self.clamp(self.selected.clone())
    }

    /// Checks: how far the text is scrolled (points), as last painted.
    pub fn demo_scroll(&self) -> f32 {
        self.scroll.pt()
    }

    /// Checks: the caret's x from the field's left edge (points, after the scroll), as last
    /// painted.
    pub fn demo_caret_x(&self) -> Option<f32> {
        let l = self.last_layout.as_ref()?;
        Some((l.x_for_index(self.to_display(self.cursor())) - self.scroll).pt())
    }

    /// Demo and checks: the field's bounds in window coordinates, as last painted.
    pub fn demo_bounds(&self) -> Option<(f32, f32, f32, f32)> {
        let b = self.last_bounds?;
        Some((b.origin.x.pt(), b.origin.y.pt(), b.size.width.pt(), b.size.height.pt()))
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.content = text.into();
        self.selected = self.content.len()..self.content.len();
        self.marked = None;
        self.history = History::default();
        cx.notify();
    }

    /// Selects the whole text (typing replaces it).
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selected = 0..self.content.len();
        self.reversed = false;
        self.history.seal();
        cx.notify();
    }

    fn cursor(&self) -> usize {
        if self.reversed { self.selected.start } else { self.selected.end }
    }

    fn move_to(&mut self, o: usize, cx: &mut Context<Self>) {
        self.selected = o..o;
        self.reversed = false;
        self.history.seal();
        cx.notify();
    }

    fn select_to(&mut self, o: usize, cx: &mut Context<Self>) {
        if self.reversed {
            self.selected.start = o;
        } else {
            self.selected.end = o;
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
        self.history.seal();
        cx.notify();
    }

    /// Mutates `content` and records the undo step; does not touch `selected` or `marked`
    /// (callers set those afterward — the IME's marked-text replace needs its own mapping).
    fn raw_replace(&mut self, range: Range<usize>, new: &str, kind: EditKind, cx: &mut Context<Self>) {
        let before = self.clamp(self.selected.clone());
        let old = self.content[range.clone()].to_string();
        self.content.replace_range(range.clone(), new);
        self.history.push(vec![Edit { at: range.start, old, new: new.to_string() }], kind, before, range.start..range.start + new.len(), Instant::now());
        cx.emit(InputEvent::Changed);
    }

    /// Replaces `range` with `new` and puts the cursor after it.
    fn replace(&mut self, range: Range<usize>, new: &str, kind: EditKind, cx: &mut Context<Self>) {
        let at = range.start + new.len();
        self.raw_replace(range, new, kind, cx);
        self.selected = at..at;
        self.reversed = false;
        self.marked = None;
        cx.notify();
    }

    fn delete_range(&mut self, range: Range<usize>, kind: EditKind, cx: &mut Context<Self>) {
        if range.is_empty() {
            return;
        }
        self.replace(range, "", kind, cx);
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        let mut content = std::mem::take(&mut self.content);
        let sel = self.history.undo(&mut content);
        self.content = content;
        if let Some(sel) = sel {
            self.restore(sel, cx);
        }
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        let mut content = std::mem::take(&mut self.content);
        let sel = self.history.redo(&mut content);
        self.content = content;
        if let Some(sel) = sel {
            self.restore(sel, cx);
        }
    }

    fn restore(&mut self, sel: Range<usize>, cx: &mut Context<Self>) {
        let len = self.content.len();
        self.selected = sel.start.min(len)..sel.end.min(len);
        self.reversed = false;
        self.marked = None;
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn prev(&self, o: usize) -> usize {
        let o = self.clamp(o..o).start;
        self.content[..o].char_indices().next_back().map(|(i, _)| i).unwrap_or(0)
    }

    fn next(&self, o: usize) -> usize {
        let o = self.clamp(o..o).start;
        self.content[o..].chars().next().map(|c| o + c.len_utf8()).unwrap_or(self.content.len())
    }

    /// What is drawn: bullets for a password.
    fn display(&self) -> String {
        if self.masked { "•".repeat(self.content.chars().count()) } else { self.content.clone() }
    }

    /// Content offset → display offset (they differ when masked).
    fn to_display(&self, o: usize) -> usize {
        let o = self.clamp(o..o).start;
        if self.masked { self.content[..o].chars().count() * '•'.len_utf8() } else { o }
    }

    fn from_display(&self, o: usize) -> usize {
        if self.masked {
            let n = o / '•'.len_utf8();
            self.content.char_indices().nth(n).map(|(i, _)| i).unwrap_or(self.content.len())
        } else {
            o
        }
    }

    fn index_for(&self, p: Point<Pixels>) -> usize {
        let (Some(b), Some(l)) = (self.last_bounds.as_ref(), self.last_layout.as_ref()) else { return 0 };
        if p.y < b.top() {
            return 0;
        }
        if p.y > b.bottom() {
            return self.content.len();
        }
        // The layout may still be the previous text's (the content changed since the last
        // paint): never point past this text or inside a character.
        floor_boundary(&self.content, self.from_display(l.closest_index_for_x(p.x - b.left() + self.scroll)))
    }

    fn on_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        self.selecting = true;
        let i = self.index_for(e.position);
        match e.click_count {
            // A double click selects the word under it; a triple click, the whole field.
            2 => {
                let r = word_at(&self.content, i);
                self.drag = Some(DragAnchor::Word(r.clone()));
                self.selected = r;
                self.reversed = false;
                self.history.seal();
                cx.notify();
            }
            n if n >= 3 => {
                self.drag = Some(DragAnchor::Line);
                self.select_all(cx);
            }
            _ => {
                self.drag = None;
                if e.modifiers.shift { self.select_to(i, cx) } else { self.move_to(i, cx) }
            }
        }
    }

    fn on_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
        self.drag_at = None;
    }

    /// A move anywhere in the window while a drag-select is on (the field may be far behind).
    fn on_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        // Released where no up reached the field.
        if e.pressed_button != Some(MouseButton::Left) {
            self.selecting = false;
            self.drag_at = None;
            return;
        }
        self.drag_at = Some(e.position);
        let i = self.index_for(e.position);
        self.drag_to(i, cx);
        // Resting past an edge keeps scrolling (no more moves arrive): a timer steps on.
        if self.past_edge().is_some() && !self.autoscrolling {
            self.autoscrolling = true;
            cx.spawn(async move |t, cx| {
                loop {
                    cx.background_executor().timer(AUTOSCROLL_TICK).await;
                    if !t.update(cx, |t, cx| t.autoscroll(cx)).unwrap_or(false) {
                        break;
                    }
                }
            })
            .detach();
        }
    }

    /// Which edge the dragging pointer is past: -1 the left one, 1 the right one (`None`:
    /// inside, or no drag).
    fn past_edge(&self) -> Option<isize> {
        let (p, b) = (self.drag_at?, self.last_bounds?);
        edge_dir(p.x.pt(), b.left().pt(), b.right().pt())
    }

    /// One tick of a drag resting past an edge: the selection's end moves on toward the
    /// pointer (as far as the pointer is out, a character at least) and the scroll follows it.
    /// False once the drag ended or came back inside: the timer stops.
    fn autoscroll(&mut self, cx: &mut Context<Self>) -> bool {
        let (Some(dir), Some(p), true) = (self.past_edge(), self.drag_at, self.selecting) else {
            self.autoscrolling = false;
            return false;
        };
        let cur = self.cursor();
        let i = match self.index_for(p) {
            i if dir < 0 && i >= cur => self.prev(cur),
            i if dir > 0 && i <= cur => self.next(cur),
            i => i,
        };
        self.drag_to(i, cx);
        true
    }

    /// A drag reaching content offset `i`: character by character, or by words after a double
    /// click.
    fn drag_to(&mut self, i: usize, cx: &mut Context<Self>) {
        match self.drag.clone() {
            Some(DragAnchor::Word(orig)) => {
                let w = word_at(&self.content, i);
                let (a, h) = if w.start < orig.start { (orig.end, w.start) } else { (orig.start, w.end.max(orig.end)) };
                self.selected = a.min(h)..a.max(h);
                self.reversed = h < a;
                cx.notify();
            }
            // One line: dragging after a triple click keeps it all selected.
            Some(DragAnchor::Line) => {}
            None => self.select_to(i, cx),
        }
    }

    /// `r` made safe for `content`: within it, on character boundaries, start ≤ end. The IME
    /// (`insert_text` and friends) can hand back a range or a selection from before the text
    /// changed under it; a panic there aborts the whole app (the callback cannot unwind).
    fn clamp(&self, r: Range<usize>) -> Range<usize> {
        let snap = |mut i: usize| {
            i = i.min(self.content.len());
            while !self.content.is_char_boundary(i) {
                i -= 1;
            }
            i
        };
        let (a, b) = (snap(r.start), snap(r.end));
        a.min(b)..a.max(b)
    }

    /// Where a replacement goes: the given range, else the marked text, else the selection.
    fn target(&self, r16: Option<&Range<usize>>) -> Range<usize> {
        let r = r16.map(|r| self.range_from16(r)).or(self.marked.clone()).unwrap_or(self.selected.clone());
        self.clamp(r)
    }

    fn utf16_to_utf8(&self, o: usize) -> usize {
        let (mut u8o, mut u16o) = (0, 0);
        for c in self.content.chars() {
            if u16o >= o {
                break;
            }
            u16o += c.len_utf16();
            u8o += c.len_utf8();
        }
        u8o
    }

    fn utf8_to_utf16(&self, o: usize) -> usize {
        self.content[..o.min(self.content.len())].chars().map(char::len_utf16).sum()
    }

    fn range_from16(&self, r: &Range<usize>) -> Range<usize> {
        self.utf16_to_utf8(r.start)..self.utf16_to_utf8(r.end)
    }

    fn range_to16(&self, r: &Range<usize>) -> Range<usize> {
        self.utf8_to_utf16(r.start)..self.utf8_to_utf16(r.end)
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(&mut self, r16: Range<usize>, actual: &mut Option<Range<usize>>, _: &mut Window, _: &mut Context<Self>) -> Option<String> {
        let r = self.clamp(self.range_from16(&r16));
        actual.replace(self.range_to16(&r));
        Some(self.content[r].to_string())
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: self.range_to16(&self.clamp(self.selected.clone())), reversed: self.reversed })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| self.range_to16(&self.clamp(r.clone())))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(&mut self, r16: Option<Range<usize>>, new: &str, _: &mut Window, cx: &mut Context<Self>) {
        let r = self.target(r16.as_ref());
        let new = new.replace(['\n', '\r'], "");
        self.replace(r, &new, EditKind::Type, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        r16: Option<Range<usize>>,
        new: &str,
        sel16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let r = self.target(r16.as_ref());
        let new = &new.replace(['\n', '\r'], "");
        self.raw_replace(r.clone(), new, EditKind::Type, cx);
        self.marked = (!new.is_empty()).then(|| r.start..r.start + new.len());
        self.selected = sel16
            .as_ref()
            .map(|s| {
                let base = &self.content[r.start..];
                let conv = |o: usize| {
                    let (mut u8o, mut u16o) = (0, 0);
                    for c in base.chars() {
                        if u16o >= o {
                            break;
                        }
                        u16o += c.len_utf16();
                        u8o += c.len_utf8();
                    }
                    r.start + u8o
                };
                conv(s.start)..conv(s.end)
            })
            .unwrap_or(r.start + new.len()..r.start + new.len());
        self.selected = self.clamp(self.selected.clone());
        cx.notify();
    }

    fn bounds_for_range(&mut self, r16: Range<usize>, b: Bounds<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        let r = self.clamp(self.range_from16(&r16));
        let l = self.last_layout.as_ref()?;
        let x = b.left() - self.scroll;
        Some(Bounds::from_corners(
            point(x + l.x_for_index(self.to_display(r.start)), b.top()),
            point(x + l.x_for_index(self.to_display(r.end)), b.bottom()),
        ))
    }

    fn character_index_for_point(&mut self, p: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        let lp = self.last_bounds?.localize(&p)?;
        let l = self.last_layout.as_ref()?;
        let i = self.from_display(l.index_for_x(lp.x + self.scroll)?);
        Some(self.utf8_to_utf16(self.clamp(i..i).start))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct Prepaint {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    scroll: Pixels,
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&gpui::InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let theme = cx.theme();
        let input = self.input.read(cx);
        let style = window.text_style();
        let (text, color) = if input.content.is_empty() {
            (input.placeholder.to_string(), theme.tx3)
        } else {
            (input.display(), style.color)
        };
        let base = TextRun { len: text.len(), font: style.font(), color, background_color: None, underline: None, strikethrough: None };
        let marked = input.marked.clone().map(|m| input.clamp(m)).filter(|m| m.end <= text.len());
        let runs = match marked.as_ref().filter(|_| !input.content.is_empty() && !input.masked) {
            Some(m) => vec![
                TextRun { len: m.start, ..base.clone() },
                TextRun {
                    len: m.end - m.start,
                    underline: Some(UnderlineStyle { color: Some(base.color), thickness: px(1.), wavy: false }),
                    ..base.clone()
                },
                TextRun { len: text.len() - m.end, ..base },
            ]
            .into_iter()
            .filter(|r| r.len > 0)
            .collect(),
            None => vec![base],
        };
        let fs = style.font_size.to_pixels(window.rem_size());
        let mut line = window.text_system().shape_line(text.clone().into(), fs, &runs, None);
        // A placeholder longer than the field ends in "…" inside it.
        if input.content.is_empty() && line.width > bounds.size.width && bounds.size.width > px(0.) {
            let mut chars: Vec<char> = text.chars().collect();
            while !chars.is_empty() && line.width > bounds.size.width {
                chars.pop();
                let t: String = chars.iter().collect::<String>() + "…";
                let run = TextRun { len: t.len(), font: style.font(), color, background_color: None, underline: None, strikethrough: None };
                line = window.text_system().shape_line(t.into(), fs, &[run], None);
            }
        }
        let (start, end, cur) = if input.content.is_empty() {
            (0, 0, 0)
        } else {
            let sel = input.clamp(input.selected.clone());
            (input.to_display(sel.start), input.to_display(sel.end), input.to_display(input.cursor()))
        };
        // The caret in view while editing; the start when the field is left (or empty).
        let scroll = if input.content.is_empty() || !input.focus.is_focused(window) {
            Pixels::ZERO
        } else {
            gpui::px(scroll_for(input.scroll.as_f32(), line.x_for_index(cur).as_f32(), line.width.as_f32(), bounds.size.width.as_f32()))
        };
        let x0 = bounds.left() - scroll;
        let (selection, cursor) = if start == end {
            (None, Some(fill(Bounds::new(point(x0 + line.x_for_index(cur), bounds.top() + px(1.)), size(px(1.5), bounds.size.height - px(2.))), theme.acc)))
        } else {
            (
                Some(fill(Bounds::from_corners(point(x0 + line.x_for_index(start), bounds.top()), point(x0 + line.x_for_index(end), bounds.bottom())), theme.sel)),
                None,
            )
        };
        Prepaint { line: Some(line), cursor, selection, scroll }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        pre: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.input.clone()), cx);
        // A drag-select follows the pointer out of the field, as far as it goes.
        let input = self.input.clone();
        window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && input.read(cx).selecting {
                input.update(cx, |i, cx| i.on_move(e, window, cx));
            }
        });
        let line = pre.line.take().unwrap();
        // Text never runs out of the field (onto its padding or a neighbour), scrolled or not;
        // the cursor may sit on its right edge.
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            if let Some(s) = pre.selection.take() {
                window.paint_quad(s);
            }
            let _ = line.paint(point(bounds.left() - pre.scroll, bounds.top()), window.line_height(), gpui::TextAlign::Left, None, window, cx);
        });
        let mask = Bounds::new(bounds.origin, size(bounds.size.width + px(2.), bounds.size.height));
        window.with_content_mask(Some(gpui::ContentMask { bounds: mask }), |window| {
            if focus.is_focused(window)
                && let Some(c) = pre.cursor.take()
            {
                window.paint_quad(c);
            }
        });
        let scroll = pre.scroll;
        self.input.update(cx, |i, _| {
            i.last_layout = Some(line);
            i.last_bounds = Some(bounds);
            i.scroll = scroll;
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus.is_focused(window);
        let t = cx.theme();
        let chrome = self.chrome;
        let error = self.error;
        div()
            .flex()
            .key_context("TextInput")
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(|t, _: &Backspace, _, cx| {
                let r = if t.selected.is_empty() { t.prev(t.cursor())..t.cursor() } else { t.clamp(t.selected.clone()) };
                let kind = if t.selected.is_empty() { EditKind::Delete } else { EditKind::Other };
                t.delete_range(r, kind, cx);
            }))
            .on_action(cx.listener(|t, _: &Delete, _, cx| {
                let r = if t.selected.is_empty() { t.cursor()..t.next(t.cursor()) } else { t.clamp(t.selected.clone()) };
                let kind = if t.selected.is_empty() { EditKind::Delete } else { EditKind::Other };
                t.delete_range(r, kind, cx);
            }))
            .on_action(cx.listener(|t, _: &DeleteToStart, _, cx| {
                let c = t.cursor();
                t.delete_range(0..c, EditKind::Other, cx);
            }))
            .on_action(cx.listener(|t, _: &DeleteWordLeft, _, cx| {
                let r = if t.selected.is_empty() { word_left(&t.content, t.cursor())..t.cursor() } else { t.clamp(t.selected.clone()) };
                t.delete_range(r, EditKind::Other, cx);
            }))
            .on_action(cx.listener(|t, _: &Left, _, cx| {
                let o = if t.selected.is_empty() { t.prev(t.cursor()) } else { t.selected.start };
                t.move_to(o, cx);
            }))
            .on_action(cx.listener(|t, _: &Right, _, cx| {
                let o = if t.selected.is_empty() { t.next(t.cursor()) } else { t.selected.end };
                t.move_to(o, cx);
            }))
            .on_action(cx.listener(|t, _: &SelectLeft, _, cx| {
                let o = t.prev(t.cursor());
                t.select_to(o, cx);
            }))
            .on_action(cx.listener(|t, _: &SelectRight, _, cx| {
                let o = t.next(t.cursor());
                t.select_to(o, cx);
            }))
            .on_action(cx.listener(|t, _: &MoveWordLeft, _, cx| {
                let o = word_left(&t.content, t.cursor());
                t.move_to(o, cx);
            }))
            .on_action(cx.listener(|t, _: &MoveWordRight, _, cx| {
                let o = word_right(&t.content, t.cursor());
                t.move_to(o, cx);
            }))
            .on_action(cx.listener(|t, _: &SelectWordLeft, _, cx| {
                let o = word_left(&t.content, t.cursor());
                t.select_to(o, cx);
            }))
            .on_action(cx.listener(|t, _: &SelectWordRight, _, cx| {
                let o = word_right(&t.content, t.cursor());
                t.select_to(o, cx);
            }))
            .on_action(cx.listener(|t, _: &SelectAll, _, cx| t.select_all(cx)))
            .on_action(cx.listener(|t, _: &Home, _, cx| t.move_to(0, cx)))
            .on_action(cx.listener(|t, _: &End, _, cx| {
                let n = t.content.len();
                t.move_to(n, cx);
            }))
            .on_action(cx.listener(|t, _: &SelectHome, _, cx| t.select_to(0, cx)))
            .on_action(cx.listener(|t, _: &SelectEnd, _, cx| {
                let n = t.content.len();
                t.select_to(n, cx);
            }))
            .on_action(cx.listener(|t, _: &Undo, _, cx| t.undo(cx)))
            .on_action(cx.listener(|t, _: &Redo, _, cx| t.redo(cx)))
            // Edit ▸ Undo / Redo (the menu dispatches the editor's actions): the field's text.
            .on_action(cx.listener(|t, _: &crate::editor::Undo, _, cx| t.undo(cx)))
            .on_action(cx.listener(|t, _: &crate::editor::Redo, _, cx| t.redo(cx)))
            .on_action(cx.listener(|t, _: &Paste, _, cx| {
                if let Some(s) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    let r = t.clamp(t.selected.clone());
                    t.replace(r, &s, EditKind::Other, cx);
                }
            }))
            .on_action(cx.listener(|t, _: &Copy, _, cx| {
                if !t.selected.is_empty() && !t.masked {
                    cx.write_to_clipboard(ClipboardItem::new_string(t.content[t.selected.clone()].to_string()));
                }
            }))
            .on_action(cx.listener(|t, _: &Cut, _, cx| {
                if !t.selected.is_empty() && !t.masked {
                    let r = t.clamp(t.selected.clone());
                    cx.write_to_clipboard(ClipboardItem::new_string(t.content[r.clone()].to_string()));
                    t.delete_range(r, EditKind::Other, cx);
                }
            }))
            .on_action(cx.listener(|_, _: &Submit, _, cx| cx.emit(InputEvent::Submit)))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_up))
            .w_full()
            .line_height(px(18.))
            .when(chrome, |d| {
                d.h(px(30.))
                    .px(px(10.))
                    .items_center()
                    .bg(t.field)
                    .border_1()
                    .text_size(px(12.5))
                    .text_color(t.tx)
                    .border_color(if focused {
                        t.acc
                    } else if error {
                        t.err
                    } else {
                        t.field_b
                    })
                    .when(focused, |d| d.shadow(t.halo()))
            })
            .child(TextElement { input: cx.entity() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 100 pt field over a 300 pt line.
    const W: f32 = 100.;
    const LINE: f32 = 300.;

    #[test]
    fn a_line_that_fits_never_scrolls() {
        assert_eq!(scroll_for(0., 80., 90., W), 0.);
        // Whatever scroll it had (the text was longer a moment ago).
        assert_eq!(scroll_for(140., 90., 90., W), 0.);
        assert_eq!(scroll_for(0., 0., 0., W), 0.);
        // Not laid out yet.
        assert_eq!(scroll_for(50., 250., LINE, 0.), 0.);
    }

    #[test]
    fn the_caret_stays_in_view() {
        // Typing at the end: the caret sits on the right edge.
        assert_eq!(scroll_for(0., LINE, LINE, W), LINE - W);
        // Inside the view: nothing moves.
        assert_eq!(scroll_for(120., 150., LINE, W), 120.);
        assert_eq!(scroll_for(120., 120., LINE, W), 120., "on the left edge");
        assert_eq!(scroll_for(120., 220., LINE, W), 120., "on the right edge");
        // Left of it (←, Home, a click on the first visible character): just enough.
        assert_eq!(scroll_for(120., 100., LINE, W), 100.);
        assert_eq!(scroll_for(120., 0., LINE, W), 0., "Home shows the start");
        // Right of it (→, End, a paste): just enough.
        assert_eq!(scroll_for(0., 130., LINE, W), 30.);
        assert_eq!(scroll_for(10., LINE, LINE, W), LINE - W, "End shows the end");
    }

    #[test]
    fn a_shrinking_line_scrolls_back() {
        // Deleting at the end of a long line: never empty room on the right.
        assert_eq!(scroll_for(200., 250., 250., W), 150.);
        assert_eq!(scroll_for(200., 120., 150., W), 50.);
        // Cut down to what fits: back to the start.
        assert_eq!(scroll_for(200., 40., 40., W), 0.);
        // A scroll from before is never negative or past the end.
        assert_eq!(scroll_for(-20., 50., LINE, W), 0.);
        assert_eq!(scroll_for(900., 250., LINE, W), LINE - W);
    }

    #[test]
    fn clicks_map_through_the_scroll() {
        // A click 30 pt into a field (left edge at 40) scrolled by 120 is 150 pt into the line:
        // where `index_for` asks the layout; the caret put there needs no scroll.
        let (left, scroll, x) = (40., 120., 70.);
        let at = x - left + scroll;
        assert_eq!(at, 150.);
        assert_eq!(scroll_for(scroll, at, LINE, W), scroll);
    }

    #[test]
    fn drags_past_an_edge() {
        assert_eq!(edge_dir(10., 20., 120.), Some(-1));
        assert_eq!(edge_dir(130., 20., 120.), Some(1));
        assert_eq!(edge_dir(20., 20., 120.), None);
        assert_eq!(edge_dir(70., 20., 120.), None);
        assert_eq!(edge_dir(120., 20., 120.), None);
    }
}

