//! Multi-line SQL editor: syntax colors from the `dataman_core` lexer, a line-number gutter,
//! IME-aware text input, undo, statement execution and local autocomplete.
//!
//! Only the visible lines are shaped and painted, and the token list is re-lexed incrementally
//! around each edit, so a 50,000-line script edits and scrolls like a short one.

use std::ops::{Range, RangeInclusive};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, DispatchPhase, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font, GlobalElementId,
    Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render, ScrollWheelEvent, ShapedLine, SharedString, Style,
    Subscription, Task, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, actions, anchored, deferred, div,
    fill, font, point, prelude::*, relative, size,
};
use crate::theme::{Pt, px};
use gpui::FontWeight;
use std::collections::HashSet;
use dataman_core::complete::{Candidate, CandidateKind, Definition, SignatureHelp, complete, definition_at, signature_help};
use dataman_core::find::{Matcher, nearest_index, next_index, prev_index};
use dataman_core::format::{FormatOptions, KeywordCase};
use dataman_core::lint::{Lint, LintContext, Severity, lint};
use dataman_core::schema::SchemaSnapshot;
use dataman_core::sql::split::{split_tokens_in, statement_at_in};
use dataman_core::sql::{Dialect, Token, TokenKind, batches_in, split_in, tokenize, tokenize_in};
use unicode_segmentation::UnicodeSegmentation;

use crate::find::{self, Count, FindBar, FindBarEvent};
use crate::motion::{FindPop, PopStyle};
use crate::theme::{ActiveTheme, Theme};

actions!(
    code_editor,
    [
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        MoveWordLeft,
        MoveWordRight,
        SelectWordLeft,
        SelectWordRight,
        MoveLineStart,
        MoveLineEnd,
        SelectLineStart,
        SelectLineEnd,
        MoveDocStart,
        MoveDocEnd,
        SelectDocStart,
        SelectDocEnd,
        PageUp,
        PageDown,
        SelectPageUp,
        SelectPageDown,
        SelectAll,
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteToLineStart,
        Newline,
        Tab,
        Outdent,
        ToggleComment,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        RunScript,
        RunStatement,
        ShowCompletions,
        Cancel,
        ShowCharacterPalette,
        RunFromHere,
        FormatSql,
        GoToDefinition,
        PeekDefinition,
    ]
);

const CONTEXT: &str = "CodeEditor";
const INDENT: usize = 4;
const MENU_ROWS: usize = 12;
/// Typing pauses longer than this start a new undo step.
const COALESCE: Duration = Duration::from_millis(1000);
/// Lines around the cursor handed to autocomplete (it re-lexes what it gets).
const COMPLETION_LINES: usize = 300;
/// Space above the first line.
const TOP_PAD: f32 = 8.;
/// The selection's outer corners.
const SEL_RADIUS: f32 = 4.;
/// Width of the gutter (marks + line numbers) for up to 4-digit line numbers.
const GUTTER_MIN: f32 = 52.;
/// The run flash fades from `acc_s` to transparent in this long, once.
const FLASH: Duration = Duration::from_millis(180);
/// The cursor blinks for this long after the last key, then stays solid.
const BLINK_FOR: Duration = Duration::from_secs(6);
const BLINK_PERIOD: Duration = Duration::from_millis(530);
/// Matches kept (and highlighted) by the find bar; the count says `+` past it.
const FIND_CAP: usize = 10_000;
/// Typing in the find field pops the current match once it pauses this long.
const FIND_POP_DELAY: Duration = Duration::from_millis(150);
/// The lint runs once typing pauses this long, on a background thread.
const LINT_DELAY: Duration = Duration::from_millis(300);

/// Check stages: a command sent to the focused element the way its key would be, whatever key
/// it has now (Run Statement, Run).
pub(crate) fn dispatch(cx: &mut gpui::AsyncWindowContext, action: Box<dyn gpui::Action>) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        window.dispatch_action(action, cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

/// Registers the editor's key bindings (context "CodeEditor").
pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("left", MoveLeft, c),
        KeyBinding::new("right", MoveRight, c),
        KeyBinding::new("up", MoveUp, c),
        KeyBinding::new("down", MoveDown, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("shift-up", SelectUp, c),
        KeyBinding::new("shift-down", SelectDown, c),
        KeyBinding::new("alt-left", MoveWordLeft, c),
        KeyBinding::new("alt-right", MoveWordRight, c),
        KeyBinding::new("alt-shift-left", SelectWordLeft, c),
        KeyBinding::new("alt-shift-right", SelectWordRight, c),
        KeyBinding::new("cmd-left", MoveLineStart, c),
        KeyBinding::new("cmd-right", MoveLineEnd, c),
        KeyBinding::new("home", MoveLineStart, c),
        KeyBinding::new("end", MoveLineEnd, c),
        KeyBinding::new("cmd-shift-left", SelectLineStart, c),
        KeyBinding::new("cmd-shift-right", SelectLineEnd, c),
        KeyBinding::new("shift-home", SelectLineStart, c),
        KeyBinding::new("shift-end", SelectLineEnd, c),
        KeyBinding::new("cmd-up", MoveDocStart, c),
        KeyBinding::new("cmd-down", MoveDocEnd, c),
        KeyBinding::new("cmd-shift-up", SelectDocStart, c),
        KeyBinding::new("cmd-shift-down", SelectDocEnd, c),
        KeyBinding::new("pageup", PageUp, c),
        KeyBinding::new("pagedown", PageDown, c),
        KeyBinding::new("shift-pageup", SelectPageUp, c),
        KeyBinding::new("shift-pagedown", SelectPageDown, c),
        KeyBinding::new("cmd-a", SelectAll, c),
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("shift-backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("alt-backspace", DeleteWordLeft, c),
        KeyBinding::new("alt-delete", DeleteWordRight, c),
        KeyBinding::new("cmd-backspace", DeleteToLineStart, c),
        KeyBinding::new("enter", Newline, c),
        KeyBinding::new("shift-enter", Newline, c),
        KeyBinding::new("tab", Tab, c),
        KeyBinding::new("shift-tab", Outdent, c),
        KeyBinding::new("cmd-c", Copy, c),
        KeyBinding::new("cmd-x", Cut, c),
        KeyBinding::new("cmd-v", Paste, c),
        KeyBinding::new("cmd-z", Undo, c),
        KeyBinding::new("cmd-shift-z", Redo, c),
        // Run, comment, format and the definitions are remappable commands (`keymap`).
        KeyBinding::new("ctrl-space", ShowCompletions, c),
        KeyBinding::new("escape", Cancel, c),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, c),
        KeyBinding::new("cmd-f", find::Find, c),
        KeyBinding::new("cmd-alt-f", find::FindReplace, c),
        KeyBinding::new("cmd-g", find::FindNext, c),
        // Only while the bar is open: elsewhere ⇧⌘G is "focus the grid".
        KeyBinding::new("cmd-shift-g", find::FindPrevious, Some("CodeEditor && find")),
    ]);
}

/// A statement to run and where it starts in the buffer.
#[derive(Clone, Debug, PartialEq)]
pub struct Stmt {
    pub at: usize,
    pub sql: String,
    /// Values bound to its `$1 … $n` (empty: none asked for yet).
    pub params: Vec<Option<String>>,
    /// Runs inside a transaction that is rolled back (Explain Analyze).
    pub rollback: bool,
}

/// A server error shown in the editor: the failed statement's lines, the token it points
/// at, and the message beside that line. Cleared by the next edit.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub stmt: Range<usize>,
    pub at: Option<usize>,
    pub message: SharedString,
}

/// Gutter mark of a statement in a multi-statement run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Ok,
    Err,
    Running,
    Todo,
}

pub enum EditorEvent {
    /// Run Statement (⌘⇧↩): the statements of the selection, else the statement under the cursor.
    Run(Vec<Stmt>),
    /// Run (⌘↩): the selection's statements, else every statement of the buffer (or from the
    /// cursor's, ⌥⌘↩).
    RunAll(Vec<Stmt>),
    /// The buffer changed.
    Changed,
    /// F12 / ⌘-click (reveal in the explorer), ⌥F12 (`peek`: open its definition).
    Definition { def: Definition, peek: bool },
}

// ---------------------------------------------------------------------------------------------
// Text model (no UI).

/// Text with a line index and its token list, both kept up to date across edits.
pub(crate) struct Buffer {
    text: String,
    /// Byte offset where each line starts; `lines[0] == 0`.
    lines: Vec<usize>,
    tokens: Vec<Token>,
    /// What the text is lexed (and split) as: the connection's dialect.
    dialect: Dialect,
    /// Bytes outside ASCII; zero makes UTF-16 conversions the identity.
    non_ascii: usize,
    /// Length in bytes of the longest line (bounds horizontal scrolling).
    longest: usize,
}

fn count_non_ascii(s: &str) -> usize {
    s.bytes().filter(|b| !b.is_ascii()).count()
}

impl Buffer {
    pub(crate) fn new(text: &str) -> Self {
        Self::new_in(text, Dialect::Postgres)
    }

    pub(crate) fn new_in(text: &str, dialect: Dialect) -> Self {
        let mut lines = vec![0];
        lines.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        let mut b = Buffer {
            text: text.to_string(),
            lines,
            tokens: tokenize_in(text, dialect),
            dialect,
            non_ascii: count_non_ascii(text),
            longest: 0,
        };
        b.longest = b.compute_longest();
        b
    }

    /// Lexes the text again as `dialect`.
    pub(crate) fn set_dialect(&mut self, dialect: Dialect) {
        if self.dialect != dialect {
            self.dialect = dialect;
            self.tokens = tokenize_in(&self.text, dialect);
        }
    }

    /// The statements of the text (top-level `;`, the dialect's `DELIMITER`, T-SQL's `GO`).
    pub(crate) fn statements(&self) -> Vec<dataman_core::sql::Statement> {
        split_tokens_in(&self.text, &self.tokens, self.dialect.script())
    }

    /// What a script runs as, one unit at a time: its statements, T-SQL's `GO` batches.
    pub(crate) fn batches(&self) -> Vec<dataman_core::sql::Statement> {
        dataman_core::sql::batches_in(&self.text, self.dialect)
    }

    fn compute_longest(&self) -> usize {
        (0..self.line_count()).map(|l| self.line_end(l) - self.line_start(l)).max().unwrap_or(0)
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }
    pub(crate) fn len(&self) -> usize {
        self.text.len()
    }
    pub(crate) fn line_count(&self) -> usize {
        self.lines.len()
    }
    pub(crate) fn line_of(&self, offset: usize) -> usize {
        self.lines.partition_point(|&s| s <= offset) - 1
    }
    pub(crate) fn line_start(&self, line: usize) -> usize {
        self.lines[line]
    }
    /// End of the line, before its `\n`.
    pub(crate) fn line_end(&self, line: usize) -> usize {
        self.lines.get(line + 1).map_or(self.text.len(), |&s| s - 1)
    }
    pub(crate) fn line(&self, line: usize) -> &str {
        &self.text[self.line_start(line)..self.line_end(line)]
    }

    /// Replaces `range` with `new`, updating the line index and the tokens.
    pub(crate) fn edit(&mut self, range: Range<usize>, new: &str) {
        self.non_ascii = self.non_ascii + count_non_ascii(new) - count_non_ascii(&self.text[range.clone()]);
        let (l0, l1) = (self.line_of(range.start), self.line_of(range.end));
        let delta = new.len() as isize - range.len() as isize;
        self.text.replace_range(range.clone(), new);
        let starts: Vec<usize> = new.match_indices('\n').map(|(i, _)| range.start + i + 1).collect();
        let n = starts.len();
        self.lines.splice(l0 + 1..l1 + 1, starts);
        for s in &mut self.lines[l0 + 1 + n..] {
            *s = (*s as isize + delta) as usize;
        }
        self.relex(range.start, range.end, range.start + new.len());
        self.longest = self.compute_longest();
    }

    /// Re-lexes around an edit of `start..old_end` (now `start..new_end`), reusing the old tokens
    /// on both sides. The lexer never looks behind a token start, and no token's extent depends on
    /// text past the whitespace that follows it, so lexing restarts at the last whitespace token
    /// before the edit and stops once a fresh token starts where an old one did.
    fn relex(&mut self, start: usize, old_end: usize, new_end: usize) {
        let delta = new_end as isize - old_end as isize;
        let first = self.tokens.partition_point(|t| t.end <= start);
        let keep = self.tokens[..first].iter().rposition(|t| t.kind == TokenKind::Whitespace).unwrap_or(0);
        let r = self.tokens.get(keep).map_or(0, |t| t.start.min(start));
        let mut chunk = new_end - r + 4096;
        loop {
            let mut end = (r + chunk).min(self.text.len());
            while !self.text.is_char_boundary(end) {
                end -= 1;
            }
            let complete = end == self.text.len();
            let fresh = tokenize_in(&self.text[r..end], self.dialect);
            // Without the rest of the text only tokens up to a terminated whitespace are certain.
            let valid = if complete {
                fresh.len()
            } else {
                fresh.iter().rposition(|t| t.kind == TokenKind::Whitespace && t.end < end - r).map_or(0, |w| w + 1)
            };
            for (j, t) in fresh[..valid].iter().enumerate() {
                let s = r + t.start;
                if s < new_end {
                    continue;
                }
                let old = (s as isize - delta) as usize;
                if let Ok(k) = self.tokens.binary_search_by_key(&old, |t| t.start) {
                    self.splice(keep..k, &fresh[..j], r, delta);
                    return;
                }
            }
            if complete {
                let all = keep..self.tokens.len();
                self.splice(all, &fresh, r, delta);
                return;
            }
            chunk *= 2;
        }
    }

    fn splice(&mut self, old: Range<usize>, fresh: &[Token], base: usize, delta: isize) {
        let n = old.start + fresh.len();
        self.tokens.splice(old, fresh.iter().map(|t| Token { kind: t.kind, start: t.start + base, end: t.end + base }));
        for t in &mut self.tokens[n..] {
            t.start = (t.start as isize + delta) as usize;
            t.end = (t.end as isize + delta) as usize;
        }
    }

