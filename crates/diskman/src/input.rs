//! A single-line text field (forms, filters), after GPUI's `input.rs` example, with password
//! masking and a submit event.

use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine, SharedString, Style, TextRun,
    UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::*, relative, size,
};
use crate::editor::{word_at, word_left, word_right};
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
        self.from_display(l.closest_index_for_x(p.x - b.left()))
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
    }

    fn on_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let i = self.index_for(e.position);
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
        Some(Bounds::from_corners(
            point(b.left() + l.x_for_index(self.to_display(r.start)), b.top()),
            point(b.left() + l.x_for_index(self.to_display(r.end)), b.bottom()),
        ))
    }

    fn character_index_for_point(&mut self, p: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        let lp = self.last_bounds?.localize(&p)?;
        let l = self.last_layout.as_ref()?;
        let i = self.from_display(l.index_for_x(p.x - lp.x)?);
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
        let (selection, cursor) = if start == end {
            (None, Some(fill(Bounds::new(point(bounds.left() + line.x_for_index(cur), bounds.top() + px(1.)), size(px(1.5), bounds.size.height - px(2.))), theme.acc)))
        } else {
            (
                Some(fill(
                    Bounds::from_corners(point(bounds.left() + line.x_for_index(start), bounds.top()), point(bounds.left() + line.x_for_index(end), bounds.bottom())),
                    theme.sel,
                )),
                None,
            )
        };
        Prepaint { line: Some(line), cursor, selection }
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
        let line = pre.line.take().unwrap();
        // Text never runs out of the field (onto a neighbour); the cursor may sit on its edge.
        let mask = Bounds::new(bounds.origin, size(bounds.size.width + px(2.), bounds.size.height));
        window.with_content_mask(Some(gpui::ContentMask { bounds: mask }), |window| {
            if let Some(s) = pre.selection.take() {
                window.paint_quad(s);
            }
            let _ = line.paint(bounds.origin, window.line_height(), gpui::TextAlign::Left, None, window, cx);
            if focus.is_focused(window)
                && let Some(c) = pre.cursor.take()
            {
                window.paint_quad(c);
            }
        });
        self.input.update(cx, |i, _| {
            i.last_layout = Some(line);
            i.last_bounds = Some(bounds);
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
            .on_mouse_move(cx.listener(Self::on_move))
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

