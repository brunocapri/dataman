use std::time::Instant;

use dataman_core::jsontree::{self, Doc, Node, Row, RowKey};
use dataman_core::value::Kind;
use gpui::{
    App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, KeyBinding, SharedString, StyledText, Task, TextRun,
    Window, actions, div, font, prelude::*,
};

use crate::assets::{Icon, icon};
use crate::editor::{CodeEditor, EditorEvent};
use crate::theme::{ActiveTheme, Theme, px};
use crate::ui::{self, Btn};

actions!(value_sheet, [CloseValue, CopyValue]);

pub const CONTEXT: &str = "ValueSheet";

const MAX_LINES: usize = 2_000;
const LINE: f32 = 20.;

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    // Space closes the sheet like it opened it, except while typing in its editor.
    let reading = Some("ValueSheet && !CodeEditor");
    cx.bind_keys([
        KeyBinding::new("escape", CloseValue, c),
        KeyBinding::new("shift-space", CloseValue, reading),
        KeyBinding::new("space", CloseValue, reading),
        KeyBinding::new("cmd-c", CopyValue, c),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Json,
    List,
}

/// PostgreSQL names array types `_int4` (the driver's name) or `int4[]` (catalog text).
pub fn shape_of(kind: Kind, type_name: &str) -> Option<Shape> {
    match kind {
        // Text holding JSON (SQL Server's nvarchar, a text column): viewable when it parses.
        Kind::Json | Kind::Text => Some(Shape::Json),
        Kind::Other if type_name.starts_with('_') || type_name.ends_with("[]") => Some(Shape::List),
        _ => None,
    }
}

pub fn viewable(kind: Kind, type_name: &str, value: &str) -> bool {
    let v = value.trim_start();
    match shape_of(kind, type_name) {
        Some(Shape::Json) if kind == Kind::Text => (v.starts_with('{') || v.starts_with('[')) && jsontree::parse(v).is_ok_and(|n| n.is_container()),
        Some(Shape::Json) => v.starts_with('{') || v.starts_with('['),
        Some(Shape::List) => v.starts_with('{') || v.starts_with('['),
        None => false,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValueCell {
    pub column: String,
    pub col: usize,
    pub type_name: String,
    pub kind: Kind,
    pub row: usize,
    pub value: String,
}

pub enum ValueEvent {
    Close,
    /// The edited JSON, for the cell's pending edit.
    Apply(String),
}

pub struct ValueView {
    focus: FocusHandle,
    cell: ValueCell,
    shape: Shape,
    doc: Option<Doc>,
    rows: Vec<Row>,
    pretty: String,
    text: bool,
    copied_at: Option<Instant>,
    _copied: Option<Task<()>>,
    /// Editable cells: the Text view is an editor, Apply makes it the cell's pending edit.
    edit: Option<Entity<CodeEditor>>,
    error: Option<String>,
    _edit_sub: Option<gpui::Subscription>,
}

impl EventEmitter<ValueEvent> for ValueView {}

impl Focusable for ValueView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ValueView {
    pub fn new(cell: ValueCell, editable: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let shape = shape_of(cell.kind, &cell.type_name).unwrap_or(Shape::Json);
        let root = match shape {
            Shape::Json => jsontree::parse(&cell.value).ok(),
            Shape::List => jsontree::from_pg_array(&cell.value),
        };
        let pretty = match (&root, shape) {
            (Some(n), Shape::Json) => n.pretty(),
            _ => cell.value.clone(),
        };
        let text = root.is_none();
        let (edit, sub) = if editable && shape == Shape::Json && root.is_some() {
            let pretty = pretty.clone();
            let e = cx.new(|cx| {
                let mut e = CodeEditor::new(window, cx).plain();
                e.set_text(&pretty, cx);
                e
            });
            let sub = cx.subscribe(&e, |v: &mut ValueView, _, ev: &EditorEvent, cx| match ev {
                EditorEvent::Changed => v.check(cx),
                EditorEvent::Run(_) | EditorEvent::RunAll(_) => v.apply(cx),
                _ => {}
            });
            (Some(e), Some(sub))
        } else {
            (None, None)
        };
        let mut v = ValueView { focus, cell, shape, doc: root.map(Doc::new), rows: Vec::new(), pretty, text, copied_at: None, _copied: None, edit, error: None, _edit_sub: sub };
        v.refresh_rows();
        v
    }

    #[cfg(feature = "checks")]
    pub fn demo_editor(&self) -> Option<(Entity<CodeEditor>, Option<String>)> {
        Some((self.edit.clone()?, self.error.clone()))
    }

    fn edited(&self, cx: &App) -> Option<String> {
        let text = self.edit.as_ref()?.read(cx).text().to_string();
        (text != self.pretty).then_some(text)
    }

    fn check(&mut self, cx: &mut Context<Self>) {
        self.error = self.edited(cx).and_then(|t| jsontree::parse(&t).err().map(|e| e.to_string()));
        cx.notify();
    }

    /// The edit, valid JSON, written as the stored value was (one line, or indented).
    fn apply(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.edited(cx) else { return };
        match jsontree::parse(&text) {
            Ok(n) => {
                let out = if self.cell.value.contains('\n') { n.pretty() } else { n.compact() };
                cx.emit(ValueEvent::Apply(out));
            }
            Err(e) => {
                self.error = Some(e.to_string());
                cx.notify();
            }
        }
    }

    fn refresh_rows(&mut self) {
        self.rows = self.doc.as_ref().map(|d| d.rows().into_iter().skip(1).collect()).unwrap_or_default();
    }

    fn toggle(&mut self, path: &[usize], cx: &mut Context<Self>) {
        if let Some(d) = &mut self.doc {
            d.toggle(path);
        }
        self.refresh_rows();
        cx.notify();
    }

    fn all(&mut self, open: bool, cx: &mut Context<Self>) {
        if let Some(d) = &mut self.doc {
            if open { d.expand_all() } else { d.collapse_all() }
        }
        self.refresh_rows();
        cx.notify();
    }

    fn set_text(&mut self, text: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.text = text || self.doc.is_none();
        if self.text
            && let Some(e) = &self.edit
        {
            window.focus(&e.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn copy_text(&self) -> &str {
        if self.text { &self.pretty } else { &self.cell.value }
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.copy_text().to_string()));
        let start = Instant::now();
        self.copied_at = Some(start);
        let redraws = crate::motion::copied_redraws(cx.reduce_motion());
        self._copied = Some(cx.spawn(async move |v, cx| {
            for at in redraws {
                cx.background_executor().timer(at.saturating_sub(start.elapsed())).await;
                if v.update(cx, |_, cx| cx.notify()).is_err() {
                    return;
                }
            }
        }));
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub fn demo_all(&mut self, open: bool, cx: &mut Context<Self>) {
        self.all(open, cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_text(&mut self, text: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.set_text(text, window, cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_state(&self) -> (String, bool, Vec<String>) {
        let lines = if self.text {
            self.pretty.lines().map(str::to_string).collect()
        } else {
            self.rows.iter().map(|r| format!("{}{}{}", "  ".repeat(r.depth - 1), key_text(r).map(|k| format!("{k}: ")).unwrap_or_default(), self.leaf_text(r))).collect()
        };
        (self.cell.column.clone(), !self.text, lines)
    }

    fn leaf_text(&self, r: &Row) -> String {
        if r.container || self.shape == Shape::Json {
            return r.value.clone();
        }
        match self.doc.as_ref().and_then(|d| d.root.get(&r.path)) {
            Some(Node::Null) => "NULL".into(),
            Some(n) => n.edit_text(),
            None => r.value.clone(),
        }
    }

    fn meta(&self) -> String {
        let bytes = self.cell.value.len();
        let size = if bytes >= 10_000 { format!("{:.1} KB", bytes as f64 / 1024.) } else { format!("{bytes} B") };
        let what = match (&self.doc, self.shape) {
            (Some(d), Shape::Json) => match &d.root {
                Node::Object(o) => format!("{} key{}", o.len(), plural(o.len())),
                n => format!("{} item{}", n.len(), plural(n.len())),
            },
            (Some(d), Shape::List) => format!("{} item{}", d.root.len(), plural(d.root.len())),
            (None, Shape::Json) => "not valid JSON".into(),
            (None, Shape::List) => "not an array literal".into(),
        };
        format!("{size} · {what}")
    }

    fn render_tree(&self, t: &Theme, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let hov = t.hov;
        self.rows
            .iter()
            .take(MAX_LINES)
            .enumerate()
            .map(|(i, r)| {
                let path = r.path.clone();
                let null = !r.container && self.doc.as_ref().and_then(|d| d.root.get(&r.path)).is_some_and(|n| matches!(n, Node::Null));
                let color = if r.container { t.tx3 } else if null && self.shape == Shape::List { t.null } else { json_color(t, r.type_name) };
                div()
                    .id(("vv-row", i))
                    .min_h(px(LINE))
                    .pl(px(12. + (r.depth - 1) as f32 * 16.))
                    .pr(px(14.))
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(6.))
                    .hover(move |s| s.bg(hov))
                    .child(
                        div()
                            .w(px(12.))
                            .h(px(LINE))
                            .flex_none()
                            .flex()
                            .items_center()
                            .when(r.container, |d| d.child(ui::disclosure(("value-chev", i), r.expanded, 10., t.tx3))),
                    )
                    .children(key_text(r).map(|k| {
                        let index = matches!(r.key, Some(RowKey::Index(_)));
                        div().flex_none().text_color(if index { t.tx4 } else { t.typ }).child(format!("{k}:"))
                    }))
                    .child(div().flex_1().min_w_0().text_color(color).when(null && self.shape == Shape::List, |d| d.italic()).child(self.leaf_text(r)))
                    .when(r.container, |d| d.cursor_pointer().on_click(cx.listener(move |v, _, _, cx| v.toggle(&path, cx))))
                    .into_any_element()
            })
            .collect()
    }

    fn render_text(&self, t: &Theme) -> Vec<gpui::AnyElement> {
        let json = self.shape == Shape::Json && self.doc.is_some();
        self.pretty
            .lines()
            .take(MAX_LINES)
            .enumerate()
            .map(|(i, l)| {
                let body = if json { json_line(t, l) } else { div().text_color(t.tx).child(l.to_string()).into_any_element() };
                div()
                    .flex()
                    .flex_row()
                    .min_h(px(LINE))
                    .child(div().w(px(44.)).flex_none().pr(px(12.)).text_right().text_color(t.tx4).child((i + 1).to_string()))
                    .child(div().flex_1().min_w_0().pr(px(14.)).child(body))
                    .into_any_element()
            })
            .collect()
    }
}

fn type_label(type_name: &str) -> String {
    match type_name.strip_prefix('_') {
        Some(elem) => format!("{elem}[]"),
        None => type_name.to_string(),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn key_text(r: &Row) -> Option<String> {
    match &r.key {
        Some(RowKey::Key(k)) => Some(k.clone()),
        Some(RowKey::Index(i)) => Some(i.to_string()),
        None => None,
    }
}

fn json_color(t: &Theme, type_name: &str) -> Hsla {
    match type_name {
        "string" => t.str,
        "number" => t.num,
        "bool" => t.kw,
        _ => t.tx3,
    }
}

fn json_line(t: &Theme, line: &str) -> gpui::AnyElement {
    let base = TextRun { len: 0, font: font(crate::theme::mono()), color: t.tx3, background_color: None, underline: None, strikethrough: None };
    let mut runs: Vec<TextRun> = Vec::new();
    let mut push = |len: usize, color: Hsla| {
        if len > 0 {
            runs.push(TextRun { len, color, ..base.clone() });
        }
    };
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        if c == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            let is_key = line[i..].trim_start().starts_with(':');
            push(i - start, if is_key { t.typ } else { t.str });
        } else if c == b'-' || c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_digit() || b"+-.eE".contains(&b[i])) {
                i += 1;
            }
            push(i - start, t.num);
        } else if c.is_ascii_alphabetic() {
            while i < b.len() && b[i].is_ascii_alphabetic() {
                i += 1;
            }
            push(i - start, if &line[start..i] == "null" { t.tx3 } else { t.kw });
        } else {
            // Punctuation and spaces, up to the next token (whole UTF-8 characters: none of
            // the bytes stopped at is a continuation byte).
            while i < b.len() && !(b[i] == b'"' || b[i] == b'-' || b[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            push(i - start, t.tx3);
        }
    }
    div().child(StyledText::new(SharedString::from(line.to_string())).with_runs(runs)).into_any_element()
}

impl Render for ValueView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let c = &self.cell;
        let tree_ok = self.doc.is_some();
        let text = self.text;
        let header = ui::modal_header_with(
            &t,
            Some(icon(Icon::View).size(px(15.)).text_color(t.tx3).into_any_element()),
            c.column.clone(),
            Some(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(11.5))
                    .child(div().px(px(6.)).py(px(1.)).bg(t.hov).text_color(t.tx2).child(type_label(&c.type_name)))
                    .child(div().text_color(t.tx3).child(format!("row {}", crate::grid::ui_count(c.row + 1))))
                    .into_any_element(),
            ),
            ui::modal_close(&t, "vv-close").on_click(cx.listener(|_, _, _, cx| cx.emit(ValueEvent::Close))),
        );
        let tree_label = if self.shape == Shape::List { "Items" } else { "Tree" };
        let toolbar = div()
            .h(px(44.))
            .flex_none()
            .px(px(20.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .child(
                ui::segmented(&t)
                    .w(px(132.))
                    .h(px(26.))
                    .child(ui::segment(&t, "vv-tree", tree_label, !text).when(!tree_ok, |d| d.opacity(0.4)).on_click(cx.listener(|v, _, window, cx| v.set_text(false, window, cx))))
                    .child(ui::segment(&t, "vv-text", "Text", text).on_click(cx.listener(|v, _, window, cx| v.set_text(true, window, cx)))),
            )
            .when(!text && self.rows.iter().any(|r| r.container), |d| {
                d.child(ui::button(&t, "vv-expand", Btn::Ghost).h(px(24.)).text_size(px(11.5)).child("Expand all").on_click(cx.listener(|v, _, _, cx| v.all(true, cx))))
                    .child(ui::button(&t, "vv-collapse", Btn::Ghost).h(px(24.)).text_size(px(11.5)).child("Collapse all").on_click(cx.listener(|v, _, _, cx| v.all(false, cx))))
            })
            .child(div().flex_1())
            .child(div().text_size(px(11.)).text_color(t.tx3).child(self.meta()));
        let editing = text && self.edit.is_some();
        let lines = if editing { Vec::new() } else if text { self.render_text(&t) } else { self.render_tree(&t, cx) };
        let total = if text { self.pretty.lines().count() } else { self.rows.len() };
        let more = (total > MAX_LINES).then(|| {
            div()
                .px(px(14.))
                .pt(px(6.))
                .text_size(px(11.))
                .text_color(t.tx3)
                .child(format!("first {} of {} lines · Copy takes the whole value", crate::grid::ui_count(MAX_LINES), crate::grid::ui_count(total)))
        });
        let empty = (lines.is_empty() && !text).then(|| div().px(px(14.)).text_color(t.tx3).child(if self.shape == Shape::List { "no items" } else { "empty" }));
        let body = div()
            .id("vv-body")
            .mx(px(20.))
            .min_h(px(96.))
            .max_h(px(460.))
            .bg(t.card)
            .border_1()
            .border_color(t.line)
            .py(px(8.))
            .font_family(crate::theme::mono())
            .text_size(px(12.))
            .line_height(px(LINE))
            .when(editing, |d| d.h(px(380.)).py(px(0.)))
            .when(!editing, |d| d.overflow_y_scroll())
            .when_some(self.edit.clone().filter(|_| editing), |d, e| d.child(e))
            .children(lines)
            .children(empty)
            .children(more);
        let changed = self.edited(cx).is_some();
        let note = self.error.clone().map(|e| div().mx(px(20.)).mt(px(8.)).text_size(px(11.5)).text_color(t.err).child(format!("Not valid JSON: {e}")));
        let copied = self.copied_at.and_then(|at| crate::motion::copied(at, Instant::now(), cx.reduce_motion()));
        let copy_tip = if text { "The formatted text" } else { "The value as stored" };
        let footer = ui::modal_footer(&t)
            .mt(px(16.))
            .child(div().flex_1())
            .child(
                div()
                    .relative()
                    .child(
                        ui::button(&t, "vv-copy", Btn::Secondary)
                            .h(px(28.))
                            .px(px(12.))
                            .when(copied.is_some(), |d| d.invisible())
                            .child(icon(Icon::Copy).size(px(13.)).text_color(t.tx2))
                            .child("Copy")
                            .child(div().text_size(px(10.5)).opacity(0.6).child("⌘C"))
                            .tooltip(ui::tooltip("Copy", Some(copy_tip), None))
                            .on_click(cx.listener(|v, _, _, cx| v.copy(cx))),
                    )
                    .children(copied.map(|a| {
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_center()
                            .gap(px(4.))
                            .text_size(px(11.5))
                            .text_color(t.ok)
                            .opacity(a)
                            .child(icon(Icon::Check).size(px(12.)).text_color(t.ok))
                            .child("Copied")
                    })),
            )
            .child(
                ui::button(&t, "vv-done", if changed { Btn::Ghost } else { Btn::Primary })
                    .h(px(28.))
                    .px(px(12.))
                    .when(!changed, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .child(if changed { "Discard" } else { "Done" })
                    .child(div().text_size(px(10.5)).font_weight(FontWeight::NORMAL).opacity(0.75).child("esc"))
                    // Under the eye that opened it, the double click's second press is no "Done".
                    .on_click(cx.listener(|_, ev: &gpui::ClickEvent, _, cx| {
                        if ev.click_count() < 2 {
                            cx.emit(ValueEvent::Close);
                        }
                    })),
            )
            .when(self.edit.is_some(), |d| {
                let ok = changed && self.error.is_none();
                d.child(
                    ui::button(&t, "vv-apply", Btn::Primary)
                        .h(px(28.))
                        .px(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .when(!ok, |d| d.opacity(0.5))
                        .child("Apply")
                        .child(div().text_size(px(10.5)).font_weight(FontWeight::NORMAL).opacity(0.75).child("⌘↩"))
                        .tooltip(ui::tooltip("Apply", Some("A pending edit of the cell: commit it with the grid's other edits"), None))
                        .on_click(cx.listener(|v, _, _, cx| v.apply(cx))),
                )
            });
        let card = ui::modal(&t, "value-sheet").w(px(720.)).child(header).child(toolbar).child(body).children(note).child(footer);
        div()
            .id("value-backdrop")
            .absolute()
            .top(px(crate::theme::metrics::TITLE_H))
            .left_0()
            .right_0()
            .bottom_0()
            .bg(t.scrim)
            .flex()
            .items_center()
            .justify_center()
            .key_context("ValueSheet Modal")
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &CloseValue, _, cx| cx.emit(ValueEvent::Close)))
            .on_action(cx.listener(|v, _: &CopyValue, _, cx| v.copy(cx)))
            // Not the second press of a double click on the eye that opened it.
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|_, ev: &gpui::MouseDownEvent, _, cx| {
                cx.stop_propagation();
                if ev.click_count < 2 {
                    cx.emit(ValueEvent::Close);
                }
            }))
            .child(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_gets_the_eye() {
        assert_eq!(shape_of(Kind::Json, "jsonb"), Some(Shape::Json));
        assert_eq!(shape_of(Kind::Json, "json"), Some(Shape::Json));
        assert_eq!(shape_of(Kind::Other, "_int4"), Some(Shape::List));
        assert_eq!(shape_of(Kind::Other, "text[]"), Some(Shape::List));
        assert_eq!(shape_of(Kind::Text, "nvarchar"), Some(Shape::Json), "text holding JSON gets the eye");
        assert_eq!(shape_of(Kind::Other, "uuid"), None);
        assert!(viewable(Kind::Json, "jsonb", r#" {"a": 1}"#));
        assert!(viewable(Kind::Json, "jsonb", "[1, 2]"));
        assert!(!viewable(Kind::Json, "jsonb", "42"), "a JSON scalar has nothing to unfold");
        assert!(!viewable(Kind::Json, "jsonb", "\"x\""));
        assert!(viewable(Kind::Other, "_text", "{a,b}"));
        assert!(viewable(Kind::Text, "text", r#"{"a": 1}"#));
        assert!(viewable(Kind::Text, "nvarchar", r#"[{"ü": "ção"}]"#));
        assert!(!viewable(Kind::Text, "nvarchar", "{not json"), "only what parses");
        assert!(!viewable(Kind::Text, "nvarchar", "[note]"));
        assert!(!viewable(Kind::Text, "varchar", "42"));
        assert_eq!(type_label("_int8"), "int8[]");
        assert_eq!(type_label("jsonb"), "jsonb");
    }
}