    pub(crate) fn to_utf16(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.text.len());
        if self.non_ascii == 0 {
            return offset;
        }
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        let s = &self.text[..offset];
        s.chars().count() + s.bytes().filter(|&b| b >= 0xf0).count()
    }

    pub(crate) fn from_utf16(&self, offset: usize) -> usize {
        if self.non_ascii == 0 {
            return offset.min(self.text.len());
        }
        let mut n = 0;
        for (i, c) in self.text.char_indices() {
            if n >= offset {
                return i;
            }
            n += c.len_utf16();
        }
        self.text.len()
    }

    pub(crate) fn prev_boundary(&self, offset: usize) -> usize {
        if offset == 0 {
            return 0;
        }
        let ls = self.line_start(self.line_of(offset));
        if offset == ls {
            return offset - 1;
        }
        self.text[ls..offset].grapheme_indices(true).next_back().map_or(ls, |(i, _)| ls + i)
    }

    pub(crate) fn next_boundary(&self, offset: usize) -> usize {
        if offset >= self.text.len() {
            return self.text.len();
        }
        let le = self.line_end(self.line_of(offset));
        if offset == le {
            return offset + 1;
        }
        self.text[offset..le].graphemes(true).next().map_or(le, |g| offset + g.len())
    }

    /// Index of the token covering `offset` (or the first one after it).
    fn token_at(&self, offset: usize) -> usize {
        self.tokens.partition_point(|t| t.end <= offset)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// Start of the word before `offset`, skipping whitespace first.
pub(crate) fn word_left(text: &str, offset: usize) -> usize {
    let mut pos = offset;
    let mut run = None;
    for (i, c) in text[..offset].char_indices().rev() {
        let k = class(c);
        match run {
            None if k == Class::Space => {}
            None => run = Some(k),
            Some(r) if r == k => {}
            Some(_) => break,
        }
        pos = i;
    }
    pos
}

/// End of the word after `offset`, skipping whitespace first.
pub(crate) fn word_right(text: &str, offset: usize) -> usize {
    let mut pos = offset;
    let mut run = None;
    for (i, c) in text[offset..].char_indices() {
        let k = class(c);
        match run {
            None if k == Class::Space => {}
            None => run = Some(k),
            Some(r) if r == k => {}
            Some(_) => break,
        }
        pos = offset + i + c.len_utf8();
    }
    pos
}

/// `offset` within `text`, on a character boundary (at or before it).
pub(crate) fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut i = offset.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The run of same-class characters around `offset` (double-click), never crossing a line.
pub(crate) fn word_at(text: &str, offset: usize) -> Range<usize> {
    let offset = floor_boundary(text, offset);
    let after = text[offset..].chars().next().filter(|&c| c != '\n');
    let before = text[..offset].chars().next_back().filter(|&c| c != '\n');
    let k = match (before, after) {
        (Some(b), Some(a)) if class(a) != Class::Word && class(b) == Class::Word => Class::Word,
        (_, Some(a)) => class(a),
        (Some(b), None) => class(b),
        (None, None) => return offset..offset,
    };
    let same = |c: char| c != '\n' && class(c) == k;
    let start = text[..offset].char_indices().rev().take_while(|&(_, c)| same(c)).last().map_or(offset, |(i, _)| i);
    let end = text[offset..]
        .char_indices()
        .take_while(|&(_, c)| same(c))
        .last()
        .map_or(offset, |(i, c)| offset + i + c.len_utf8());
    start..end
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

type Edits = Vec<(Range<usize>, String)>;

/// Inserts one indent level at the start of every non-empty line.
pub(crate) fn indent_edits(buf: &Buffer, lines: RangeInclusive<usize>) -> Edits {
    lines
        .filter(|&l| !buf.line(l).is_empty())
        .map(|l| {
            let s = buf.line_start(l);
            (s..s, " ".repeat(INDENT))
        })
        .collect()
}

/// Removes up to one indent level (spaces, or a tab) from every line.
pub(crate) fn outdent_edits(buf: &Buffer, lines: RangeInclusive<usize>) -> Edits {
    lines
        .filter_map(|l| {
            let s = buf.line_start(l);
            let line = buf.line(l);
            let n =
                if line.starts_with('\t') { 1 } else { line.bytes().take(INDENT).take_while(|&b| b == b' ').count() };
            (n > 0).then(|| (s..s + n, String::new()))
        })
        .collect()
}

/// Toggles `-- ` comments: removes them when every non-blank line has one, else adds them at
/// the smallest indentation.
pub(crate) fn comment_edits(buf: &Buffer, lines: RangeInclusive<usize>) -> Edits {
    let all: Vec<usize> = lines.clone().collect();
    let mut body: Vec<usize> = all.iter().copied().filter(|&l| !buf.line(l).trim().is_empty()).collect();
    if body.is_empty() {
        body = all;
    }
    let commented = body.iter().all(|&l| buf.line(l).trim_start().starts_with("--"));
    if commented {
        body.iter()
            .map(|&l| {
                let line = buf.line(l);
                let at = buf.line_start(l) + indent_of(line).len();
                let n = if line[at - buf.line_start(l)..].starts_with("-- ") { 3 } else { 2 };
                (at..at + n, String::new())
            })
            .collect()
    } else {
        let col = body.iter().map(|&l| indent_of(buf.line(l)).len()).min().unwrap_or(0);
        body.iter()
            .map(|&l| {
                let at = buf.line_start(l) + col.min(buf.line(l).len());
                (at..at, "-- ".to_string())
            })
            .collect()
    }
}

/// Maps a pre-edit offset through sorted, disjoint `edits`; `right` keeps it after text inserted
/// exactly at it.
pub(crate) fn map_offset(offset: usize, edits: &[(Range<usize>, String)], right: bool) -> usize {
    let mut delta = 0isize;
    for (r, s) in edits {
        let d = s.len() as isize - r.len() as isize;
        if r.end < offset || (r.end == offset && (!r.is_empty() || right)) {
            delta += d;
        } else if r.start < offset && offset < r.end {
            let base = r.start as isize + delta;
            return (base + if right { s.len() as isize } else { 0 }) as usize;
        } else {
            break;
        }
    }
    (offset as isize + delta) as usize
}

/// Applies sorted, disjoint `edits` to `buf` as one edit over their span (Replace All: one relex
/// and one reindex however many matches), returned for the undo history.
pub(crate) fn splice_edits(buf: &mut Buffer, edits: &[(Range<usize>, String)]) -> Option<Edit> {
    let span = edits.first()?.0.start..edits.last()?.0.end;
    let mut new = String::with_capacity(span.len());
    let mut pos = span.start;
    for (r, s) in edits {
        new.push_str(&buf.text[pos..r.start]);
        new.push_str(s);
        pos = r.end;
    }
    let old = buf.text[span.clone()].to_string();
    buf.edit(span.clone(), &new);
    Some(Edit { at: span.start, old, new })
}

/// The match the selection is on (or the first one after it): the find bar's `n` in `n/m`.
fn current_match(matches: &[Range<usize>], sel: &Range<usize>) -> Option<usize> {
    let i = matches.partition_point(|m| m.start < sel.start);
    if matches.get(i) == Some(sel) {
        return Some(i);
    }
    nearest_index(matches, |m| m.start, sel.start)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EditKind {
    Type,
    Delete,
    Other,
}

#[derive(Clone, Debug)]
pub(crate) struct Edit {
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

/// Undo/redo stacks; consecutive typing (or deleting) coalesces into one step.
#[derive(Default)]
pub(crate) struct History {
    undo: Vec<Group>,
    redo: Vec<Group>,
    sealed: bool,
}

impl History {
    /// Ends the current step: the next edit starts a new one.
    pub(crate) fn seal(&mut self) {
        self.sealed = true;
    }

    pub(crate) fn push(
        &mut self,
        edits: Vec<Edit>,
        kind: EditKind,
        before: Range<usize>,
        after: Range<usize>,
        now: Instant,
    ) {
        self.redo.clear();
        if let [e] = edits.as_slice()
            && !self.sealed
            && kind != EditKind::Other
            && !e.new.contains('\n')
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
        if self.undo.len() > 2000 {
            self.undo.remove(0);
        }
    }

    /// Reverts the last step; returns the selection to restore.
    pub(crate) fn undo(&mut self, buf: &mut Buffer) -> Option<Range<usize>> {
        let g = self.undo.pop()?;
        for e in g.edits.iter().rev() {
            buf.edit(e.at..e.at + e.new.len(), &e.old);
        }
        let sel = g.before.clone();
        self.redo.push(g);
        self.sealed = true;
        Some(sel)
    }

    pub(crate) fn redo(&mut self, buf: &mut Buffer) -> Option<Range<usize>> {
        let g = self.redo.pop()?;
        for e in &g.edits {
            buf.edit(e.at..e.at + e.old.len(), &e.new);
        }
        let sel = g.after.clone();
        self.undo.push(g);
        self.sealed = true;
        Some(sel)
    }
}

fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

// ---------------------------------------------------------------------------------------------
// The view.

struct Menu {
    replace: Range<usize>,
    items: Vec<Candidate>,
    selected: usize,
    top: usize,
}

enum Drag {
    Char,
    Word(Range<usize>),
    Line(Range<usize>),
    /// A scrollbar's thumb (vertical or not), grabbed this far from its start.
    Thumb { vertical: bool, grab: Pixels },
}

/// Hit width of a scrollbar along the editor's right or bottom edge, and its thumb's thickness
/// (the grid's: 6 pt, square, `line2`, over the text).
const BAR: f32 = 10.;
const THUMB: f32 = 6.;
const THUMB_MIN: f32 = 24.;

/// One scrollbar of the last frame (window coordinates): its track (where a press pages or
/// grabs) and its thumb.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Bar {
    track: Bounds<Pixels>,
    thumb: Bounds<Pixels>,
}

/// The thumb on a track of `track` pt for `view` pt of `view + range` pt scrolled `at` pt in:
/// its start and length.
fn thumb_span(track: f32, view: f32, range: f32, at: f32) -> (f32, f32) {
    let len = (track * view / (view + range)).max(THUMB_MIN).min(track);
    let start = if range > 0. { (at / range).clamp(0., 1.) * (track - len) } else { 0. };
    (start, len)
}

/// The find bar (`find.rs`) and what it found. Created on the first ⌘F.
struct EditorFind {
    bar: Entity<FindBar>,
    open: bool,
    /// The query compiled (rebuilt when it or a toggle changes, not on every edit).
    matcher: Option<Matcher>,
    invalid: bool,
    /// Every match (up to `FIND_CAP`), sorted and disjoint; recomputed on an edit or a new query.
    matches: Vec<Range<usize>>,
    capped: bool,
    cur: Option<usize>,
    /// Where typing in the field searches from: the selection when the bar opened.
    origin: usize,
    pop_task: Option<Task<()>>,
    _sub: Subscription,
}

/// Where the last frame put things (window coordinates).
#[derive(Clone, Copy)]
struct Geometry {
    bounds: Bounds<Pixels>,
    /// x of column 0 when not scrolled.
    origin_x: Pixels,
    /// Width of the line-number gutter (its divider is its last point).
    gutter_w: Pixels,
    text_width: Pixels,
    char_width: Pixels,
}

pub struct CodeEditor {
    focus: FocusHandle,
    buf: Buffer,
    sel: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    goal_x: Option<Pixels>,
    history: History,
    /// Scroll offset of the content (both components >= 0).
    scroll: Point<Pixels>,
    /// The UI zoom `scroll` was made at.
    zoom_at: f32,
    autoscroll: bool,
    schema: Arc<SchemaSnapshot>,
    menu: Option<Menu>,
    /// The erroneous token (wavy underline) of `diagnostic`.
    error: Option<Range<usize>>,
    diagnostic: Option<Diagnostic>,
    marks: Vec<(usize, Mark)>,
    flash: Option<(Range<usize>, Instant)>,
    /// The find indicator over a range (⌘' on the error), evaluated in `layout`.
    pop: Option<FindPop<Range<usize>>>,
    /// ⌘F: the find bar and its matches.
    find: Option<EditorFind>,
    last_run: Option<Range<usize>>,
    drag: Option<Drag>,
    geo: Option<Geometry>,
    /// The scrollbars of the last frame (vertical, horizontal), only while there is more to see.
    bars: (Option<Bar>, Option<Bar>),
    focused: bool,
    blink_on: bool,
    last_input: Instant,
    blink_task: Option<Task<()>>,
    /// Plain text (a value editor's long text, raw JSON): no SQL colors, no completion.
    plain: bool,
    /// The function call around the cursor and its parameter (the popover above the line).
    sig: Option<SignatureHelp>,
    /// Lint findings, as byte ranges of the buffer (sorted), from the last pause in typing.
    lints: Vec<Lint>,
    lint_task: Option<Task<()>>,
    /// The local lint's findings and the server's syntax errors (SQL Server); `lints` is both.
    local_lints: Vec<Lint>,
    server_lints: Vec<Lint>,
    /// Bumped on every buffer change: a lint of an older text is dropped.
    rev: u64,
    /// The lint under the mouse (its message shows under it).
    hover: Option<usize>,
    /// A LAR server: the lint does not report its catalog's gaps.
    lar: bool,
    /// Relations the tab's session created (temp tables), lower case.
    known: Arc<HashSet<String>>,
    /// A query tab's: the gutter as wide as its grid's row numbers (`grid::GutterShare`).
    gutter: Option<crate::grid::GutterShare>,
    _blur: Subscription,
    _focus: Subscription,
}

impl EventEmitter<EditorEvent> for CodeEditor {}

impl Focusable for CodeEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The code font size in points (Settings ▸ Editor: 12, 13 or 14); read on every layout.
static CODE_PT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(crate::theme::text::CODE as u32);

/// Changes the editor's font size (the caller refreshes the windows).
pub fn set_code_size(pt: u8) {
    CODE_PT.store(u32::from(pt.clamp(11, 16)), std::sync::atomic::Ordering::Relaxed);
}

/// The formatter's options (Settings ▸ Editor), packed: keyword case in the low bits, indent above.
static FORMAT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(2 << 4);

pub fn set_format_options(o: FormatOptions) {
    let case = match o.keyword_case {
        KeywordCase::Upper => 0,
        KeywordCase::Lower => 1,
        KeywordCase::Preserve => 2,
    };
    FORMAT.store(case | (o.indent as u32) << 4, std::sync::atomic::Ordering::Relaxed);
}

fn format_options() -> FormatOptions {
    let v = FORMAT.load(std::sync::atomic::Ordering::Relaxed);
    let keyword_case = match v & 0xf {
        1 => KeywordCase::Lower,
        2 => KeywordCase::Preserve,
        _ => KeywordCase::Upper,
    };
    FormatOptions { keyword_case, indent: if v >> 4 == 4 { 4 } else { 2 } }
}

fn line_height() -> Pixels {
    // 13 pt → 20 (the design's line), one more point of leading per point of size.
    px(crate::theme::metrics::EDITOR_LINE + 1.5 * (code_size().pt() - crate::theme::text::CODE)).round()
}

fn mono() -> Font {
    font(crate::theme::mono())
}

fn code_size() -> Pixels {
    px(CODE_PT.load(std::sync::atomic::Ordering::Relaxed) as f32)
}

/// The line as shaped: tabs and other control characters show as one space (same byte length).
fn display_text(s: &str) -> SharedString {
    if s.bytes().any(|b| b < 0x20) {
        s.chars().map(|c| if c.is_control() && c.is_ascii() { ' ' } else { c }).collect::<String>().into()
    } else {
        SharedString::from(s.to_string())
    }
}

/// Which corners of a selection run (left, right) are on the selection's outline and get
/// rounded — top-left, top-right, bottom-right, bottom-left — given the runs on the lines
/// above and below. A corner is inside the shape when the neighbouring run touches this one
/// and reaches at least as far that way.
fn outline_corners(above: Option<(f32, f32)>, run: (f32, f32), below: Option<(f32, f32)>) -> [bool; 4] {
    let (x0, x1) = run;
    let touch = |n: Option<(f32, f32)>| n.filter(|n| n.0 < x1 && n.1 > x0);
    let (above, below) = (touch(above), touch(below));
    [
        !above.is_some_and(|n| n.0 <= x0),
        !above.is_some_and(|n| n.1 >= x1),
        !below.is_some_and(|n| n.1 >= x1),
        !below.is_some_and(|n| n.0 <= x0),
    ]
}

/// Words colored as LAR syntax (`FOR SYSTEM_TIME AS OF`, `SUBSCRIBE`, `LISTEN … AS`).
const LAR_WORDS: &[&str] = &["SYSTEM_TIME", "SUBSCRIBE", "LISTEN", "LAR_HISTORY"];
/// Type names colored as types when they appear as identifiers.
const TYPE_WORDS: &[&str] = &[
    "bigint", "bool", "boolean", "bytea", "date", "float4", "float8", "int", "int2", "int4", "int8", "integer",
    "interval", "json", "jsonb", "numeric", "text", "timestamp", "timestamptz", "uuid", "varchar", "vector",
];

fn token_color(t: &Theme, kind: TokenKind, word: &str, next_is_paren: bool) -> Hsla {
    match kind {
        TokenKind::Keyword | TokenKind::Ident if LAR_WORDS.iter().any(|w| w.eq_ignore_ascii_case(word)) => t.lkw,
        TokenKind::Keyword | TokenKind::Ident if TYPE_WORDS.iter().any(|w| w.eq_ignore_ascii_case(word)) => t.typ,
        TokenKind::Ident if next_is_paren => t.func,
        TokenKind::Keyword => t.kw,
        TokenKind::String | TokenKind::DollarString => t.str,
        TokenKind::Number | TokenKind::Param => t.num,
        TokenKind::LineComment | TokenKind::BlockComment => t.cm,
        TokenKind::QuotedIdent | TokenKind::Ident => t.tx,
        TokenKind::Operator | TokenKind::Punct => t.op,
        TokenKind::Whitespace | TokenKind::Unknown => t.tx,
    }
}

/// SQL in the editor's colors, one (text, runs) per line, for read-only views (the activity
/// monitor's details). Tabs and control characters become spaces.
pub(crate) fn highlighted_lines(t: &Theme, sql: &str) -> Vec<(SharedString, Vec<TextRun>)> {
    let clean: String = sql.chars().map(|c| if c.is_ascii_control() && c != '\n' { ' ' } else { c }).collect();
    let toks = tokenize(&clean);
    let base = TextRun { len: 0, font: mono(), color: t.tx, background_color: None, underline: None, strikethrough: None };
    let mut lines = vec![(String::new(), Vec::<TextRun>::new())];
    for (i, tok) in toks.iter().enumerate() {
        let word = &clean[tok.start..tok.end];
        let paren = toks.get(i + 1..).and_then(|rest| rest.iter().find(|t| t.kind != TokenKind::Whitespace)).is_some_and(|t| &clean[t.start..t.end] == "(");
        let color = token_color(t, tok.kind, word, paren);
        for (k, piece) in word.split('\n').enumerate() {
            if k > 0 {
                lines.push((String::new(), Vec::new()));
            }
            if !piece.is_empty() {
                let line = lines.last_mut().unwrap();
                line.0.push_str(piece);
                line.1.push(TextRun { len: piece.len(), color, ..base.clone() });
            }
        }
    }
    lines.into_iter().map(|(s, r)| (SharedString::from(s), r)).collect()
}

/// Adds `underline` to the part of `runs` (covering text from `base`) that overlaps `range`.
fn underline_runs(runs: &mut Vec<TextRun>, base: usize, range: &Range<usize>, underline: UnderlineStyle) {
    decorate_runs(runs, base, range, |r| r.underline = Some(underline));
}

/// Applies `f` to the part of `runs` (covering text from `base`) that overlaps `range`,
/// splitting runs at the range's edges.
fn decorate_runs(runs: &mut Vec<TextRun>, base: usize, range: &Range<usize>, f: impl Fn(&mut TextRun)) {
    let mut out = Vec::with_capacity(runs.len() + 2);
    let mut pos = base;
    for run in runs.drain(..) {
        let (s, e) = (pos, pos + run.len);
        pos = e;
        let (a, b) = (range.start.max(s), range.end.min(e));
        if a >= b {
            out.push(run);
            continue;
        }
        for (x, y, u) in [(s, a, false), (a, b, true), (b, e, false)] {
            if y > x {
                let mut r = run.clone();
                r.len = y - x;
                if u {
                    f(&mut r);
                }
                out.push(r);
            }
        }
    }
    *runs = out;
}

impl CodeEditor {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let blur = cx.on_blur(&focus, window, |this, _, cx| {
            this.drag = None;
            this.focused = false;
            this.menu = None;
            this.sig = None;
            cx.notify();
        });
        let on_focus = cx.on_focus(&focus, window, |this, _, cx| {
            this.focused = true;
            this.reset_blink(cx);
        });
        CodeEditor {
            focus,
            buf: Buffer::new(""),
            sel: 0..0,
            reversed: false,
            marked: None,
            goal_x: None,
            history: History::default(),
            scroll: Point::default(),
            zoom_at: crate::theme::zoom(),
            autoscroll: false,
            schema: Arc::new(SchemaSnapshot::default()),
            menu: None,
            error: None,
            diagnostic: None,
            marks: Vec::new(),
            flash: None,
            pop: None,
            find: None,
            last_run: None,
            drag: None,
            geo: None,
            bars: (None, None),
            focused: false,
            blink_on: true,
            last_input: Instant::now(),
            blink_task: None,
            plain: false,
            sig: None,
            lints: Vec::new(),
            lint_task: None,
            local_lints: Vec::new(),
            server_lints: Vec::new(),
            rev: 0,
            hover: None,
            lar: false,
            known: Arc::default(),
            gutter: None,
            _blur: blur,
            _focus: on_focus,
        }
    }

    /// Plain text: no SQL colors, no completion (⌘↩ still emits `Run`).
    pub fn plain(mut self) -> Self {
        self.plain = true;
        self
    }

    /// Draws the line-number gutter as wide as the tab's grid's row numbers.
    pub fn share_gutter(&mut self, share: crate::grid::GutterShare) {
        self.gutter = Some(share);
    }

    pub fn text(&self) -> &str {
        self.buf.text()
    }

    /// Replaces the whole buffer; resets undo and puts the cursor at the end.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.buf = Buffer::new_in(&normalize_newlines(text), self.buf.dialect);
        let end = self.buf.len();
        self.sel = end..end;
        self.reversed = false;
        self.marked = None;
        self.goal_x = None;
        self.history = History::default();
        self.menu = None;
        self.error = None;
        self.diagnostic = None;
        self.marks.clear();
        self.flash = None;
        self.pop = None;
        self.last_run = None;
        self.autoscroll = true;
        self.clear_lints();
        self.hover = None;
        self.sig = None;
        self.rev += 1;
        self.schedule_lint(cx);
        self.refind(cx);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn set_schema(&mut self, schema: Arc<SchemaSnapshot>, cx: &mut Context<Self>) {
        // The connection's dialect: what the text is highlighted, split and checked as.
        if schema.dialect != self.buf.dialect {
            self.buf.set_dialect(schema.dialect);
            self.rev += 1;
        }
        self.schema = schema;
        if self.menu.is_some() {
            self.refresh_menu(true);
        }
        self.update_signature();
        // The schema rules see the new cache (debounced like typing).
        self.schedule_lint(cx);
        cx.notify();
    }

    /// The server is LAR (the lint skips what its catalog does not list).
    pub fn set_server(&mut self, lar: bool, cx: &mut Context<Self>) {
        if self.lar != lar {
            self.lar = lar;
            self.schedule_lint(cx);
        }
    }

    /// Relations the tab's session created (temp tables): never unknown to the lint.
    pub fn set_known(&mut self, known: Arc<HashSet<String>>, cx: &mut Context<Self>) {
        self.known = known;
        self.schedule_lint(cx);
    }

    /// The lint's findings (demo and checks).
    pub fn lints(&self) -> &[Lint] {
        &self.lints
    }

    /// The signature help shown (demo and checks).
    pub fn signature(&self) -> Option<&SignatureHelp> {
        self.sig.as_ref()
    }

    /// Shows lint `ix`'s message as if the mouse were on it (demo screenshots).
    pub fn demo_hover(&mut self, ix: Option<usize>, cx: &mut Context<Self>) {
        self.hover = ix.filter(|&i| i < self.lints.len());
        cx.notify();
    }

    /// Lints the buffer `LINT_DELAY` after the last change, on a background thread; a result
    /// for a text that changed since is dropped. Typing never waits for it.
    fn schedule_lint(&mut self, cx: &mut Context<Self>) {
        if self.plain {
            return;
        }
        let rev = self.rev;
        self.lint_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LINT_DELAY).await;
            let Ok(Some((text, schema, lar, known))) =
                this.update(cx, |e, _| (e.rev == rev).then(|| (e.buf.text.clone(), e.schema.clone(), e.lar, e.known.clone())))
            else {
                return;
            };
            let found = cx
                .background_executor()
                .spawn(async move { lint(&text, &LintContext { schema: Some(&schema), lar, known: Some(&known) }) })
                .await;
            let _ = this.update(cx, |e, cx| {
                if e.rev == rev && e.local_lints != found {
                    e.local_lints = found;
                    e.merge_lints();
                    cx.notify();
                }
            });
        }));
    }

    /// Bumped on every change of the buffer (an answer about an older text is dropped).
    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// The server's syntax errors for the text at `rev` (SQL Server's PARSEONLY); dropped when
    /// the text changed since.
    pub fn set_server_lints(&mut self, rev: u64, found: Vec<Lint>, cx: &mut Context<Self>) {
        if rev != self.rev || self.server_lints == found {
            return;
        }
        self.server_lints = found;
        self.merge_lints();
        cx.notify();
    }

    /// `lints` = the local findings and the server's, in order (a range the server reports
    /// is not repeated by a local finding with the same message).
    fn merge_lints(&mut self) {
        let mut all: Vec<Lint> = self.local_lints.iter().chain(&self.server_lints).cloned().collect();
        all.sort_by(|a, b| a.range.start.cmp(&b.range.start).then(a.range.end.cmp(&b.range.end)));
        all.dedup_by(|a, b| a.range == b.range && a.message == b.message);
        self.lints = all;
        self.hover = None;
    }

    fn clear_lints(&mut self) {
        self.lints = Vec::new();
        self.local_lints.clear();
        self.server_lints.clear();
    }

    /// Lints next to an edit of `range` (now `new_len` long) go; the ones after it move.
    fn shift_lints(&mut self, range: &Range<usize>, new_len: usize) {
        if self.lints.is_empty() {
            return;
        }
        let delta = new_len as isize - range.len() as isize;
        let shift = |v: &mut Vec<Lint>| {
            v.retain_mut(|l| {
                if l.range.end < range.start {
                    true
                } else if l.range.start > range.end {
                    l.range.start = (l.range.start as isize + delta) as usize;
                    l.range.end = (l.range.end as isize + delta) as usize;
                    true
                } else {
                    false
                }
            })
        };
        shift(&mut self.local_lints);
        shift(&mut self.server_lints);
        self.merge_lints();
    }

    /// The call around the cursor, from the buffer's own tokens (no re-lexing).
    fn update_signature(&mut self) {
        self.sig = if self.plain || !self.sel.is_empty() || self.marked.is_some() || self.schema.catalog.is_none() {
            None
        } else {
            signature_help(&self.buf.text, &self.buf.tokens, self.sel.end, &self.schema)
        };
    }

    /// ⌥⇧F: formats the selection, or the whole script, as one undo step. The cursor stays on
    /// the same character (counting all but whitespace); a selection keeps covering the text.
    pub fn format_sql(&mut self, cx: &mut Context<Self>) {
        if self.plain {
            return;
        }
        let range = if self.sel.is_empty() { 0..self.buf.len() } else { self.sel.clone() };
        let old = self.buf.text[range.clone()].to_string();
        let new = dataman_core::format::format_in(&old, &format_options(), self.buf.dialect);
        if new == old {
            return;
        }
        let before = self.sel.clone();
        let had_sel = !self.sel.is_empty();
        let caret = self.cursor().clamp(range.start, range.end);
        let solid = self.buf.text[range.start..caret].chars().filter(|c| !c.is_whitespace()).count();
        self.buf.edit(range.clone(), &new);
        if had_sel {
            self.set_sel(range.start, range.start + new.len());
        } else {
            let mut n = 0;
            let mut at = new.len();
            for (i, c) in new.char_indices() {
                if n == solid {
                    at = i;
                    break;
                }
                if !c.is_whitespace() {
                    n += 1;
                }
            }
            self.set_sel(range.start + at, range.start + at);
        }
        self.history.seal();
        self.history.push(vec![Edit { at: range.start, old, new }], EditKind::Other, before, self.sel.clone(), Instant::now());
        self.history.seal();
        self.marked = None;
        self.menu = None;
        self.clear_lints();
        self.after_edit(cx);
    }

    fn format_action(&mut self, _: &FormatSql, _: &mut Window, cx: &mut Context<Self>) {
        self.format_sql(cx);
    }

    /// What the name at `offset` refers to (a CTE's range in buffer offsets).
    fn definition_here(&self, offset: usize) -> Option<Definition> {
        if self.plain {
            return None;
        }
        let w = self.completion_window(offset);
        Some(match definition_at(&self.buf.text[w.clone()], offset - w.start, &self.schema)? {
            Definition::Local(r) => Definition::Local(r.start + w.start..r.end + w.start),
            d => d,
        })
    }

    /// F12 / ⌘-click (reveal in the explorer) and ⌥F12 (`peek`: its definition in a new tab);
    /// a CTE is selected in place.
    pub fn go_to_definition(&mut self, offset: usize, peek: bool, cx: &mut Context<Self>) {
        match self.definition_here(offset) {
            Some(Definition::Local(r)) => {
                self.set_sel(r.start, r.end);
                self.autoscroll = true;
                self.menu = None;
                self.find_pop(r, cx);
            }
            Some(def) => cx.emit(EditorEvent::Definition { def, peek }),
            None => {}
        }
    }

    fn go_to_definition_action(&mut self, _: &GoToDefinition, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to_definition(self.cursor(), false, cx);
    }

    fn peek_definition_action(&mut self, _: &PeekDefinition, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to_definition(self.cursor(), true, cx);
    }

    /// Typed input at the cursor, as if the user typed it (`\n` and `\t` act as Enter and Tab).
    pub fn type_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = normalize_newlines(text);
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let n = rest.find(['\n', '\t']).unwrap_or(rest.len());
            if n > 0 {
                let range = self.marked.clone().unwrap_or(self.sel.clone());
                self.insert_typed(range, &rest[..n], cx);
            }
            match rest[n..].chars().next() {
                Some('\n') => self.newline(&Newline, window, cx),
                Some('\t') => self.tab(&Tab, window, cx),
                _ => {}
            }
            rest = &rest[(n + 1).min(rest.len())..];
        }
    }

    /// Shows a server error (until the next edit); `None` clears. The token at the error's
    /// position gets a wavy underline.
    pub fn set_diagnostic(&mut self, d: Option<Diagnostic>, cx: &mut Context<Self>) {
        self.error = d.as_ref().and_then(|d| d.at).and_then(|o| {
            let toks = &self.buf.tokens;
            let i = self.buf.token_at(o.min(self.buf.len()));
            let j = (i.min(toks.len())..toks.len())
                .find(|&j| toks[j].kind != TokenKind::Whitespace)
                .or_else(|| (0..i.min(toks.len())).rev().find(|&j| toks[j].kind != TokenKind::Whitespace))?;
            // A qualified name (`s.delivered_at`) is underlined whole.
            let mut end = j;
            let is_name = |k: TokenKind| matches!(k, TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword);
            while is_name(toks[end].kind)
                && end + 2 < toks.len()
                && &self.buf.text[toks[end + 1].start..toks[end + 1].end] == "."
                && is_name(toks[end + 2].kind)
            {
                end += 2;
            }
            Some(toks[j].start..toks[end].end)
        });
        self.diagnostic = d.map(|mut d| {
            d.stmt.end = d.stmt.end.min(self.buf.len());
            d.stmt.start = d.stmt.start.min(d.stmt.end);
            d
        });
        cx.notify();
    }

    pub fn diagnostic(&self) -> Option<&Diagnostic> {
        self.diagnostic.as_ref()
    }

    /// Marks for the statements of a multi-statement run (by offset); cleared by the next edit.
    pub fn set_marks(&mut self, marks: Vec<(usize, Mark)>, cx: &mut Context<Self>) {
        self.marks = marks;
        cx.notify();
    }

    /// Puts the cursor on the error (⌘'), which pops in Xcode's find yellow (the underlined
    /// token, or the token where the failed statement starts); false when there is none.
    pub fn jump_to_error(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(d) = &self.diagnostic else { return false };
        let at = d.at.unwrap_or(d.stmt.start).min(self.buf.len());
        let target = self.error.clone().unwrap_or_else(|| {
            let toks = &self.buf.tokens;
            (self.buf.token_at(at)..toks.len())
                .find(|&i| toks[i].kind != TokenKind::Whitespace)
                .map_or(at..at, |i| toks[i].start..toks[i].end)
        });
        self.set_cursor(at, cx);
        self.find_pop(target, cx);
        true
    }

    /// Demo and live checks: the range a find pop is running on now.
    pub fn demo_pop(&self) -> Option<Range<usize>> {
        self.pop.as_ref().filter(|p| p.frame(Instant::now()).is_some()).map(|p| p.target.clone())
    }

    /// Pops the find indicator over `range` (Xcode's yellow box with the text on top). For any
    /// "found it" in the editor (⌘' today; a find bar later).
    pub fn find_pop(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let range = self.clamp_range(range);
        self.pop = FindPop::start(range, PopStyle::Find, cx);
        cx.notify();
    }

    /// Highlights `range` with the run flash: `acc_s` fading to transparent in 180 ms, once.
    pub fn flash(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        self.flash = Some((range, Instant::now()));
        cx.notify();
    }

    /// The cursor shows again and blinks for a while; then it stays solid (no idle redraws).
    fn reset_blink(&mut self, cx: &mut Context<Self>) {
        self.blink_on = true;
        self.last_input = Instant::now();
        if self.blink_task.is_some() || !self.focused {
            return;
        }
        self.blink_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK_PERIOD).await;
                let go_on = this
                    .update(cx, |e, cx| {
                        let done = !e.focused || e.last_input.elapsed() >= BLINK_FOR;
                        let on = if done { true } else { !e.blink_on };
                        if on != e.blink_on {
                            e.blink_on = on;
                            cx.notify();
                        }
                        if done {
                            e.blink_task = None;
                        }
                        !done
                    })
                    .unwrap_or(false);
                if !go_on {
                    break;
                }
            }
        }));
    }

    /// Puts the cursor at byte `offset` (clamped to a char boundary) and scrolls it into view.
    pub fn set_cursor(&mut self, offset: usize, cx: &mut Context<Self>) {
        let mut offset = offset.min(self.buf.len());
        while !self.buf.text.is_char_boundary(offset) {
            offset -= 1;
        }
        self.marked = None;
        self.move_to(offset, cx);
    }

    /// Byte range of what the last `Run`/`RunAll` executed.
    #[allow(dead_code)]
    pub fn last_run_range(&self) -> Option<Range<usize>> {
        self.last_run.clone()
    }

    /// Inserts text at the cursor (replacing the selection) as one undo step.
    pub fn insert_at_cursor(&mut self, text: &str, cx: &mut Context<Self>) {
        self.history.seal();
        self.marked = None;
        self.menu = None;
        self.replace(self.sel.clone(), &normalize_newlines(text), EditKind::Other, cx);
        self.history.seal();
    }

    /// The token the current error points at.
    pub fn error_range(&self) -> Option<Range<usize>> {
        self.error.clone()
    }

    /// Replaces the erroneous token (the error view's "Apply hint").
    pub fn replace_error_token(&mut self, with: &str, cx: &mut Context<Self>) {
        let Some(r) = self.error.clone() else { return };
        self.history.seal();
        self.replace(r, with, EditKind::Other, cx);
        self.history.seal();
    }

    pub fn completion_visible(&self) -> bool {
        self.menu.is_some()
    }

    /// Checks: where the cursor is (a byte offset), and what the text is lexed as.
    pub fn demo_cursor(&self) -> (usize, Dialect) {
        (self.cursor(), self.buf.dialect)
    }

    fn cursor(&self) -> usize {
        if self.reversed { self.sel.start } else { self.sel.end }
    }

    fn anchor(&self) -> usize {
        if self.reversed { self.sel.end } else { self.sel.start }
    }

    fn set_sel(&mut self, anchor: usize, head: usize) {
        self.reversed = head < anchor;
        self.sel = anchor.min(head)..anchor.max(head);
    }

    fn after_move(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        self.history.seal();
        self.update_signature();
        self.autoscroll = true;
        self.reset_blink(cx);
        cx.notify();
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.set_sel(offset, offset);
        self.goal_x = None;
        self.after_move(cx);
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.set_sel(self.anchor(), offset);
        self.goal_x = None;
        self.after_move(cx);
    }

    fn sel_lines(&self) -> RangeInclusive<usize> {
        let l0 = self.buf.line_of(self.sel.start);
        let mut l1 = self.buf.line_of(self.sel.end);
        if l1 > l0 && self.sel.end == self.buf.line_start(l1) {
            l1 -= 1;
        }
        l0..=l1
    }

    // Editing.

    fn after_edit(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.diagnostic = None;
        self.marks.clear();
        self.goal_x = None;
        self.autoscroll = true;
        self.rev += 1;
        self.hover = None;
        self.update_signature();
        self.schedule_lint(cx);
        self.refind(cx);
        self.reset_blink(cx);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// Replaces `range` with `new` and puts the cursor after it.
    fn replace(&mut self, range: Range<usize>, new: &str, kind: EditKind, cx: &mut Context<Self>) {
        let before = self.sel.clone();
        let old = self.buf.text[range.clone()].to_string();
        self.buf.edit(range.clone(), new);
        self.shift_lints(&range, new.len());
        let caret = range.start + new.len();
        self.set_sel(caret, caret);
        self.history.push(
            vec![Edit { at: range.start, old, new: new.to_string() }],
            kind,
            before,
            self.sel.clone(),
            Instant::now(),
        );
        self.after_edit(cx);
    }

    /// The selection's (anchor, head) after sorted, disjoint `edits`: text inserted at a caret
    /// goes before it; a selection grows to take in text inserted at its edges.
    fn mapped_sel(&self, edits: &[(Range<usize>, String)]) -> (usize, usize) {
        let (a, h) = (self.anchor(), self.cursor());
        let map = |o: usize, right: bool| map_offset(o, edits, right);
        if self.sel.is_empty() {
            (map(a, true), map(h, true))
        } else if self.reversed {
            (map(a, true), map(h, false))
        } else {
            (map(a, false), map(h, true))
        }
    }

    /// Applies sorted, disjoint edits as one undo step and maps the selection through them.
    fn apply(&mut self, edits: Edits, cx: &mut Context<Self>) {
        if edits.is_empty() {
            return;
        }
        let before = self.sel.clone();
        let (a, h) = self.mapped_sel(&edits);
        let mut done = Vec::with_capacity(edits.len());
        for (r, s) in edits.into_iter().rev() {
            let old = self.buf.text[r.clone()].to_string();
            self.buf.edit(r.clone(), &s);
            done.push(Edit { at: r.start, old, new: s });
        }
        self.set_sel(a, h);
        self.history.seal();
        self.history.push(done, EditKind::Other, before, self.sel.clone(), Instant::now());
        self.history.seal();
        self.menu = None;
        self.clear_lints();
        self.after_edit(cx);
    }

    fn insert_typed(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        self.marked = None;
        self.replace(range, text, EditKind::Type, cx);
        let trigger = text.chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
        if trigger {
            self.refresh_menu(false);
        } else {
            self.menu = None;
        }
    }

    fn delete_range(&mut self, range: Range<usize>, kind: EditKind, cx: &mut Context<Self>) {
        if range.is_empty() {
            return;
        }
        self.replace(range, "", kind, cx);
        if self.menu.is_some() {
            self.refresh_menu(false);
        }
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if !self.sel.is_empty() {
            return self.delete_range(self.sel.clone(), EditKind::Other, cx);
        }
        let c = self.sel.start;
        let ls = self.buf.line_start(self.buf.line_of(c));
        let before = &self.buf.text[ls..c];
        let start = if !before.is_empty() && before.bytes().all(|b| b == b' ') {
            c - ((before.len() - 1) % INDENT + 1)
        } else {
            self.buf.prev_boundary(c)
        };
        self.delete_range(start..c, EditKind::Delete, cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if !self.sel.is_empty() {
            return self.delete_range(self.sel.clone(), EditKind::Other, cx);
        }
        let c = self.sel.start;
        self.delete_range(c..self.buf.next_boundary(c), EditKind::Delete, cx);
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let r =
            if self.sel.is_empty() { word_left(self.text(), self.sel.start)..self.sel.start } else { self.sel.clone() };
        self.history.seal();
        self.delete_range(r, EditKind::Other, cx);
    }

    fn delete_word_right(&mut self, _: &DeleteWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let r = if self.sel.is_empty() {
            self.sel.start..word_right(self.text(), self.sel.start)
        } else {
            self.sel.clone()
        };
        self.history.seal();
        self.delete_range(r, EditKind::Other, cx);
    }

    fn delete_to_line_start(&mut self, _: &DeleteToLineStart, _: &mut Window, cx: &mut Context<Self>) {
        let r = if self.sel.is_empty() {
            let c = self.sel.start;
            let ls = self.buf.line_start(self.buf.line_of(c));
            if c == ls { self.buf.prev_boundary(c)..c } else { ls..c }
        } else {
            self.sel.clone()
        };
        self.history.seal();
        self.delete_range(r, EditKind::Other, cx);
    }

    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.is_some() {
            return self.accept(cx);
        }
        let ls = self.buf.line_start(self.buf.line_of(self.sel.start));
        let line = &self.buf.text[ls..self.sel.start];
        let text = format!("\n{}", indent_of(line));
        self.history.seal();
        self.replace(self.sel.clone(), &text, EditKind::Other, cx);
    }

    fn tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.is_some() {
            return self.accept(cx);
        }
        let lines = self.sel_lines();
        if lines.start() != lines.end() {
            return self.apply(indent_edits(&self.buf, lines), cx);
        }
        let ls = self.buf.line_start(self.buf.line_of(self.sel.start));
        let col = self.buf.text[ls..self.sel.start].chars().count();
        self.history.seal();
        self.replace(self.sel.clone(), &" ".repeat(INDENT - col % INDENT), EditKind::Other, cx);
    }

    fn outdent(&mut self, _: &Outdent, _: &mut Window, cx: &mut Context<Self>) {
        self.apply(outdent_edits(&self.buf, self.sel_lines()), cx);
    }

    fn toggle_comment(&mut self, _: &ToggleComment, _: &mut Window, cx: &mut Context<Self>) {
        self.apply(comment_edits(&self.buf, self.sel_lines()), cx);
    }

    /// The selection, or the whole current line when nothing is selected.
    fn copy_range(&self) -> Range<usize> {
        if !self.sel.is_empty() {
            return self.sel.clone();
        }
        let l = self.buf.line_of(self.sel.start);
        let end = if l + 1 < self.buf.line_count() { self.buf.line_start(l + 1) } else { self.buf.len() };
        self.buf.line_start(l)..end
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let r = self.copy_range();
        if !r.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.buf.text[r].to_string()));
        }
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        let r = self.copy_range();
        if !r.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.buf.text[r.clone()].to_string()));
            self.history.seal();
            self.delete_range(r, EditKind::Other, cx);
            self.history.seal();
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.history.seal();
            self.marked = None;
            self.menu = None;
            self.replace(self.sel.clone(), &normalize_newlines(&text), EditKind::Other, cx);
            self.history.seal();
        }
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(sel) = self.history.undo(&mut self.buf) {
            self.restore(sel, cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(sel) = self.history.redo(&mut self.buf) {
            self.restore(sel, cx);
        }
    }

    fn restore(&mut self, sel: Range<usize>, cx: &mut Context<Self>) {
        let len = self.buf.len();
        self.sel = sel.start.min(len)..sel.end.min(len);
        self.reversed = false;
        self.marked = None;
        self.menu = None;
        self.clear_lints();
        self.after_edit(cx);
    }

    // Movement.

    fn x_for(&self, offset: usize, window: &mut Window) -> Pixels {
        let l = self.buf.line_of(offset);
        self.shape_plain(l, window).x_for_index(offset - self.buf.line_start(l))
    }

    fn shape_plain(&self, line: usize, window: &mut Window) -> ShapedLine {
        let text = display_text(self.buf.line(line));
        let run = TextRun {
            len: text.len(),
            font: mono(),
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window.text_system().shape_line(text, code_size(), &[run], None)
    }

    /// The offset `rows` lines above/below `from`, at the goal column.
    fn vertical(&mut self, from: usize, rows: isize, window: &mut Window) -> usize {
        let x = match self.goal_x {
            Some(x) => x,
            None => self.x_for(from, window),
        };
        self.goal_x = Some(x);
        let target = self.buf.line_of(from) as isize + rows;
        if target < 0 {
            return 0;
        }
        if target as usize >= self.buf.line_count() {
            return self.buf.len();
        }
        let l = target as usize;
        self.buf.line_start(l) + self.shape_plain(l, window).closest_index_for_x(x)
    }

    fn move_vertical(&mut self, rows: isize, select: bool, window: &mut Window, cx: &mut Context<Self>) {
        let from = if select || self.sel.is_empty() {
            self.cursor()
        } else if rows < 0 {
            self.sel.start
        } else {
            self.sel.end
        };
        let to = self.vertical(from, rows, window);
        let anchor = if select { self.anchor() } else { to };
        self.set_sel(anchor, to);
        self.after_move(cx);
    }

    fn page_rows(&self) -> isize {
        let h = self.geo.map_or(px(600.), |g| g.bounds.size.height);
        ((h / line_height()).floor() as isize - 1).max(1)
    }

    fn move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        let to = if self.sel.is_empty() { self.buf.prev_boundary(self.sel.start) } else { self.sel.start };
        self.move_to(to, cx);
    }

    fn move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        let to = if self.sel.is_empty() { self.buf.next_boundary(self.sel.end) } else { self.sel.end };
        self.move_to(to, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.buf.prev_boundary(self.cursor()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.buf.next_boundary(self.cursor()), cx);
    }

    fn move_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(m) = &mut self.menu {
            m.selected = (m.selected + m.items.len() - 1) % m.items.len();
            return cx.notify();
        }
        self.move_vertical(-1, false, window, cx);
    }

    fn move_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(m) = &mut self.menu {
            m.selected = (m.selected + 1) % m.items.len();
            return cx.notify();
        }
        self.move_vertical(1, false, window, cx);
    }

    fn select_up(&mut self, _: &SelectUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(-1, true, window, cx);
    }

    fn select_down(&mut self, _: &SelectDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(1, true, window, cx);
    }

    fn page(&mut self, rows: isize, select: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.scroll.y = (self.scroll.y + line_height() * rows as f32).max(px(0.));
        self.move_vertical(rows, select, window, cx);
    }

    fn page_up(&mut self, _: &PageUp, window: &mut Window, cx: &mut Context<Self>) {
        self.page(-self.page_rows(), false, window, cx);
    }

    fn page_down(&mut self, _: &PageDown, window: &mut Window, cx: &mut Context<Self>) {
        self.page(self.page_rows(), false, window, cx);
    }

    fn select_page_up(&mut self, _: &SelectPageUp, window: &mut Window, cx: &mut Context<Self>) {
        self.page(-self.page_rows(), true, window, cx);
    }

    fn select_page_down(&mut self, _: &SelectPageDown, window: &mut Window, cx: &mut Context<Self>) {
        self.page(self.page_rows(), true, window, cx);
    }

    fn move_word_left(&mut self, _: &MoveWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(word_left(self.text(), self.cursor()), cx);
    }

    fn move_word_right(&mut self, _: &MoveWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(word_right(self.text(), self.cursor()), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(word_left(self.text(), self.cursor()), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(word_right(self.text(), self.cursor()), cx);
    }

    /// First non-blank of the line, or column 0 when already there.
    fn smart_home(&self) -> usize {
        let c = self.cursor();
        let l = self.buf.line_of(c);
        let ls = self.buf.line_start(l);
        let first = ls + indent_of(self.buf.line(l)).len();
        if c == first { ls } else { first }
    }

    fn line_end_of_cursor(&self) -> usize {
        self.buf.line_end(self.buf.line_of(self.cursor()))
    }

    fn move_line_start(&mut self, _: &MoveLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.smart_home(), cx);
    }

    fn move_line_end(&mut self, _: &MoveLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.line_end_of_cursor(), cx);
    }

    fn select_line_start(&mut self, _: &SelectLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.smart_home(), cx);
    }

    fn select_line_end(&mut self, _: &SelectLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.line_end_of_cursor(), cx);
    }

    fn move_doc_start(&mut self, _: &MoveDocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn move_doc_end(&mut self, _: &MoveDocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.buf.len(), cx);
    }

    fn select_doc_start(&mut self, _: &SelectDocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_doc_end(&mut self, _: &SelectDocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.buf.len(), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.set_sel(0, self.buf.len());
        self.menu = None;
        self.history.seal();
        cx.notify();
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            cx.notify();
        } else if self.find.as_ref().is_some_and(|f| f.open) {
            self.close_find(cx);
        } else {
            cx.propagate();
        }
    }

    fn show_character_palette(&mut self, _: &ShowCharacterPalette, window: &mut Window, _: &mut Context<Self>) {
        window.show_character_palette();
    }

    // Find and replace.

    fn find_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<FindBar> {
        if let Some(f) = &self.find {
            return f.bar.clone();
        }
        let bar = cx.new(|cx| FindBar::new(true, cx));
        let sub = cx.subscribe_in(&bar, window, Self::on_find_event);
        self.find = Some(EditorFind {
            bar: bar.clone(),
            open: false,
            matcher: None,
            invalid: false,
            matches: Vec::new(),
            capped: false,
            cur: None,
            origin: 0,
            pop_task: None,
            _sub: sub,
        });
        bar
    }

    /// ⌘F (⌥⌘F with the replace row): the bar opens on its field, prefilled with a one-line
    /// selection, and the match at or after the selection becomes the current one.
    pub fn open_find(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        let bar = self.find_bar(window, cx);
        let picked = (!self.sel.is_empty())
            .then(|| self.buf.text[self.sel.clone()].to_string())
            .filter(|s| !s.contains('\n') && s.len() <= 512);
        bar.update(cx, |b, cx| {
            if let Some(s) = &picked {
                b.set_query(s, cx);
            }
            b.set_replacing(replace, cx);
            b.focus_query(window, cx);
        });
        self.menu = None;
        if let Some(f) = &mut self.find {
            f.open = true;
            f.origin = self.sel.start;
        }
        self.compile_find(cx);
        self.find_from_origin(cx);
        cx.notify();
    }

    /// Closes the bar (the query stays for ⌘G); the selection stays on the current match.
    pub fn close_find(&mut self, cx: &mut Context<Self>) {
        if let Some(f) = &mut self.find {
            f.open = false;
            f.matches = Vec::new();
            f.cur = None;
            f.pop_task = None;
        }
        cx.notify();
    }

    /// The find bar's field has the focus (the work column counts it as the editor's region).
    pub fn find_has_focus(&self, window: &Window, cx: &App) -> bool {
        self.find.as_ref().is_some_and(|f| f.open && f.bar.read(cx).has_focus(window, cx))
    }

    fn compile_find(&mut self, cx: &mut Context<Self>) {
        let Some(f) = &mut self.find else { return };
        let (q, opts) = {
            let b = f.bar.read(cx);
            (b.query(cx), b.options())
        };
        (f.matcher, f.invalid) = match Matcher::new(&q, opts) {
            Ok(m) => (m, false),
            Err(_) => (None, true),
        };
    }

    /// Recomputes the matches of an open bar (after an edit or a new query; never per frame).
    fn refind(&mut self, cx: &mut Context<Self>) {
        let Some(f) = self.find.as_mut().filter(|f| f.open) else { return };
        (f.matches, f.capped) = match &f.matcher {
            Some(m) => m.find_all(&self.buf.text, FIND_CAP),
            None => (Vec::new(), false),
        };
        f.cur = current_match(&f.matches, &self.sel);
        self.push_find_count(cx);
    }

    fn push_find_count(&self, cx: &mut Context<Self>) {
        let Some(f) = &self.find else { return };
        let c = Count { cur: f.cur, total: f.matches.len(), capped: f.capped, busy: false, invalid: f.invalid, query: f.matcher.is_some() || f.invalid };
        f.bar.update(cx, |b, cx| b.set_count(c, cx));
    }

    /// Typing in the field: the first match at or after where the search started is selected
    /// and scrolled to; it pops once typing pauses.
    fn find_from_origin(&mut self, cx: &mut Context<Self>) {
        self.refind(cx);
        let Some(f) = &self.find else { return };
        match nearest_index(&f.matches, |m| m.start, f.origin) {
            Some(i) => self.select_match(i, false, cx),
            None => cx.notify(),
        }
    }

    /// Selects match `i`, scrolls to it and pops it (now, or after `FIND_POP_DELAY`).
    fn select_match(&mut self, i: usize, pop_now: bool, cx: &mut Context<Self>) {
        let Some(f) = &mut self.find else { return };
        let Some(r) = f.matches.get(i).cloned() else { return };
        f.cur = Some(i);
        f.pop_task = None;
        if !pop_now {
            f.pop_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(FIND_POP_DELAY).await;
                let _ = this.update(cx, |e, cx| {
                    let cur = e.find.as_ref().filter(|f| f.open).and_then(|f| f.cur.and_then(|i| f.matches.get(i).cloned()));
                    if let Some(r) = cur {
                        e.find_pop(r, cx);
                    }
                });
            }));
        }
        self.set_sel(r.start, r.end);
        self.goal_x = None;
        self.menu = None;
        self.history.seal();
        self.autoscroll = true;
        self.push_find_count(cx);
        if pop_now {
            self.find_pop(r, cx);
        }
        cx.notify();
    }

    /// ⌘G / ⇧⌘G (↩ / ⇧↩ in the field): the next or previous match from the selection, around
    /// the ends. With the bar closed ⌘G brings it back on the last query (the focus stays
    /// here); with no query yet it is ⌘F.
    pub fn find_step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.find.as_ref().is_some_and(|f| f.open) {
            if !self.find.as_ref().is_some_and(|f| f.matcher.is_some()) {
                return self.open_find(false, window, cx);
            }
            if let Some(f) = &mut self.find {
                f.open = true;
            }
            self.refind(cx);
        }
        let Some(f) = &self.find else { return };
        let key = |m: &Range<usize>| (m.start, m.end);
        let at = (self.sel.start, self.sel.end);
        let ix = if forward { next_index(&f.matches, key, at) } else { prev_index(&f.matches, key, at) };
        if let Some(i) = ix {
            self.select_match(i, true, cx);
        }
    }

    /// Replace: the current match (the selection when it is one, else the next one) becomes
    /// the replacement, as one undo step; then the next match is selected.
    fn replace_current(&mut self, cx: &mut Context<Self>) {
        let Some(f) = &self.find else { return };
        let Some(m) = f.matcher.clone() else { return };
        let rep = f.bar.read(cx).replacement(cx);
        let i = f.matches.iter().position(|r| *r == self.sel).or_else(|| nearest_index(&f.matches, |r| r.start, self.sel.start));
        let Some(r) = i.and_then(|i| f.matches.get(i).cloned()) else { return };
        let new = m.expand(&self.buf.text, r.clone(), &rep);
        self.history.seal();
        self.marked = None;
        self.menu = None;
        self.replace(r.clone(), &new, EditKind::Other, cx);
        self.history.seal();
        let at = r.start + new.len();
        if let Some(i) = self.find.as_ref().and_then(|f| nearest_index(&f.matches, |x| x.start, at)) {
            self.select_match(i, true, cx);
        }
    }

    /// Replace All: every match (not only the first `FIND_CAP`), `$1` groups expanded for a
    /// regular expression, in one edit and one undo step.
    fn replace_all_matches(&mut self, cx: &mut Context<Self>) {
        let Some(f) = &self.find else { return };
        let Some(m) = f.matcher.clone() else { return };
        let rep = f.bar.read(cx).replacement(cx);
        let edits = m.replace_all(&self.buf.text, &rep);
        if edits.is_empty() {
            return;
        }
        let before = self.sel.clone();
        let (a, h) = self.mapped_sel(&edits);
        let Some(edit) = splice_edits(&mut self.buf, &edits) else { return };
        self.set_sel(a, h);
        self.history.seal();
        self.history.push(vec![edit], EditKind::Other, before, self.sel.clone(), Instant::now());
        self.history.seal();
        self.marked = None;
        self.menu = None;
        self.clear_lints();
        self.after_edit(cx);
    }

    fn on_find_event(&mut self, _: &Entity<FindBar>, ev: &FindBarEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            FindBarEvent::Changed => {
                self.compile_find(cx);
                self.find_from_origin(cx);
            }
            FindBarEvent::Next => self.find_step(true, window, cx),
            FindBarEvent::Prev => self.find_step(false, window, cx),
            FindBarEvent::Close => {
                self.close_find(cx);
                window.focus(&self.focus, cx);
            }
            FindBarEvent::Replace => self.replace_current(cx),
            FindBarEvent::ReplaceAll => self.replace_all_matches(cx),
        }
    }

    /// Demo and checks: the bar (once created), whether it is open, and its count (`3/17`).
    pub fn demo_find(&self, cx: &App) -> Option<(Entity<FindBar>, bool, String)> {
        self.find.as_ref().map(|f| (f.bar.clone(), f.open, f.bar.read(cx).count().label()))
    }

    /// Demo and checks: the selection.
    /// Demo and checks: the line height and the measured advance of a character, in window
    /// points (both follow the UI zoom).
    pub fn demo_metrics(&self) -> (f32, f32) {
        (line_height().as_f32(), self.geo.map_or(0., |g| g.char_width.as_f32()))
    }

    pub fn demo_selection(&self) -> Range<usize> {
        self.sel.clone()
    }

    /// Demo and checks: the left edge of the gutter's divider (window pixels), as last laid out.
    pub fn demo_gutter_divider(&self) -> Option<f32> {
        self.geo.map(|g| (g.bounds.left() + g.gutter_w - px(1.)).as_f32())
    }

    /// The selection as (anchor, head) byte offsets (the saved session).
    pub fn selection_ends(&self) -> (usize, usize) {
        if self.reversed { (self.sel.end, self.sel.start) } else { (self.sel.start, self.sel.end) }
    }

    /// The scroll in points at 100 % (the saved session).
    pub fn scroll_points(&self) -> (f32, f32) {
        (self.scroll.x.pt(), self.scroll.y.pt())
    }

    /// Checks: the last frame's scrollbars as (track, thumb), vertical then horizontal, and
    /// how far the content scrolls (x, y), in window coordinates.
    pub fn demo_bars(&self) -> ([Option<(Bounds<Pixels>, Bounds<Pixels>)>; 2], (f32, f32)) {
        let (v, h) = self.bars;
        let (mx, my) = self.max_scroll();
        ([v.map(|b| (b.track, b.thumb)), h.map(|b| (b.track, b.thumb))], (mx.pt(), my.pt()))
    }

    /// Bumped on every change of the text.
    pub fn revision(&self) -> u64 {
        self.rev
    }

    /// A saved editor back (session restore): its text, selection and scroll, without
    /// scrolling to the cursor.
    pub fn restore_saved(&mut self, text: &str, (anchor, head): (usize, usize), (sx, sy): (f32, f32), cx: &mut Context<Self>) {
        self.set_text(text, cx);
        let at = |i: usize| {
            let (s, mut i) = (self.buf.text(), i.min(self.buf.len()));
            while !s.is_char_boundary(i) {
                i -= 1;
            }
            i
        };
        let (a, h) = (at(anchor), at(head));
        self.set_sel(a, h);
        self.scroll = point(px(sx.max(0.)), px(sy.max(0.)));
        self.zoom_at = crate::theme::zoom();
        self.autoscroll = false;
        cx.notify();
    }

    /// Demo: selects `anchor..head` (the `selection` screen).
    pub fn demo_select(&mut self, anchor: usize, head: usize, cx: &mut Context<Self>) {
        let n = self.buf.len();
        self.set_sel(anchor.min(n), head.min(n));
        cx.notify();
    }

    // Execution.

    fn run_statement_action(&mut self, _: &RunStatement, window: &mut Window, cx: &mut Context<Self>) {
        self.run_statement(window, cx);
    }

    /// Run Statement (⌘⇧↩): emits `Run` with the selection's statements, else the statement
    /// under the cursor.
    pub fn run_statement(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        let text = self.buf.text();
        let (stmts, base) = if self.sel.is_empty() {
            let all = self.buf.statements();
            let mut at: Vec<_> = statement_at_in(text, &all, self.sel.start).into_iter().collect();
            // T-SQL: a statement using variables declared above it runs with them.
            if self.buf.dialect == dataman_core::sql::Dialect::TSql {
                for s in &mut at {
                    s.range = dataman_core::sql::split::tsql_run_range(text, s);
                }
            }
            (at, 0)
        } else {
            (batches_in(&text[self.sel.clone()], self.buf.dialect), self.sel.start)
        };
        self.emit_run(stmts.iter().map(|s| base + s.range.start..base + s.range.end).collect(), false, cx);
    }

    /// The statement Explain works on: the first of the selection, else the one under the
    /// cursor (flashed like a run).
    pub fn current_statement(&mut self, cx: &mut Context<Self>) -> Option<Stmt> {
        self.menu = None;
        let text = self.buf.text();
        let range = if self.sel.is_empty() {
            let all = self.buf.statements();
            statement_at_in(text, &all, self.sel.start).map(|s| s.range)
        } else {
            split_in(&text[self.sel.clone()], self.buf.dialect).into_iter().find(|s| !text[self.sel.start + s.range.start..self.sel.start + s.range.end].trim().is_empty()).map(|s| self.sel.start + s.range.start..self.sel.start + s.range.end)
        }?;
        let sql = text[range.clone()].to_string();
        if sql.trim().is_empty() || self.plain {
            return None;
        }
        self.flash(range.clone(), cx);
        Some(Stmt { at: range.start, sql, params: Vec::new(), rollback: false })
    }

    fn run_script_action(&mut self, _: &RunScript, _: &mut Window, cx: &mut Context<Self>) {
        self.run_script(cx);
    }

    /// Run (⌘↩, the toolbar's button): the selection's statements, else every statement of the
    /// buffer (T-SQL: every batch).
    pub fn run_script(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        if !self.sel.is_empty() {
            let base = self.sel.start;
            let stmts = batches_in(&self.buf.text()[self.sel.clone()], self.buf.dialect);
            self.emit_run(stmts.iter().map(|s| base + s.range.start..base + s.range.end).collect(), true, cx);
            return;
        }
        let stmts = self.buf.batches();
        self.emit_run(stmts.into_iter().map(|s| s.range).collect(), true, cx);
    }

    /// ⌥⌘↩: the statement (T-SQL: the batch) under the cursor and every one after it.
    pub fn run_from_here(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        let text = self.buf.text();
        let all = self.buf.batches();
        let from = statement_at_in(text, &all, self.sel.start).map(|s| s.range.start).unwrap_or(self.sel.start);
        let ranges = all.into_iter().map(|s| s.range).filter(|r| r.end > from).collect();
        self.emit_run(ranges, true, cx);
    }

    fn run_from_here_action(&mut self, _: &RunFromHere, _: &mut Window, cx: &mut Context<Self>) {
        self.run_from_here(cx);
    }

    fn emit_run(&mut self, ranges: Vec<Range<usize>>, all: bool, cx: &mut Context<Self>) {
        // Plain text has no statements: ⌘↩ just says "done" (a value editor applies).
        if self.plain {
            cx.emit(EditorEvent::Run(Vec::new()));
            return;
        }
        let ranges: Vec<_> = ranges.into_iter().filter(|r| !self.buf.text[r.clone()].trim().is_empty()).collect();
        let (Some(first), Some(last)) = (ranges.first(), ranges.last()) else { return };
        let span = first.start..last.end;
        let stmts = ranges.iter().map(|r| Stmt { at: r.start, sql: self.buf.text[r.clone()].to_string(), params: Vec::new(), rollback: false }).collect();
        self.last_run = Some(span.clone());
        self.flash(span, cx);
        cx.emit(if all { EditorEvent::RunAll(stmts) } else { EditorEvent::Run(stmts) });
    }

    // Completion.

    /// Lines around the cursor, starting at a token boundary.
    fn completion_window(&self, cursor: usize) -> Range<usize> {
        let l = self.buf.line_of(cursor);
        let a = self.buf.line_start(l.saturating_sub(COMPLETION_LINES));
        let b = self.buf.line_end((l + COMPLETION_LINES).min(self.buf.line_count() - 1));
        let a = self.buf.tokens.get(self.buf.token_at(a)).map_or(a, |t| t.start.min(a));
        a..b.max(cursor)
    }

    /// Recomputes the popup at the cursor; with an empty partial word it stays open only when
    /// `forced` or right after a `.`.
    fn refresh_menu(&mut self, forced: bool) {
        if !self.sel.is_empty() || self.marked.is_some() || self.plain {
            self.menu = None;
            return;
        }
        let cursor = self.sel.end;
        let w = self.completion_window(cursor);
        let c = complete(&self.buf.text[w.clone()], cursor - w.start, &self.schema);
        let replace = w.start + c.replace.start..w.start + c.replace.end;
        let after_dot = self.buf.text[..replace.start].ends_with('.');
        let partial = &self.buf.text[replace.clone()];
        let exact = c.items.len() == 1 && c.items[0].insert.eq_ignore_ascii_case(partial);
        self.menu = if c.items.is_empty() || exact || (replace.is_empty() && !forced && !after_dot) {
            None
        } else {
            Some(Menu { replace, items: c.items, selected: 0, top: 0 })
        };
    }

    fn show_completions(&mut self, _: &ShowCompletions, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh_menu(true);
        cx.notify();
    }

    fn accept(&mut self, cx: &mut Context<Self>) {
        let selected = self.menu.as_ref().map_or(0, |m| m.selected);
        self.accept_item(selected, cx);
    }

    fn accept_item(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(m) = self.menu.take() else { return };
        let Some(item) = m.items.get(ix) else { return };
        self.history.seal();
        self.replace(m.replace.clone(), &item.insert, EditKind::Other, cx);
        self.history.seal();
        // A call: the cursor goes between its parens, and its signature shows.
        if let Some(c) = item.caret {
            let at = m.replace.start + c;
            self.set_sel(at, at);
            self.update_signature();
        }
    }

    // Mouse and scrolling.

    fn offset_at(&self, pos: Point<Pixels>, window: &mut Window) -> usize {
        let Some(g) = self.geo else { return 0 };
        let y = pos.y - g.bounds.top() - px(TOP_PAD) + self.scroll.y;
        let row = (y / line_height()).floor();
        if row >= self.buf.line_count() as f32 {
            return self.buf.len();
        }
        let l = row.max(0.) as usize;
        let x = (pos.x - g.origin_x + self.scroll.x).max(px(0.));
        self.buf.line_start(l) + self.shape_plain(l, window).closest_index_for_x(x)
    }

    fn line_range(&self, offset: usize) -> Range<usize> {
        let l = self.buf.line_of(offset);
        let end = if l + 1 < self.buf.line_count() { self.buf.line_start(l + 1) } else { self.buf.len() };
        self.buf.line_start(l)..end
    }

    fn mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        if self.bar_down(ev.position, cx) {
            return;
        }
        self.menu = None;
        self.history.seal();
        self.goal_x = None;
        let off = self.offset_at(ev.position, window);
        // ⌘-click: go to the definition of the name clicked.
        if ev.modifiers.platform && ev.click_count == 1 && !ev.modifiers.shift {
            self.set_sel(off, off);
            self.update_signature();
            self.go_to_definition(off, false, cx);
            cx.notify();
            return;
        }
        match ev.click_count {
            2 => {
                let r = word_at(self.text(), off);
                self.set_sel(r.start, r.end);
                self.drag = Some(Drag::Word(r));
            }
            n if n >= 3 => {
                let r = self.line_range(off);
                self.set_sel(r.start, r.end);
                self.drag = Some(Drag::Line(r));
            }
            _ => {
                let anchor = if ev.modifiers.shift { self.anchor() } else { off };
                self.set_sel(anchor, off);
                self.drag = Some(Drag::Char);
            }
        }
        self.update_signature();
        cx.notify();
    }

    /// A press on a scrollbar: on its thumb it grabs it, elsewhere on the track it pages
    /// towards the press. `false` when the press is not on one.
    fn bar_down(&mut self, p: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let (v, h) = self.bars;
        for (bar, vertical) in [(v, true), (h, false)] {
            let Some(b) = bar.filter(|b| b.track.contains(&p)) else { continue };
            let (at, start, end) = if vertical { (p.y, b.thumb.top(), b.thumb.bottom()) } else { (p.x, b.thumb.left(), b.thumb.right()) };
            if at >= start && at <= end {
                self.drag = Some(Drag::Thumb { vertical, grab: at - start });
            } else {
                let page = if vertical { b.track.size.height - line_height() } else { b.track.size.width * 0.9 };
                let step = if at < start { -page } else { page };
                if vertical {
                    self.scroll.y += step;
                } else {
                    self.scroll.x += step;
                }
                self.clamp_scroll();
            }
            cx.notify();
            return true;
        }
        false
    }

    /// A grabbed thumb follows the pointer.
    fn thumb_drag(&mut self, vertical: bool, grab: Pixels, pos: Point<Pixels>) {
        let (max_x, max_y) = self.max_scroll();
        let bar = if vertical { self.bars.0 } else { self.bars.1 };
        let Some(b) = bar else { return };
        if vertical {
            let room = b.track.size.height - b.thumb.size.height;
            if room > px(0.) {
                self.scroll.y = max_y * ((pos.y - b.track.top() - grab) / room).clamp(0., 1.);
            }
        } else {
            let room = b.track.size.width - b.thumb.size.width;
            if room > px(0.) {
                self.scroll.x = max_x * ((pos.x - b.track.left() - grab) / room).clamp(0., 1.);
            }
        }
    }

    fn mouse_drag(&mut self, pos: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Drag::Thumb { vertical, grab }) = self.drag {
            self.thumb_drag(vertical, grab, pos);
            cx.notify();
            return;
        }
        let off = self.offset_at(pos, window);
        let (a, h) = match &self.drag {
            None | Some(Drag::Thumb { .. }) => return,
            Some(Drag::Char) => (self.anchor(), off),
            Some(Drag::Word(orig)) => {
                let w = word_at(self.text(), off);
                if w.start < orig.start { (orig.end, w.start) } else { (orig.start, w.end.max(orig.end)) }
            }
            Some(Drag::Line(orig)) => {
                let w = self.line_range(off);
                if w.start < orig.start { (orig.end, w.start) } else { (orig.start, w.end.max(orig.end)) }
            }
        };
        if (a, h) != (self.anchor(), self.cursor()) {
            self.set_sel(a, h);
            self.autoscroll = true;
            cx.notify();
        }
    }

    fn scroll_by(&mut self, delta: Point<Pixels>, cx: &mut Context<Self>) {
        self.scroll.x -= delta.x;
        self.scroll.y -= delta.y;
        self.clamp_scroll();
        cx.notify();
    }

    /// How far the content scrolls (x, y): three lines past the last one, four columns past the
    /// longest line.
    fn max_scroll(&self) -> (Pixels, Pixels) {
        let Some(g) = self.geo else { return (px(0.), px(0.)) };
        let lh = line_height();
        let total = lh * self.buf.line_count() as f32;
        let max_y = (total + px(TOP_PAD) - g.bounds.size.height + lh * 3.).max(px(0.));
        let max_x = (g.char_width * (self.buf.longest + 4) as f32 - g.text_width).max(px(0.));
        (max_x, max_y)
    }

    fn clamp_scroll(&mut self) {
        if self.geo.is_none() {
            return;
        }
        let (max_x, max_y) = self.max_scroll();
        self.scroll.y = self.scroll.y.clamp(px(0.), max_y);
        self.scroll.x = self.scroll.x.clamp(px(0.), max_x);
    }

    /// The scrollbars for this frame: one per direction with more to see, over the right and
    /// bottom edges (the horizontal one under the text only).
    fn layout_bars(&self, text_left: Pixels) -> (Option<Bar>, Option<Bar>) {
        let Some(g) = self.geo else { return (None, None) };
        let (max_x, max_y) = self.max_scroll();
        let b = g.bounds;
        // Content that fits (the three lines past the end aside) shows no bar.
        let lh = line_height();
        let tall = max_y > lh * 3. + px(0.5);
        let wide = max_x > px(0.5);
        let bar = px(BAR);
        let v = tall.then(|| {
            let h = b.size.height - if wide { bar } else { px(0.) };
            let track = Bounds::new(point(b.right() - bar, b.top()), size(bar, h));
            let (start, len) = thumb_span(h.pt(), b.size.height.pt(), max_y.pt(), self.scroll.y.pt());
            let thumb = Bounds::new(point(b.right() - px(THUMB + 2.), b.top() + px(start)), size(px(THUMB), px(len)));
            Bar { track, thumb }
        });
        let h = wide.then(|| {
            let w = b.right() - text_left - if tall { bar } else { px(0.) };
            let track = Bounds::new(point(text_left, b.bottom() - bar), size(w, bar));
            let (start, len) = thumb_span(w.pt(), g.text_width.pt(), max_x.pt(), self.scroll.x.pt());
            let thumb = Bounds::new(point(text_left + px(start), b.bottom() - px(THUMB + 2.)), size(px(len), px(THUMB)));
            Bar { track, thumb }
        });
        (v, h)
    }

    /// Scrolls so the cursor is visible (once, after a move or an edit).
    fn autoscroll_now(&mut self, window: &mut Window) {
        let Some(g) = self.geo else { return };
        if !std::mem::take(&mut self.autoscroll) {
            return;
        }
        let lh = line_height();
        let h = g.bounds.size.height;
        let c = self.cursor();
        let top = lh * self.buf.line_of(c) as f32 + px(TOP_PAD);
        let margin = if h > lh * 8. { lh * 2. } else { px(0.) };
        if top - margin < self.scroll.y {
            self.scroll.y = top - margin;
        } else if top + lh + margin > self.scroll.y + h {
            self.scroll.y = top + lh + margin - h;
        }
        let x = self.x_for(c, window);
        let cw = g.char_width;
        if x < self.scroll.x + cw {
            self.scroll.x = x - cw * 4.;
        } else if x > self.scroll.x + g.text_width - cw * 2. {
            self.scroll.x = x - g.text_width + cw * 4.;
        }
        self.clamp_scroll();
    }

    // Painting.

    fn line_runs(&self, t: &Theme, l: usize, text_len: usize) -> Vec<TextRun> {
        let (start, end) = (self.buf.line_start(l), self.buf.line_end(l));
        let toks = &self.buf.tokens;
        let text = self.buf.text();
        let mut runs = Vec::new();
        let mut i = self.buf.token_at(start);
        let mut pos = start;
        let base =
            TextRun { len: 0, font: mono(), color: t.tx, background_color: None, underline: None, strikethrough: None };
        while pos < end {
            let (kind, tstart, tend) =
                toks.get(i).map_or((TokenKind::Unknown, pos, end), |t| (t.kind, t.start, t.end.min(end)));
            let tend = tend.max(pos + 1).min(end);
            let color = if self.plain {
                t.tx
            } else if matches!(kind, TokenKind::Keyword | TokenKind::Ident) {
                let word = &text[tstart.max(start)..tend];
                let paren = toks
                    .get(i + 1..)
                    .and_then(|rest| rest.iter().find(|t| t.kind != TokenKind::Whitespace))
                    .is_some_and(|t| &text[t.start..t.end] == "(");
                token_color(t, kind, word, paren)
            } else {
                token_color(t, kind, "", false)
            };
            runs.push(TextRun { len: tend - pos, color, ..base.clone() });
            pos = tend;
            i += 1;
        }
        debug_assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text_len);
        if let Some(e) = &self.error {
            let u = UnderlineStyle { thickness: px(1.), color: Some(t.err), wavy: true };
            let bg = t.err_l;
            decorate_runs(&mut runs, start, e, |r| {
                r.underline = Some(u);
                r.background_color = Some(bg);
            });
        }
        // Lint squiggles (red errors, amber warnings).
        let hi = self.lints.partition_point(|l| l.range.start <= end);
        for l in self.lints[..hi].iter().filter(|l| l.range.end >= start && !self.being_typed(l)) {
            let color = if l.severity == Severity::Error { t.err } else { t.warn };
            underline_runs(&mut runs, start, &l.range, UnderlineStyle { thickness: px(1.), color: Some(color), wavy: true });
        }
        if let Some(m) = &self.marked {
            let u = UnderlineStyle { thickness: px(1.), color: Some(t.tx), wavy: false };
            underline_runs(&mut runs, start, m, u);
        }
        runs
    }

    /// An unknown name that ends at the cursor is still being typed: it waits for the cursor to
    /// leave.
    fn being_typed(&self, l: &Lint) -> bool {
        self.sel.is_empty() && l.range.end == self.sel.end && l.message.ends_with("does not exist")
    }

    /// The lint under the mouse (`None`: the mouse left the editor).
    fn hover_at(&mut self, pos: Option<Point<Pixels>>, window: &mut Window, cx: &mut Context<Self>) {
        let ix = pos.and_then(|p| {
            let off = self.offset_at(p, window);
            self.lints.iter().position(|l| l.range.start <= off && off < l.range.end && !self.being_typed(l))
        });
        if ix != self.hover {
            self.hover = ix;
            cx.notify();
        }
    }

    /// The hovered lint's message, under its first character.
    fn render_hover(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let l = self.lints.get(self.hover?)?.clone();
        let g = self.geo?;
        let t = cx.theme();
        let at = l.range.start.min(self.buf.len());
        let x = g.origin_x - self.scroll.x + self.x_for(at, window);
        let y = g.bounds.top() + px(TOP_PAD) + line_height() * (self.buf.line_of(at) + 1) as f32 - self.scroll.y + px(2.);
        let color = if l.severity == Severity::Error { t.err } else { t.warn };
        Some(
            deferred(
                anchored().position(point(x - px(8.), y)).snap_to_window_with_margin(px(8.)).child(
                    div()
                        .occlude()
                        .px(px(8.))
                        .py(px(4.))
                        .bg(t.pop)
                        .shadow(t.ring())
                        .font_family(crate::theme::mono())
                        .text_size(px(11.5))
                        .text_color(color)
                        .child(l.message),
                ),
            )
            .priority(1),
        )
    }

    /// Signature help: the call's parameters above the cursor's line, the one being typed in
    /// bold accent.
    fn render_signature(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        if !self.focused {
            return None;
        }
        let s = self.sig.clone()?;
        let g = self.geo?;
        let t = cx.theme();
        let name_at = s.paren.saturating_sub(s.function.name.len()).min(self.buf.len());
        let x = g.origin_x - self.scroll.x + self.x_for(name_at, window);
        let y = g.bounds.top() + px(TOP_PAD) + line_height() * self.buf.line_of(self.sel.end) as f32 - self.scroll.y - px(4.);
        let mut parts: Vec<(String, bool)> = vec![(format!("{}(", s.function.name), false)];
        for (i, p) in s.params.iter().enumerate() {
            if i > 0 {
                parts.push((", ".into(), false));
            }
            parts.push((p.clone(), s.active == Some(i)));
        }
        parts.push((")".into(), false));
        let result = (s.function.kind != 'p' && !s.function.result.is_empty()).then(|| format!("→ {}", s.function.result));
        Some(
            deferred(
                anchored().anchor(gpui::Anchor::BottomLeft).position(point(x - px(8.), y)).snap_to_window_with_margin(px(8.)).child(
                    div()
                        .occlude()
                        .max_w(px(640.))
                        .px(px(8.))
                        .py(px(4.))
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap(px(8.))
                        .bg(t.pop)
                        .shadow(t.ring())
                        .font_family(crate::theme::mono())
                        .text_size(px(12.))
                        .line_height(px(18.))
                        .child(div().flex().flex_row().flex_wrap().text_color(t.tx2).children(parts.into_iter().map(|(text, on)| {
                            div().when(on, |d| d.text_color(t.acc).font_weight(FontWeight::BOLD)).child(text)
                        })))
                        .children(result.map(|r| div().text_color(t.typ).child(r)))
                        .when(s.overloads > 1, |d| d.child(div().text_color(t.tx4).child(format!("+{}", s.overloads - 1)))),
                ),
            )
            .priority(1),
        )
    }

    /// Quads covering `range` on the visible lines. With a `radius`, the runs read as one shape
    /// (Xcode's, Zed's): a corner is rounded where it is on the outline, and square where the
    /// run above or below continues past it.
    fn range_quads(
        &self,
        range: &Range<usize>,
        lines: &[(usize, ShapedLine)],
        color: Hsla,
        radius: f32,
        g: &Geometry,
        out: &mut Vec<PaintQuad>,
    ) {
        let lh = line_height();
        let x = g.origin_x - self.scroll.x;
        // (line, left, right) of each run, top to bottom.
        let mut runs: Vec<(usize, Pixels, Pixels)> = Vec::new();
        for (l, shaped) in lines {
            let (ls, le) = (self.buf.line_start(*l), self.buf.line_end(*l));
            if range.end < ls || range.start > le || (range.end == ls && !range.is_empty()) {
                continue;
            }
            let x0 = if range.start > ls { shaped.x_for_index(range.start - ls) } else { px(0.) };
            let x1 =
                if range.end <= le { shaped.x_for_index(range.end - ls) } else { shaped.width() + g.char_width * 0.6 };
            if x1 <= x0 {
                continue;
            }
            runs.push((*l, x + x0, x + x1));
        }
        for (i, &(l, x0, x1)) in runs.iter().enumerate() {
            let y = g.bounds.top() + px(TOP_PAD) + lh * l as f32 - self.scroll.y;
            let b = Bounds::from_corners(point(x0, y), point(x1, y + lh));
            if radius <= 0. {
                out.push(fill(b, color));
                continue;
            }
            // The runs on the lines above and below.
            let near = |n: Option<&(usize, Pixels, Pixels)>, line: usize| n.filter(|n| n.0 == line).map(|n| (n.1.as_f32(), n.2.as_f32()));
            let above = if i > 0 { near(runs.get(i - 1), l - 1) } else { None };
            let round = outline_corners(above, (x0.as_f32(), x1.as_f32()), near(runs.get(i + 1), l + 1));
            let r = px(radius).min((x1 - x0) / 2.);
            let corner = |on: bool| if on { r } else { px(0.) };
            let corners =
                gpui::Corners { top_left: corner(round[0]), top_right: corner(round[1]), bottom_right: corner(round[2]), bottom_left: corner(round[3]) };
            out.push(gpui::quad(b, corners, color, px(0.), gpui::transparent_black(), gpui::BorderStyle::Solid));
        }
    }

    fn layout(&mut self, bounds: Bounds<Pixels>, hitbox: Hitbox, find_focused: bool, window: &mut Window, t: &Theme) -> Prepainted {
        let lh = line_height();
        let fs = code_size();
        let ts = window.text_system().clone();
        let run = |len: usize, color: Hsla| TextRun {
            len,
            font: mono(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let cw = ts.shape_line("0".into(), fs, &[run(1, t.tx)], None).width();
        let digits = self.buf.line_count().to_string().len();
        let need = px(GUTTER_MIN).max(cw * digits as f32 + px(32.));
        let gutter_w = self.gutter.as_ref().map_or(need, |g| px(g.editor(need.pt())));
        let text_bounds = Bounds::from_corners(point(bounds.left() + gutter_w, bounds.top()), bounds.bottom_right());
        let origin_x = text_bounds.left() + px(12.);
        let g = Geometry { bounds, origin_x, gutter_w, text_width: text_bounds.size.width - px(12.), char_width: cw };
        // The scroll is in window points: a zoom keeps the same lines in view.
        let z = crate::theme::zoom();
        if (z - self.zoom_at).abs() > 1e-4 {
            let k = z / self.zoom_at;
            self.scroll = point(self.scroll.x * k, self.scroll.y * k);
            self.goal_x = None;
            self.zoom_at = z;
        }
        self.geo = Some(g);
        self.autoscroll_now(window);
        self.clamp_scroll();

        let count = self.buf.line_count();
        let first = (((self.scroll.y - px(TOP_PAD)) / lh).floor().max(0.) as usize).min(count);
        let last = (first + (bounds.size.height / lh).ceil() as usize + 2).min(count);
        let cursor = self.cursor();
        let cursor_line = self.buf.line_of(cursor);
        let focused = self.focus.is_focused(window);
        let y_of = |l: usize| bounds.top() + px(TOP_PAD) + lh * l as f32 - self.scroll.y;
        let full_row = |l: usize, color: Hsla| {
            fill(Bounds::from_corners(point(bounds.left(), y_of(l)), point(bounds.right(), y_of(l) + lh)), color)
        };
        let bar = |l: usize, color: Hsla| fill(Bounds::new(point(bounds.left(), y_of(l)), size(px(2.), lh)), color);

        let num_size = px(11.5);
        let mut lines = Vec::with_capacity(last - first);
        let mut numbers = Vec::with_capacity(last - first);
        for l in first..last {
            let text = display_text(self.buf.line(l));
            let runs = self.line_runs(t, l, text.len());
            let shaped = ts.shape_line(text, fs, &runs, None);
            lines.push((l, shaped));
            let color = if l == cursor_line { t.tx } else { t.tx4 };
            let n: SharedString = (l + 1).to_string().into();
            let shaped_n = ts.shape_line(n.clone(), num_size, &[run(n.len(), color)], None);
            numbers.push((point(bounds.left() + gutter_w - px(12.) - shaped_n.width(), y_of(l)), shaped_n));
        }

        // Whole-width rows: the run flash, the failed statement, the current line.
        let mut rows = Vec::new();
        let lines_of = |r: &Range<usize>| self.buf.line_of(r.start)..=self.buf.line_of(r.end.max(r.start));
        let mut flash_lines = None;
        if let Some((range, at)) = self.flash.clone() {
            let p = at.elapsed().as_secs_f32() / FLASH.as_secs_f32();
            if p >= 1. {
                self.flash = None;
            } else {
                // ease-out: most of the fade happens at the start.
                let a = (1. - p).powi(3);
                let ls = lines_of(&range);
                for l in first.max(*ls.start())..last.min(ls.end() + 1) {
                    rows.push(full_row(l, t.acc_s.opacity(a)));
                    rows.push(bar(l, t.acc.opacity(a)));
                }
                flash_lines = Some(ls);
                window.request_animation_frame();
            }
        }
        let mut err_lines = None;
        if let Some(d) = &self.diagnostic {
            let ls = lines_of(&d.stmt);
            for l in first.max(*ls.start())..last.min(ls.end() + 1) {
                rows.push(full_row(l, t.err_line));
                rows.push(bar(l, t.err));
            }
            err_lines = Some(ls);
        }
        let highlighted = |l: usize| {
            flash_lines.as_ref().is_some_and(|r| r.contains(&l)) || err_lines.as_ref().is_some_and(|r| r.contains(&l))
        };
        if self.sel.is_empty() && (first..last).contains(&cursor_line) && !highlighted(cursor_line) {
            rows.insert(0, full_row(cursor_line, t.cur_line));
        }

        let mut backgrounds = Vec::new();
        // Every match of an open find bar, faintly: only the visible ones, found by binary search
        // in the list computed at the last edit or query change.
        if let Some(f) = self.find.as_ref().filter(|f| f.open && !f.matches.is_empty())
            && first < last
        {
            let (vs, ve) = (self.buf.line_start(first), self.buf.line_end(last - 1));
            let lo = f.matches.partition_point(|m| m.end <= vs);
            let hi = f.matches.partition_point(|m| m.start <= ve).max(lo);
            // The current match is the selection, painted plain below.
            for m in f.matches[lo..hi].iter().filter(|m| **m != self.sel) {
                let l0 = self.buf.line_of(m.start).max(first);
                let l1 = self.buf.line_of(m.end).min(last - 1);
                if l0 <= l1 {
                    self.range_quads(m, &lines[l0 - first..=l1 - first], t.find_s, 0., &g, &mut backgrounds);
                }
            }
        }
        // The current match is the selection: it reads as selected while the bar has the keys.
        let sel_color = if focused || find_focused { t.sel } else { t.prs };
        self.range_quads(&self.sel, &lines, sel_color, SEL_RADIUS, &g, &mut backgrounds);

        // The find indicator (⌘' on the error): Xcode's yellow box over the range (its first
        // line), 2 pt wider than the text each side, the text redrawn dark on top at the box's
        // scale. Positions come from this frame's layout, so it scrolls with the text.
        let mut pop = None;
        match self.pop.as_ref().map(|p| (p.frame(Instant::now()), p.target.clone())) {
            Some((None, _)) => self.pop = None,
            Some((Some(f), target)) => {
                window.request_animation_frame();
                let r = self.clamp_range(target);
                let l = self.buf.line_of(r.start);
                if let Some((_, shaped)) = lines.iter().find(|(x, _)| *x == l) {
                    let ls = self.buf.line_start(l);
                    let end = r.end.min(self.buf.line_end(l)).max(r.start);
                    let x0 = shaped.x_for_index(r.start - ls);
                    let x1 = if end > r.start { shaped.x_for_index(end - ls) } else { x0 + cw };
                    let x = origin_x - self.scroll.x;
                    let rest = Bounds::from_corners(point(x + x0 - px(2.), y_of(l)), point(x + x1 + px(2.), y_of(l) + lh));
                    let text = display_text(&self.buf.text[r.start..end]);
                    let line = ts.shape_line(text.clone(), fs * f.scale, &[run(text.len(), t.find_tx.opacity(f.strength))], None);
                    pop = Some(EditorPop { rest, fill: t.find.opacity(f.strength), scale: f.scale, line, origin: point(x + x0, y_of(l)) });
                }
            }
            None => {}
        }

        // Gutter marks of a multi-statement run.
        let mut marks = Vec::new();
        for &(at, m) in &self.marks {
            if at > self.buf.len() {
                continue;
            }
            let mut l = self.buf.line_of(at);
            // A statement offset may point at the blank line before its first word.
            while l + 1 < count && self.buf.line(l).trim().is_empty() {
                l += 1;
            }
            if !(first..last).contains(&l) {
                continue;
            }
            let (glyph, color) = match m {
                Mark::Ok => ("✓", t.ok),
                Mark::Err => ("✕", t.err),
                Mark::Running => ("▸", t.acc),
                Mark::Todo => ("·", t.tx4),
            };
            let shaped = ts.shape_line(glyph.into(), px(10.), &[run(glyph.len(), color)], None);
            let x = bounds.left() + px(8.) - shaped.width() / 2.;
            marks.push((point(x, y_of(l)), shaped));
        }

        // The error message beside its line.
        let lens = self.diagnostic.as_ref().and_then(|d| {
            let l = self.buf.line_of(d.at.unwrap_or(d.stmt.start).min(self.buf.len()));
            let (_, shaped) = lines.iter().find(|(x, _)| *x == l)?;
            let text: SharedString = format!("◂ {}", d.message.lines().next().unwrap_or("")).into();
            let line = ts.shape_line(text.clone(), num_size, &[run(text.len(), t.err.opacity(0.9))], None);
            Some((point(origin_x - self.scroll.x + shaped.width() + px(24.), y_of(l)), line))
        });

        let cursor_quad = (focused && self.blink_on && (first..last).contains(&cursor_line)).then(|| {
            let shaped = &lines[cursor_line - first].1;
            let x = origin_x - self.scroll.x + shaped.x_for_index(cursor - self.buf.line_start(cursor_line));
            let color = if self.diagnostic.is_some() { t.err } else { t.acc };
            fill(Bounds::new(point(x, y_of(cursor_line) + px(1.)), size(px(2.), lh - px(2.))), color)
        });

        let lines = lines.into_iter().map(|(l, s)| (point(origin_x - self.scroll.x, y_of(l)), s)).collect();
        let gutter = Bounds::new(bounds.origin, size(gutter_w, bounds.size.height));
        let gutter_line = fill(Bounds::new(point(bounds.left() + gutter_w - px(1.), bounds.top()), size(px(1.), bounds.size.height)), t.line);
        self.bars = self.layout_bars(text_bounds.left());
        let (vbar, hbar) = self.bars;
        let bars = [vbar, hbar]
            .into_iter()
            .flatten()
            .map(|b| (fill(b.thumb, t.line2), window.insert_hitbox(b.track, HitboxBehavior::Normal)))
            .collect();
        Prepainted {
            bars,
            hitbox,
            text_bounds,
            gutter,
            gutter_line,
            rows,
            lines,
            numbers,
            marks,
            lens,
            backgrounds,
            cursor: cursor_quad,
            pop,
        }
    }

    fn render_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let t = cx.theme();
        let g = self.geo?;
        let m = self.menu.as_mut()?;
        if m.selected < m.top {
            m.top = m.selected;
        } else if m.selected >= m.top + MENU_ROWS {
            m.top = m.selected + 1 - MENU_ROWS;
        }
        let (top, selected, start) = (m.top, m.selected, m.replace.start);
        let partial = self.buf.text[m.replace.clone()].to_string();
        let rows: Vec<(usize, Candidate)> = m.items.iter().cloned().enumerate().skip(top).take(MENU_ROWS).collect();
        let current = m.items.get(selected).cloned();
        let lh = line_height();
        let x = g.origin_x - self.scroll.x + self.x_for(start, window);
        let y = g.bounds.top() + px(TOP_PAD) + lh * (self.buf.line_of(start) + 1) as f32 - self.scroll.y + px(4.);
        let tag = |k: CandidateKind| match k {
            CandidateKind::Column => ("col", t.typ, t.typ_s),
            CandidateKind::Table => ("tbl", t.acc, t.kw_s),
            CandidateKind::View => ("view", t.ok, t.view_s),
            CandidateKind::Schema => ("sch", t.tx2, t.hov),
            CandidateKind::Keyword => ("kw", t.kw, t.kw_s),
            CandidateKind::Cte => ("cte", t.acc, t.kw_s),
            CandidateKind::Function => ("fn", t.func, t.func_s),
            CandidateKind::Type => ("type", t.typ, t.typ_s),
            CandidateKind::Value => ("val", t.str, t.view_s),
        };
        let items = rows.into_iter().map(|(ix, c)| {
            let (label, fg, bg) = tag(c.kind);
            let on = ix == selected;
            let hits = match_positions(&c.label, &partial);
            let mut parts: Vec<(String, bool)> = Vec::new();
            for (i, ch) in c.label.chars().enumerate() {
                let hit = hits.contains(&i);
                match parts.last_mut() {
                    Some((s, h)) if *h == hit => s.push(ch),
                    _ => parts.push((ch.to_string(), hit)),
                }
            }
            div()
                .h(px(24.))
                .flex_none()
                .pl(px(4.))
                .pr(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .when(on, |d| d.bg(t.acc_s).border_1().border_color(t.acc).text_color(t.tx))
                .when(!on, |d| d.text_color(t.tx2))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                        this.accept_item(ix, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(30.))
                        .h(px(16.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(9.5))
                        .text_color(fg)
                        .bg(bg)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().flex().flex_row().children(
                    parts.into_iter().map(|(s, hit)| {
                        div().when(hit, |d| d.text_color(t.tx).font_weight(FontWeight::BOLD).border_b_1().border_color(t.acc)).child(s)
                    }),
                ))
                .child(div().flex_none().text_color(t.tx3).text_size(px(11.)).child(c.detail))
        });
        let detail = current.map(|c| {
            let (kind, sub) = match c.kind {
                CandidateKind::Column => ("column", c.detail.clone()),
                CandidateKind::Table | CandidateKind::View => {
                    let n = self.schema.resolve(Some(&c.detail), &c.label).map(|i| self.schema.columns_of(i).len());
                    let k = if c.kind == CandidateKind::Table { "table" } else { "view" };
                    (k, n.map(|n| format!("{n} columns")).unwrap_or_default())
                }
                CandidateKind::Schema => ("schema", String::new()),
                CandidateKind::Keyword => ("keyword", String::new()),
                CandidateKind::Cte => ("cte", String::new()),
                CandidateKind::Function => ("function", String::new()),
                CandidateKind::Type => ("type", c.detail.clone()),
                CandidateKind::Value => ("label", c.detail.clone()),
            };
            let qualified = match c.kind {
                CandidateKind::Table | CandidateKind::View if !c.detail.is_empty() => format!("{}.{}", c.detail, c.label),
                _ => c.label.clone(),
            };
            // A function: its overloads' signatures.
            let sigs: Vec<String> = match (c.kind, &self.schema.catalog) {
                (CandidateKind::Function, Some(cat)) => {
                    let all: Vec<_> = cat.functions_named(None, &c.label).collect();
                    let mut v: Vec<String> = all.iter().take(3).map(|f| format!("({}) → {}", f.args, f.result)).collect();
                    if all.len() > 3 {
                        v.push(format!("+{}", all.len() - 3));
                    }
                    v
                }
                _ => Vec::new(),
            };
            div()
                .w(px(220.))
                .flex_none()
                .px(px(12.))
                .py(px(10.))
                .flex()
                .flex_col()
                .gap(px(6.))
                .bg(t.bg)
                .border_l_1()
                .border_color(t.line)
                .text_size(px(11.5))
                .child(div().text_color(t.tx).font_weight(FontWeight::SEMIBOLD).child(qualified))
                .children(sigs.into_iter().map(|s| div().text_color(t.tx2).child(s)))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.))
                        .child(div().text_color(if c.kind == CandidateKind::Column { t.typ } else { t.tx2 }).child(sub))
                        .child(div().text_color(t.tx3).child(kind)),
                )
        });
        Some(
            deferred(
                anchored().position(point(x - px(46.), y)).snap_to_window_with_margin(px(8.)).child(
                    div()
                        .occlude()
                        .flex()
                        .flex_row()
                        .bg(t.pop)
                        .shadow(t.ring())
                        .overflow_hidden()
                        .font_family(crate::theme::mono())
                        .text_size(px(12.))
                        .line_height(px(16.))
                        .child(div().w(px(320.)).flex_none().p(px(4.)).flex().flex_col().children(items))
                        .children(detail),
                ),
            )
            .priority(1),
        )
    }
}

/// Character positions of `label` that match `partial` (prefix, else subsequence), for bold.
fn match_positions(label: &str, partial: &str) -> Vec<usize> {
    if partial.is_empty() {
        return Vec::new();
    }
    let l: Vec<char> = label.chars().flat_map(char::to_lowercase).collect();
    let p: Vec<char> = partial.chars().flat_map(char::to_lowercase).collect();
    if l.starts_with(&p) {
        return (0..p.len()).collect();
    }
    let mut out = Vec::new();
    let mut j = 0;
    for (i, c) in l.iter().enumerate() {
        if j < p.len() && *c == p[j] {
            out.push(i);
            j += 1;
        }
    }
    if j == p.len() { out } else { Vec::new() }
}

impl CodeEditor {
    /// `r` made safe for the buffer: within it, on character boundaries, start ≤ end. Ranges
    /// from the IME may predate an edit, and a panic in its callbacks aborts the app.
    fn clamp_range(&self, r: Range<usize>) -> Range<usize> {
        let t = &self.buf.text;
        let snap = |mut i: usize| {
            i = i.min(t.len());
            while !t.is_char_boundary(i) {
                i -= 1;
            }
            i
        };
        let (a, b) = (snap(r.start), snap(r.end));
        a.min(b)..a.max(b)
    }
}

impl EntityInputHandler for CodeEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.clamp_range(self.buf.from_utf16(range_utf16.start)..self.buf.from_utf16(range_utf16.end));
        actual_range.replace(self.buf.to_utf16(range.start)..self.buf.to_utf16(range.end));
        Some(self.buf.text[range].to_string())
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.buf.to_utf16(self.sel.start)..self.buf.to_utf16(self.sel.end),
            reversed: self.reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| self.buf.to_utf16(r.start)..self.buf.to_utf16(r.end))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.buf.from_utf16(r.start)..self.buf.from_utf16(r.end))
            .or(self.marked.clone())
            .unwrap_or(self.sel.clone());
        let range = self.clamp_range(range);
        self.insert_typed(range, &normalize_newlines(text), cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.buf.from_utf16(r.start)..self.buf.from_utf16(r.end))
            .or(self.marked.clone())
            .unwrap_or(self.sel.clone());
        let range = self.clamp_range(range);
        self.menu = None;
        self.replace(range.clone(), text, EditKind::Type, cx);
        self.marked = (!text.is_empty()).then(|| range.start..range.start + text.len());
        if let Some(r) = new_selected_range_utf16 {
            // Relative to the marked text, in UTF-16 units.
            let at = |u: usize| {
                let mut n = 0;
                for (i, c) in text.char_indices() {
                    if n >= u {
                        return range.start + i;
                    }
                    n += c.len_utf16();
                }
                range.start + text.len()
            };
            self.sel = at(r.start)..at(r.end);
            self.reversed = false;
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: Bounds<Pixels>,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let g = self.geo?;
        let start = self.buf.from_utf16(range_utf16.start);
        let l = self.buf.line_of(start);
        let x = g.origin_x - self.scroll.x + self.x_for(start, window);
        let y = g.bounds.top() + px(TOP_PAD) + line_height() * l as f32 - self.scroll.y;
        Some(Bounds::new(point(x, y), size(g.char_width, line_height())))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let off = self.offset_at(point, window);
        Some(self.buf.to_utf16(off))
    }
}

impl Render for CodeEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Scroll before the popup is placed so it anchors to this frame's positions.
        self.autoscroll_now(window);
        let menu = self.render_menu(window, cx);
        let signature = self.render_signature(window, cx);
        let hover = self.render_hover(window, cx);
        let t = cx.theme();
        let find_open = self.find.as_ref().is_some_and(|f| f.open);
        // The find bar sits beside the editor's key context (its field must not get the
        // editor's keys), docked at the top right.
        let bar = self.find.as_ref().filter(|f| f.open).map(|f| div().absolute().top(px(6.)).right(px(10.)).child(f.bar.clone()));
        // Transparent: the work surface paints the fill once under every region.
        let editor = div()
            .key_context(if find_open { "CodeEditor find" } else { CONTEXT })
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .text_color(t.tx)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::move_word_left))
            .on_action(cx.listener(Self::move_word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::move_line_start))
            .on_action(cx.listener(Self::move_line_end))
            .on_action(cx.listener(Self::select_line_start))
            .on_action(cx.listener(Self::select_line_end))
            .on_action(cx.listener(Self::move_doc_start))
            .on_action(cx.listener(Self::move_doc_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::select_page_up))
            .on_action(cx.listener(Self::select_page_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::delete_to_line_start))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::tab))
            .on_action(cx.listener(Self::outdent))
            .on_action(cx.listener(Self::toggle_comment))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::run_statement_action))
            .on_action(cx.listener(Self::run_script_action))
            .on_action(cx.listener(Self::run_from_here_action))
            .on_action(cx.listener(Self::show_completions))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::format_action))
            .on_action(cx.listener(Self::go_to_definition_action))
            .on_action(cx.listener(Self::peek_definition_action))
            .on_action(cx.listener(|e, _: &find::Find, w, cx| e.open_find(false, w, cx)))
            .on_action(cx.listener(|e, _: &find::FindReplace, w, cx| e.open_find(true, w, cx)))
            .on_action(cx.listener(|e, _: &find::FindNext, w, cx| e.find_step(true, w, cx)))
            .on_action(cx.listener(|e, _: &find::FindPrevious, w, cx| e.find_step(false, w, cx)))
            .child(EditorElement { editor: cx.entity() })
            .children(menu)
            .children(signature)
            .children(hover);
        div().relative().size_full().child(editor).children(bar)
    }
}

// ---------------------------------------------------------------------------------------------
// The element that paints the visible lines.

struct EditorElement {
    editor: Entity<CodeEditor>,
}

struct Prepainted {
    hitbox: Hitbox,
    /// The scrollbars' thumbs and tracks (the arrow cursor over them).
    bars: Vec<(PaintQuad, Hitbox)>,
    text_bounds: Bounds<Pixels>,
    gutter: Bounds<Pixels>,
    gutter_line: PaintQuad,
    /// Whole-width backgrounds (current line, run flash, failed statement).
    rows: Vec<PaintQuad>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    numbers: Vec<(Point<Pixels>, ShapedLine)>,
    marks: Vec<(Point<Pixels>, ShapedLine)>,
    lens: Option<(Point<Pixels>, ShapedLine)>,
    /// Text-area backgrounds (selection).
    backgrounds: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    pop: Option<EditorPop>,
}

/// This frame of the find indicator: its box at rest, its fill, its scale, and the text shaped
/// at that scale with the text's resting origin.
struct EditorPop {
    rest: Bounds<Pixels>,
    fill: Hsla,
    scale: f32,
    line: ShapedLine,
    origin: Point<Pixels>,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepainted;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        style.flex_grow = 1.;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepainted {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let theme = cx.theme();
        let find_focused = self.editor.read(cx).find_has_focus(window, cx);
        self.editor.update(cx, |editor, _| editor.layout(bounds, hitbox, find_focused, window, &theme))
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        p: &mut Prepainted,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.editor.read(cx).focus.clone();
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.editor.clone()), cx);
        let lh = line_height();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for q in p.rows.drain(..) {
                window.paint_quad(q);
            }
            window.paint_quad(p.gutter_line.clone());
        });
        window.with_content_mask(Some(ContentMask { bounds: p.text_bounds }), |window| {
            for q in p.backgrounds.drain(..) {
                window.paint_quad(q);
            }
            for (origin, line) in &p.lines {
                line.paint(*origin, lh, TextAlign::Left, None, window, cx).ok();
            }
            if let Some((origin, line)) = &p.lens {
                line.paint(*origin, lh, TextAlign::Left, None, window, cx).ok();
            }
            if let Some(pop) = &p.pop {
                crate::motion::paint_pop(window, cx, pop.rest, 0., pop.fill, pop.scale, Some((&pop.line, pop.origin, lh)));
            }
            if let Some(c) = p.cursor.take() {
                window.paint_quad(c);
            }
        });
        window.with_content_mask(Some(ContentMask { bounds: p.gutter }), |window| {
            for (origin, line) in p.numbers.iter().chain(&p.marks) {
                line.paint(*origin, lh, TextAlign::Left, None, window, cx).ok();
            }
        });
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (thumb, track) in p.bars.drain(..) {
                window.paint_quad(thumb);
                window.set_cursor_style(CursorStyle::Arrow, &track);
            }
        });

        let editor = self.editor.clone();
        let hitbox = p.hitbox.clone();
        window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left && hitbox.is_hovered(window) {
                editor.update(cx, |e, cx| e.mouse_down(ev, window, cx));
            }
        });
        let editor = self.editor.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || editor.read(cx).drag.is_none() {
                return;
            }
            editor.update(cx, |e, cx| {
                if ev.pressed_button == Some(MouseButton::Left) {
                    e.mouse_drag(ev.position, window, cx);
                } else {
                    e.drag = None;
                }
            });
        });
        // The lint under the mouse (its message shows); nothing to do without lints.
        let editor = self.editor.clone();
        let hitbox = p.hitbox.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
            let e = editor.read(cx);
            if phase != DispatchPhase::Bubble || e.drag.is_some() || (e.lints.is_empty() && e.hover.is_none()) {
                return;
            }
            let over = hitbox.is_hovered(window).then_some(ev.position);
            editor.update(cx, |e, cx| e.hover_at(over, window, cx));
        });
        let editor = self.editor.clone();
        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble && editor.read(cx).drag.is_some() {
                editor.update(cx, |e, _| e.drag = None);
            }
        });
        let editor = self.editor.clone();
        let hitbox = p.hitbox.clone();
        window.on_mouse_event(move |ev: &ScrollWheelEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hitbox.should_handle_scroll(window) {
                editor.update(cx, |e, cx| e.scroll_by(ev.delta.pixel_delta(line_height()), cx));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A three-line selection (from mid-line to mid-line) is one shape: rounded on its
    /// outline, square where the lines join; a lone run is rounded all round, and runs that
    /// do not touch are shapes of their own.
    #[test]
    fn selection_runs_join_into_one_shape() {
        let (first, mid, last) = ((40., 120.), (0., 90.), (0., 30.));
        // The first run sticks out right of the middle one; its bottom-left sits on it.
        assert_eq!(outline_corners(None, first, Some(mid)), [true, true, true, false]);
        assert_eq!(outline_corners(Some(first), mid, Some(last)), [true, false, true, false]);
        assert_eq!(outline_corners(Some(mid), last, None), [false, false, true, true]);
        // Same left edge on every line: square down the left side.
        assert_eq!(outline_corners(Some((0., 50.)), (0., 50.), Some((0., 80.))), [false, false, false, false]);
        assert_eq!(outline_corners(None, (10., 20.), None), [true; 4]);
        // From the end of a long line to the start of the next: they do not touch.
        assert_eq!(outline_corners(None, (100., 120.), Some((0., 30.))), [true; 4]);
        assert_eq!(outline_corners(Some((100., 120.)), (0., 30.), None), [true; 4]);
    }

    /// A scrollbar's thumb: as long as the share in view (never under the minimum), at the
    /// start when not scrolled and at the end when scrolled all the way.
    #[test]
    fn thumb_spans() {
        assert_eq!(thumb_span(400., 400., 400., 0.), (0., 200.));
        assert_eq!(thumb_span(400., 400., 400., 400.), (200., 200.));
        assert_eq!(thumb_span(400., 400., 400., 200.), (100., 200.));
        let (start, len) = thumb_span(400., 400., 1_000_000., 1_000_000.);
        assert_eq!((start, len), (400. - THUMB_MIN, THUMB_MIN));
        assert_eq!(thumb_span(100., 400., 0., 0.), (0., 100.));
    }

    #[test]
    fn line_index() {
        let mut b = Buffer::new("ab\ncd\n\nef");
        assert_eq!(b.line_count(), 4);
        assert_eq!((b.line_of(0), b.line_of(2), b.line_of(3), b.line_of(6), b.line_of(9)), (0, 0, 1, 2, 3));
        assert_eq!((b.line(1), b.line(2), b.line(3)), ("cd", "", "ef"));
        b.edit(1..4, "X\nY\nZ");
        assert_eq!(b.text(), "aX\nY\nZd\n\nef");
        assert_eq!(b.lines, vec![0, 3, 5, 8, 9]);
        b.edit(0..b.len(), "");
        assert_eq!(b.lines, vec![0]);
        assert_eq!(b.line_end(0), 0);
        let b = Buffer::new("x\n");
        assert_eq!((b.line_count(), b.line_of(2), b.line(1)), (2, 1, ""));
    }

    #[test]
    fn utf16() {
        let b = Buffer::new("café\n😀 x\né");
        // c a f é(2 bytes) \n 😀(2 units) space x \n é
        assert_eq!(b.to_utf16(0), 0);
        assert_eq!(b.to_utf16(5), 4); // after "café"
        assert_eq!(b.to_utf16(6), 5); // after "café\n"
        assert_eq!(b.to_utf16(10), 7); // after the emoji
        assert_eq!(b.to_utf16(b.len()), 11);
        for off in (0..=b.len()).filter(|&o| b.text().is_char_boundary(o)) {
            assert_eq!(b.from_utf16(b.to_utf16(off)), off);
        }
        let ascii = Buffer::new("select 1;\nselect 2;");
        assert_eq!((ascii.to_utf16(12), ascii.from_utf16(12), ascii.from_utf16(99)), (12, 12, ascii.len()));
    }

    #[test]
    fn graphemes_cross_lines() {
        let b = Buffer::new("é\nab");
        assert_eq!(b.next_boundary(0), 2);
        assert_eq!(b.next_boundary(2), 3);
        assert_eq!(b.prev_boundary(3), 2);
        assert_eq!(b.prev_boundary(2), 0);
        assert_eq!(b.next_boundary(b.len()), b.len());
    }

    #[test]
    fn word_boundaries() {
        let t = "select p.total,  x_1\n  from";
        assert_eq!(word_right(t, 0), 6);
        assert_eq!(word_right(t, 6), 8);
        assert_eq!(word_right(t, 8), 9);
        assert_eq!(word_right(t, 9), 14);
        assert_eq!(word_right(t, 14), 15);
        assert_eq!(word_right(t, 15), 20);
        assert_eq!(word_right(t, 20), t.len());
        assert_eq!(word_left(t, t.len()), 23);
        assert_eq!(word_left(t, 23), 17);
        assert_eq!(word_left(t, 9), 8);
        assert_eq!(word_left(t, 6), 0);
        assert_eq!(word_at(t, 10), 9..14);
        assert_eq!(word_at(t, 14), 9..14);
        assert_eq!(word_at(t, 3), 0..6);
        assert_eq!(word_at("café x", 3), 0..5);
        assert_eq!(word_at("a\n\nb", 2), 2..2);
        // A stale offset (the text changed under a double click) never panics.
        assert_eq!(word_at("", 5), 0..0);
        assert_eq!(word_at("café", 4), 0..5);
    }

    fn edits_applied(buf: &Buffer, edits: &Edits) -> String {
        let mut b = Buffer::new(buf.text());
        for (r, s) in edits.iter().rev() {
            b.edit(r.clone(), s);
        }
        b.text().to_string()
    }

    #[test]
    fn indentation() {
        let b = Buffer::new("a\n\n  b\n\tc");
        assert_eq!(edits_applied(&b, &indent_edits(&b, 0..=3)), "    a\n\n      b\n    \tc");
        assert_eq!(edits_applied(&b, &outdent_edits(&b, 0..=3)), "a\n\nb\nc");
        let b = Buffer::new("      x");
        assert_eq!(edits_applied(&b, &outdent_edits(&b, 0..=0)), "  x");
        assert_eq!(indent_of("  \tx "), "  \t");
    }

    #[test]
    fn comment_toggling() {
        let b = Buffer::new("  select 1\n\n    from t");
        let on = edits_applied(&b, &comment_edits(&b, 0..=2));
        assert_eq!(on, "  -- select 1\n\n  --   from t");
        let b2 = Buffer::new(&on);
        assert_eq!(edits_applied(&b2, &comment_edits(&b2, 0..=2)), "  select 1\n\n    from t");
        let mixed = Buffer::new("-- a\nb");
        assert_eq!(edits_applied(&mixed, &comment_edits(&mixed, 0..=1)), "-- -- a\n-- b");
        let bare = Buffer::new("--a");
        assert_eq!(edits_applied(&bare, &comment_edits(&bare, 0..=0)), "a");
    }

    #[test]
    fn offsets_through_edits() {
        let edits: Edits = vec![(0..0, "--".into()), (5..7, "X".into())];
        assert_eq!(map_offset(0, &edits, false), 0);
        assert_eq!(map_offset(0, &edits, true), 2);
        assert_eq!(map_offset(3, &edits, false), 5);
        assert_eq!(map_offset(6, &edits, false), 7);
        assert_eq!(map_offset(6, &edits, true), 8);
        assert_eq!(map_offset(7, &edits, false), 8);
        assert_eq!(map_offset(9, &edits, true), 10);
    }

    fn typed(b: &mut Buffer, h: &mut History, at: usize, s: &str, kind: EditKind, now: Instant) {
        let old = String::new();
        b.edit(at..at, s);
        h.push(vec![Edit { at, old, new: s.into() }], kind, at..at, at + s.len()..at + s.len(), now);
    }

    #[test]
    fn undo_coalescing() {
        let mut b = Buffer::new("");
        let mut h = History::default();
        let t = Instant::now();
        for (i, c) in "select".chars().enumerate() {
            typed(&mut b, &mut h, i, &c.to_string(), EditKind::Type, t);
        }
        typed(&mut b, &mut h, 6, " 1", EditKind::Type, t + Duration::from_millis(200));
        assert_eq!(h.undo.len(), 1);
        // A pause starts a new step, and so does a seal (cursor moved).
        typed(&mut b, &mut h, 8, ";", EditKind::Type, t + Duration::from_secs(5));
        h.seal();
        typed(&mut b, &mut h, 9, "\n", EditKind::Type, t + Duration::from_secs(5));
        assert_eq!(h.undo.len(), 3);
        // Backspaces coalesce together.
        let now = t + Duration::from_secs(6);
        for at in [8, 7] {
            let old = b.text()[at..at + 1].to_string();
            b.edit(at..at + 1, "");
            h.push(vec![Edit { at, old, new: String::new() }], EditKind::Delete, at + 1..at + 1, at..at, now);
        }
        assert_eq!(b.text(), "select \n");
        assert_eq!(h.undo.len(), 4);
        assert_eq!(h.undo(&mut b), Some(9..9));
        assert_eq!(b.text(), "select 1;\n");
        h.undo(&mut b);
        h.undo(&mut b);
        assert_eq!(b.text(), "select 1");
        h.undo(&mut b);
        assert_eq!(b.text(), "");
        assert!(h.undo(&mut b).is_none());
        h.redo(&mut b);
        h.redo(&mut b);
        assert_eq!(b.text(), "select 1;");
        // A new edit drops the redo stack.
        typed(&mut b, &mut h, 0, "x", EditKind::Other, now);
        assert!(h.redo(&mut b).is_none());
    }

    #[test]
    fn ime_replacement_coalesces() {
        let mut b = Buffer::new("");
        let mut h = History::default();
        let t = Instant::now();
        typed(&mut b, &mut h, 0, "a", EditKind::Type, t);
        typed(&mut b, &mut h, 1, "´", EditKind::Type, t);
        b.edit(1..3, "é");
        h.push(vec![Edit { at: 1, old: "´".into(), new: "é".into() }], EditKind::Type, 3..3, 3..3, t);
        assert_eq!(b.text(), "aé");
        assert_eq!(h.undo.len(), 1);
        h.undo(&mut b);
        assert_eq!(b.text(), "");
    }

    /// Incremental re-lexing must always match lexing the whole text.
    #[test]
    fn relex_matches_full() {
        let base = "select 'a;b', \"Q\" from t -- c\n/* x /* y */ */ $f$ body; $f$ where a::int >= 1.5e3;\n";
        let mut b = Buffer::new(&base.repeat(80));
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut rnd = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let pieces = ["'", "\"", "--", "/*", "*/", "$f$", "$$", " ", "\n", "x", "1", ";", "é", "e3", "."];
        for _ in 0..3000 {
            let len = b.len();
            let mut s = rnd(len + 1);
            while !b.text().is_char_boundary(s) {
                s -= 1;
            }
            let mut e = (s + rnd(4)).min(len);
            while !b.text().is_char_boundary(e) {
                e += 1;
            }
            let ins = if rnd(3) == 0 { "" } else { pieces[rnd(pieces.len())] };
            b.edit(s..e, ins);
            assert_eq!(b.tokens, tokenize(b.text()), "after replacing {s}..{e} with {ins:?}");
            let mut lines = vec![0];
            lines.extend(b.text().match_indices('\n').map(|(i, _)| i + 1));
            assert_eq!(b.lines, lines);
            assert_eq!(b.non_ascii, count_non_ascii(b.text()));
        }
    }

    /// Replace All: every match, `$1` groups expanded, in one buffer edit that one undo reverts
    /// (and one redo brings back).
    #[test]
    fn replace_all_is_one_undo_step() {
        use dataman_core::find::FindOptions;
        let text = "select p.id, p.total\n  from orders p\n where p.id > 1;";
        let mut b = Buffer::new(text);
        let mut h = History::default();
        let m = Matcher::new(r"p\.(\w+)", FindOptions { regex: true, ..Default::default() }).unwrap().unwrap();
        let edits = m.replace_all(b.text(), "o.$1");
        assert_eq!(edits.len(), 3);
        let e = splice_edits(&mut b, &edits).unwrap();
        assert_eq!(b.text(), "select o.id, o.total\n  from orders p\n where o.id > 1;");
        assert_eq!(b.tokens, tokenize(b.text()));
        h.seal();
        h.push(vec![e], EditKind::Other, 0..0, 0..0, Instant::now());
        h.seal();
        assert_eq!(h.undo.len(), 1);
        h.undo(&mut b);
        assert_eq!(b.text(), text);
        assert!(h.undo(&mut b).is_none(), "one step");
        h.redo(&mut b);
        assert_eq!(b.text(), "select o.id, o.total\n  from orders p\n where o.id > 1;");
        // Nothing to replace: no edit.
        assert!(splice_edits(&mut b, &[]).is_none());
    }

    #[test]
    fn the_current_match_follows_the_selection() {
        let ms = vec![2..4, 10..12, 20..25];
        assert_eq!(current_match(&ms, &(10..12)), Some(1), "on a match");
        assert_eq!(current_match(&ms, &(5..5)), Some(1), "the next one after a caret");
        assert_eq!(current_match(&ms, &(10..11)), Some(1));
        assert_eq!(current_match(&ms, &(30..30)), Some(0), "around the end");
        assert_eq!(current_match(&[], &(0..0)), None);
    }

    #[test]
    fn underline_splits_runs() {
        let run = TextRun {
            len: 6,
            font: mono(),
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let mut runs = vec![run.clone(), TextRun { len: 4, ..run }];
        let u = UnderlineStyle { thickness: px(1.), color: None, wavy: true };
        underline_runs(&mut runs, 10, &(14..17), u);
        let lens: Vec<_> = runs.iter().map(|r| (r.len, r.underline.is_some())).collect();
        assert_eq!(lens, vec![(4, false), (2, true), (1, true), (3, false)]);
    }
}
