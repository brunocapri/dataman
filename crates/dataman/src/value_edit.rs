use std::sync::Arc;

use chrono::{NaiveDate, NaiveTime};
use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, KeyDownEvent,
    MouseButton, Subscription, Window, actions, div, prelude::*,
};
use crate::theme::px;
use dataman_core::edit::CellValue;
use dataman_core::jsontree::{self, Doc, Node, Path, RowKey};
use dataman_core::value::Kind;

use crate::assets::Icon;
use crate::editor::{CodeEditor, EditorEvent};
use crate::input::{InputEvent, TextInput};
use crate::theme::{ActiveTheme, Theme, mono};
use crate::ui::{self, Btn};

actions!(value_editor, [ApplyValue, CancelValue, Up, Down, Left, Right, Confirm, RemoveNode, ToggleNullValue]);

pub const CONTEXT: &str = "ValueEditor";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        crate::keymap::kb("cmd-enter", ApplyValue, c),
        crate::keymap::kb("escape", CancelValue, c),
        crate::keymap::kb("cmd-shift-n", ToggleNullValue, c),
    ]);
    let b = Some("ValueBody");
    cx.bind_keys([
        crate::keymap::kb("up", Up, b),
        crate::keymap::kb("down", Down, b),
        crate::keymap::kb("left", Left, b),
        crate::keymap::kb("right", Right, b),
        crate::keymap::kb("enter", Confirm, b),
        crate::keymap::kb("space", Confirm, b),
        crate::keymap::kb("backspace", RemoveNode, b),
    ]);
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    Bool,
    Enum(Arc<Vec<String>>),
    Json,
    LongText,
}

pub const LONG_TEXT: usize = 80;

pub fn mode_for(kind: Kind, enum_labels: Option<&Arc<Vec<String>>>, value: Option<&str>) -> Option<Mode> {
    match kind {
        Kind::Bool => Some(Mode::Bool),
        Kind::Json => Some(Mode::Json),
        // Dates and times are typed as text (checked when the edits are committed).
        Kind::Date | Kind::Time | Kind::Timestamp | Kind::TimestampTz => None,
        _ => {
            if let Some(l) = enum_labels.filter(|l| !l.is_empty()) {
                return Some(Mode::Enum(l.clone()));
            }
            let long = value.is_some_and(|v| v.contains('\n') || v.chars().count() > LONG_TEXT);
            (long && matches!(kind, Kind::Text | Kind::Other)).then_some(Mode::LongText)
        }
    }
}

/// A date or time typed in a cell, as the servers read it: `YYYY-MM-DD`, `HH:MM[:SS[.fff]]`,
/// both with a space or a `T` between them, and an offset after a time (`Z`, `±HH[:MM]`)
/// where the type has a time zone. MySQL's zero date passes (its server decides).
pub fn date_text_ok(kind: Kind, s: &str) -> bool {
    let s = s.trim();
    let date = |p: &str| p == "0000-00-00" || NaiveDate::parse_from_str(p, "%Y-%m-%d").is_ok();
    let time = |p: &str| ["%H:%M:%S%.f", "%H:%M"].iter().any(|f| NaiveTime::parse_from_str(p, f).is_ok());
    let zoned = |p: &str| -> bool {
        if let Some(t) = p.strip_suffix('Z') {
            return time(t);
        }
        match p.rfind(['+', '-']) {
            Some(at) if at > 0 => {
                let off = &p[at + 1..];
                let ok = matches!(off.len(), 2 | 4 | 5) && off.bytes().enumerate().all(|(i, b)| b.is_ascii_digit() || (i == 2 && b == b':' && off.len() == 5));
                ok && time(&p[..at])
            }
            _ => time(p),
        }
    };
    match kind {
        Kind::Date => date(s),
        Kind::Time => time(s),
        Kind::Timestamp | Kind::TimestampTz => {
            if matches!(s, "infinity" | "-infinity") {
                return true;
            }
            match s.split_once([' ', 'T']) {
                None => date(s),
                Some((d, t)) if kind == Kind::TimestampTz => date(d) && zoned(t.trim()),
                Some((d, t)) => date(d) && time(t.trim()),
            }
        }
        _ => true,
    }
}

/// What a date column takes, for the message about a value it does not.
pub fn date_format(kind: Kind) -> &'static str {
    match kind {
        Kind::Date => "YYYY-MM-DD",
        Kind::Time => "HH:MM:SS",
        Kind::TimestampTz => "YYYY-MM-DD HH:MM:SS+HH:MM",
        _ => "YYYY-MM-DD HH:MM:SS",
    }
}

pub enum ValueEvent {
    Commit,
    Cancel,
}

struct JsonState {
    doc: Doc,
    raw: bool,
    editor: Entity<CodeEditor>,
    error: Option<String>,
    sel: usize,
    field: Option<(Path, bool, Entity<TextInput>)>,
}

enum State {
    Bool(Option<bool>),
    /// `hi == labels.len()` is NULL.
    Enum { labels: Arc<Vec<String>>, hi: usize },
    Json(Box<JsonState>),
    Text(Entity<CodeEditor>),
}

pub struct ValueEditor {
    state: State,
    null: bool,
    nullable: bool,
    focus: FocusHandle,
    body: FocusHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ValueEvent> for ValueEditor {}

impl Focusable for ValueEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ValueEditor {
    pub fn new(mode: Mode, value: Option<&str>, nullable: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut subs = Vec::new();
        let null = value.is_none() && nullable;
        let body = cx.focus_handle();
        let state = match mode {
            Mode::Bool => State::Bool(match value {
                Some(v) => Some(matches!(v.trim().to_ascii_lowercase().as_str(), "t" | "true" | "1" | "yes" | "on")),
                None if nullable => None,
                None => Some(true),
            }),
            Mode::Enum(labels) => {
                let none = if nullable { labels.len() } else { 0 };
                let hi = value.and_then(|v| labels.iter().position(|l| l == v)).unwrap_or(if value.is_none() { none } else { 0 });
                State::Enum { labels, hi }
            }
            Mode::Json => {
                let text = value.unwrap_or("null");
                let (doc, raw, error) = match Doc::parse(text) {
                    Ok(d) => (d, false, None),
                    Err(e) => (Doc::new(Node::Null), true, Some(e.to_string())),
                };
                let pretty = if raw { text.to_string() } else { doc.root.pretty() };
                let editor = cx.new(|cx| {
                    let mut e = CodeEditor::new(window, cx).plain();
                    e.set_folding(true, cx);
                    e.set_text(&pretty, cx);
                    e
                });
                subs.push(cx.subscribe_in(&editor, window, |e, _, ev: &EditorEvent, _, cx| match ev {
                    EditorEvent::Changed => e.reparse(cx),
                    EditorEvent::Run(_) | EditorEvent::RunAll(_) => e.apply(cx),
                    EditorEvent::Definition { .. } | EditorEvent::Notice(_) => {}
                }));
                State::Json(Box::new(JsonState { doc, raw, editor, error, sel: 0, field: None }))
            }
            Mode::LongText => {
                let editor = cx.new(|cx| {
                    let mut e = CodeEditor::new(window, cx).plain();
                    e.set_text(value.unwrap_or(""), cx);
                    e
                });
                subs.push(cx.subscribe_in(&editor, window, |e, _, ev: &EditorEvent, _, cx| match ev {
                    EditorEvent::Changed => {
                        e.null = false;
                        cx.notify();
                    }
                    EditorEvent::Run(_) | EditorEvent::RunAll(_) => e.apply(cx),
                    EditorEvent::Definition { .. } | EditorEvent::Notice(_) => {}
                }));
                State::Text(editor)
            }
        };
        let mut e = ValueEditor { state, null, nullable, focus: cx.focus_handle(), body, _subs: subs };
        e.focus_start(window, cx);
        e
    }

    pub fn focus_start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.state {
            State::Text(ed) => window.focus(&ed.focus_handle(cx), cx),
            State::Json(j) if j.raw => window.focus(&j.editor.focus_handle(cx), cx),
            _ => window.focus(&self.body, cx),
        }
    }

    pub fn value(&self, cx: &App) -> Result<CellValue, String> {
        match &self.state {
            State::Bool(b) => Ok(match b {
                None => CellValue::Null,
                Some(b) => CellValue::text(if *b { "true" } else { "false" }),
            }),
            State::Enum { labels, hi } => Ok(labels.get(*hi).map(CellValue::text).unwrap_or(CellValue::Null)),
            _ if self.null => Ok(CellValue::Null),
            State::Json(j) => {
                if j.raw {
                    let text = j.editor.read(cx).text().to_string();
                    jsontree::parse(&text).map(|n| CellValue::text(n.compact())).map_err(|e| e.to_string())
                } else {
                    Ok(CellValue::text(j.doc.root.compact()))
                }
            }
            State::Text(ed) => Ok(CellValue::text(ed.read(cx).text())),
        }
    }

    pub fn apply(&mut self, cx: &mut Context<Self>) {
        self.end_field(true, cx);
        if self.value(cx).is_ok() {
            cx.emit(ValueEvent::Commit);
        }
    }

    fn toggle_null(&mut self, cx: &mut Context<Self>) {
        if !self.nullable {
            return;
        }
        match &mut self.state {
            State::Bool(b) => *b = None,
            State::Enum { labels, hi } => *hi = labels.len(),
            _ => self.null = !self.null,
        }
        cx.notify();
    }

    fn arrow(&mut self, dx: i64, dy: i64, cx: &mut Context<Self>) {
        let extra = self.nullable as i64;
        match &mut self.state {
            State::Bool(b) => {
                let order = [Some(true), Some(false), None];
                let i = order.iter().position(|x| x == b).unwrap_or(0) as i64;
                *b = order[(i + dx + dy).rem_euclid(2 + extra) as usize];
            }
            State::Enum { labels, hi } => {
                let n = labels.len() as i64 + extra;
                *hi = ((*hi as i64 + dy + dx).rem_euclid(n)) as usize;
            }
            State::Json(j) if !j.raw => {
                let rows = j.doc.rows();
                if dy != 0 {
                    j.sel = (j.sel as i64 + dy).clamp(0, rows.len() as i64 - 1) as usize;
                } else if let Some(r) = rows.get(j.sel)
                    && r.container
                    && (dx > 0) != r.expanded
                {
                    j.doc.toggle(&r.path);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &mut self.state {
            State::Json(j) if !j.raw => {
                let rows = j.doc.rows();
                if let Some(r) = rows.get(j.sel).cloned() {
                    if r.container {
                        j.doc.toggle(&r.path);
                        cx.notify();
                    } else {
                        self.begin_field(r.path, false, window, cx);
                    }
                }
            }
            State::Bool(_) | State::Enum { .. } => self.apply(cx),
            _ => {}
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if k.modifiers.platform || k.modifiers.control || k.modifiers.alt {
            return;
        }
        let Some(ch) = k.key_char.as_ref().and_then(|s| s.chars().next()).map(|c| c.to_ascii_lowercase()) else { return };
        let nullable = self.nullable;
        match &mut self.state {
            State::Bool(b) => match ch {
                't' | '1' => *b = Some(true),
                'f' | '0' => *b = Some(false),
                'n' if nullable => *b = None,
                _ => return,
            },
            State::Enum { labels, hi } => {
                let n = labels.len();
                if let Some(i) = (1..=n).map(|d| (*hi + d) % n.max(1)).find(|&i| labels[i].to_lowercase().starts_with(ch)) {
                    *hi = i;
                } else if nullable && ch == 'n' && !labels.iter().any(|l| l.to_lowercase().starts_with('n')) {
                    *hi = n;
                } else {
                    return;
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn reparse(&mut self, cx: &mut Context<Self>) {
        if let State::Json(j) = &mut self.state
            && j.raw
        {
            self.null = false;
            j.error = jsontree::parse(j.editor.read(cx).text()).err().map(|e| e.to_string());
            cx.notify();
        }
    }

    fn set_raw(&mut self, raw: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.end_field(true, cx);
        let State::Json(j) = &mut self.state else { return };
        if raw == j.raw {
            return;
        }
        if raw {
            let text = j.doc.root.pretty();
            j.editor.update(cx, |e, cx| e.set_text(&text, cx));
            j.raw = true;
            j.error = None;
            window.focus(&j.editor.focus_handle(cx), cx);
        } else {
            match Doc::parse(j.editor.read(cx).text()) {
                Ok(d) => {
                    j.doc = d;
                    j.raw = false;
                    j.sel = 0;
                    window.focus(&self.body, cx);
                }
                Err(e) => j.error = Some(e.to_string()),
            }
        }
        cx.notify();
    }

    fn begin_field(&mut self, path: Path, key: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.end_field(true, cx);
        let State::Json(j) = &mut self.state else { return };
        let text = if key {
            let Some((&i, parent)) = path.split_last() else { return };
            match j.doc.root.get(parent) {
                Some(Node::Object(o)) => o.get(i).map(|m| m.0.clone()).unwrap_or_default(),
                _ => return,
            }
        } else {
            j.doc.root.get(&path).map(Node::edit_text).unwrap_or_default()
        };
        let input = cx.new(|cx| {
            let mut i = TextInput::new(cx, "").bare();
            i.set_text(text, cx);
            i.select_all(cx);
            i
        });
        let sub = cx.subscribe_in(&input, window, |e, _, ev: &InputEvent, window, cx| {
            if let InputEvent::Submit = ev {
                e.end_field(true, cx);
                window.focus(&e.body, cx);
            }
        });
        self._subs.push(sub);
        window.focus(&input.focus_handle(cx), cx);
        j.field = Some((path, key, input));
        cx.notify();
    }

    fn end_field(&mut self, keep: bool, cx: &mut Context<Self>) -> bool {
        let State::Json(j) = &mut self.state else { return false };
        let Some((path, key, input)) = j.field.take() else { return false };
        if keep {
            let text = input.read(cx).text().to_string();
            if key {
                let _ = j.doc.rename(&path, &text);
            } else {
                j.doc.set_text(&path, &text);
            }
            self.null = false;
        }
        cx.notify();
        true
    }

    fn add_node(&mut self, path: Path, window: &mut Window, cx: &mut Context<Self>) {
        self.end_field(true, cx);
        let State::Json(j) = &mut self.state else { return };
        if let Some(p) = j.doc.add(&path) {
            self.null = false;
            let rows = j.doc.rows();
            j.sel = rows.iter().position(|r| r.path == p).unwrap_or(j.sel);
            let is_key = matches!(rows.get(j.sel).and_then(|r| r.key.clone()), Some(RowKey::Key(_)));
            self.begin_field(p, is_key, window, cx);
        }
    }

    fn remove_node(&mut self, path: Path, cx: &mut Context<Self>) {
        self.end_field(false, cx);
        let State::Json(j) = &mut self.state else { return };
        j.doc.remove(&path);
        let n = j.doc.rows().len();
        j.sel = j.sel.min(n.saturating_sub(1));
        self.null = false;
        cx.notify();
    }

    fn remove_selected(&mut self, cx: &mut Context<Self>) {
        let path = match &self.state {
            State::Json(j) if !j.raw && j.field.is_none() => j.doc.rows().get(j.sel).map(|r| r.path.clone()),
            _ => None,
        };
        if let Some(p) = path.filter(|p| !p.is_empty()) {
            self.remove_node(p, cx);
        }
    }

    #[cfg(feature = "checks")]
    pub fn demo_edit_first_leaf(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = match &self.state {
            State::Json(j) => j.doc.rows().into_iter().find(|r| !r.container && !r.path.is_empty()).map(|r| r.path),
            _ => None,
        };
        if let Some(p) = path {
            if let State::Json(j) = &mut self.state {
                j.sel = j.doc.rows().iter().position(|r| r.path == p).unwrap_or(0);
            }
            self.begin_field(p, false, window, cx);
        }
    }
}

fn json_color(t: &Theme, type_name: &str) -> Hsla {
    match type_name {
        "string" => t.str,
        "number" => t.num,
        "bool" => t.kw,
        "null" => t.tx3,
        _ => t.tx3,
    }
}

impl ValueEditor {
    fn render_bool(&self, b: Option<bool>, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        ui::segmented(t)
            .w(px(240.))
            .children([(Some(true), "true"), (Some(false), "false"), (None, "NULL")].into_iter().filter(|(v, _)| v.is_some() || self.nullable).enumerate().map(|(i, (v, label))| {
                ui::segment(t, ("bool-seg", i), label, b == v).on_click(cx.listener(move |e, _: &ClickEvent, _, cx| {
                    if let State::Bool(x) = &mut e.state {
                        *x = v;
                    }
                    e.apply(cx);
                }))
            }))
            .into_any_element()
    }

    fn render_enum(&self, labels: &Arc<Vec<String>>, hi: usize, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (acc, on_acc, hov) = (t.acc, t.on_acc, t.hov);
        let rows = labels.iter().map(|l| (l.clone(), false)).chain(self.nullable.then(|| ("NULL".to_string(), true)));
        div()
            .id("enum-list")
            .max_h(px(240.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .children(rows.enumerate().map(|(i, (label, null))| {
                let on = i == hi;
                div()
                    .id(("enum-item", i))
                    .h(px(24.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .when(on, |d| d.bg(acc).text_color(on_acc))
                    .when(!on, |d| d.text_color(if null { t.null } else { t.tx }).hover(move |s| s.bg(hov)))
                    .when(null, |d| d.italic())
                    .child(label)
                    .on_click(cx.listener(move |e, _: &ClickEvent, _, cx| {
                        if let State::Enum { hi, .. } = &mut e.state {
                            *hi = i;
                        }
                        e.apply(cx);
                    }))
            }))
            .into_any_element()
    }

    fn render_json(&self, j: &JsonState, t: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let raw = j.raw;
        let modes = ui::segmented(t)
            .w(px(120.))
            .h(px(24.))
            .child(ui::segment(t, "json-tree", "Tree", !raw).on_click(cx.listener(|e, _: &ClickEvent, window, cx| e.set_raw(false, window, cx))))
            .child(ui::segment(t, "json-raw", "Raw", raw).on_click(cx.listener(|e, _: &ClickEvent, window, cx| e.set_raw(true, window, cx))));
        let status: gpui::AnyElement = match (&j.error, raw) {
            (Some(e), _) => div().text_size(px(11.)).text_color(t.err).overflow_hidden().whitespace_nowrap().text_ellipsis().child(e.clone()).into_any_element(),
            (None, true) => div().text_size(px(11.)).text_color(t.ok).child("valid").into_any_element(),
            (None, false) => div().into_any_element(),
        };
        let head = div().flex().flex_row().items_center().gap(px(10.)).child(modes).child(div().flex_1().min_w_0().child(status));
        let body: gpui::AnyElement = if raw {
            div().h(px(220.)).border_1().border_color(t.line2).overflow_hidden().child(j.editor.clone()).into_any_element()
        } else {
            let rows = j.doc.rows();
            let (sel_bg, hov) = (t.sel, t.hov);
            div()
                .id("json-rows")
                .h(px(220.))
                .overflow_y_scroll()
                .border_1()
                .border_color(t.line2)
                .py(px(4.))
                .children(rows.into_iter().enumerate().map(|(i, r)| {
                    let on = i == j.sel;
                    let editing = j.field.as_ref().filter(|(p, _, _)| *p == r.path).map(|(_, k, input)| (*k, input.clone()));
                    let p_toggle = r.path.clone();
                    let p_key = r.path.clone();
                    let p_val = r.path.clone();
                    let p_add = r.path.clone();
                    let p_rm = r.path.clone();
                    let is_root = r.path.is_empty();
                    let group: gpui::SharedString = format!("json-row-{i}").into();
                    let key_el: Option<gpui::AnyElement> = match &r.key {
                        Some(RowKey::Key(k)) => Some(match &editing {
                            Some((true, input)) => div().min_w(px(60.)).child(input.clone()).into_any_element(),
                            _ => div()
                                .id(("json-key", i))
                                .flex_none()
                                .text_color(t.typ)
                                .child(format!("\"{k}\""))
                                .on_click(cx.listener(move |e, ev: &ClickEvent, window, cx| {
                                    if ev.click_count() >= 1 {
                                        e.begin_field(p_key.clone(), true, window, cx);
                                    }
                                }))
                                .into_any_element(),
                        }),
                        Some(RowKey::Index(n)) => Some(div().flex_none().text_color(t.tx4).child(n.to_string()).into_any_element()),
                        None => None,
                    };
                    let value_el: gpui::AnyElement = match &editing {
                        Some((false, input)) => div().flex_1().min_w(px(60.)).child(input.clone()).into_any_element(),
                        _ if r.container => div().flex_none().text_color(t.tx3).child(r.value.clone()).into_any_element(),
                        _ => div()
                            .id(("json-val", i))
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(json_color(t, r.type_name))
                            .child(r.value.clone())
                            .on_click(cx.listener(move |e, _: &ClickEvent, window, cx| e.begin_field(p_val.clone(), false, window, cx)))
                            .into_any_element(),
                    };
                    div()
                        .id(("json-row", i))
                        .group(group.clone())
                        .h(px(22.))
                        .pl(px(6. + r.depth as f32 * 14.))
                        .pr(px(4.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .when(on, |d| d.bg(sel_bg))
                        .when(!on, |d| d.hover(move |s| s.bg(hov)))
                        .on_mouse_down(MouseButton::Left, cx.listener(move |e, _, _, cx| {
                            if let State::Json(j) = &mut e.state {
                                j.sel = i;
                            }
                            cx.notify();
                        }))
                        .child(
                            div()
                                .id(("json-toggle", i))
                                .w(px(12.))
                                .flex_none()
                                .when(r.container, |d| {
                                    d.child(ui::disclosure(("json-chev", i), r.expanded, 10., t.tx3))
                                        .on_click(cx.listener(move |e, _: &ClickEvent, _, cx| {
                                            if let State::Json(j) = &mut e.state {
                                                j.doc.toggle(&p_toggle);
                                            }
                                            cx.notify();
                                        }))
                                }),
                        )
                        .children(key_el)
                        .when(r.key.is_some(), |d| d.child(div().flex_none().text_color(t.tx4).child(":")))
                        .child(value_el)
                        .child(div().flex_1())
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(2.))
                                .invisible()
                                .group_hover(group, |s| s.visible())
                                .when(on, |d| d.visible())
                                .child(ui::icon_button(t, ("json-add", i), Icon::Plus, 18., 10.).on_click(cx.listener(move |e, _: &ClickEvent, window, cx| e.add_node(p_add.clone(), window, cx))))
                                .when(!is_root, |d| {
                                    d.child(ui::icon_button(t, ("json-rm", i), Icon::Close, 18., 10.).on_click(cx.listener(move |e, _: &ClickEvent, _, cx| e.remove_node(p_rm.clone(), cx))))
                                }),
                        )
                }))
                .into_any_element()
        };
        div().flex().flex_col().gap(px(8.)).child(head).child(body).into_any_element()
    }
}

impl Render for ValueEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let null = self.null;
        let (body, width, footer): (gpui::AnyElement, f32, bool) = match &self.state {
            State::Bool(b) => (self.render_bool(*b, &t, cx), 260., false),
            State::Enum { labels, hi } => {
                let (l, h) = (labels.clone(), *hi);
                (self.render_enum(&l, h, &t, cx), 240., false)
            }
            State::Json(j) => (self.render_json(j, &t, cx), 440., true),
            State::Text(ed) => (
                div().h(px(160.)).border_1().border_color(t.line2).overflow_hidden().child(ed.clone()).into_any_element(),
                420.,
                true,
            ),
        };
        let can_apply = self.value(cx).is_ok();
        let nullable = self.nullable;
        let foot = footer.then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .when(nullable, |d| d.child(
                    div()
                        .id("value-null")
                        .flex_none()
                        .h(px(18.))
                        .px(px(5.))
                        .flex()
                        .items_center()
                        .text_size(px(10.))
                        .when(null, |d| d.bg(t.acc).text_color(t.on_acc))
                        .when(!null, |d| d.bg(t.hov).text_color(t.tx3).border_1().border_color(t.line2))
                        .child("NULL")
                        .on_click(cx.listener(|e, _: &ClickEvent, _, cx| e.toggle_null(cx))),
                ))
                .child(div().flex_1())
                .child(
                    ui::button(&t, "value-apply", Btn::Primary)
                        .h(px(24.))
                        .text_size(px(11.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .when(!can_apply, |d| d.opacity(0.5))
                        .child("Apply")
                        .child(ui::key_hint("⌘↩"))
                        .on_click(cx.listener(|e, _: &ClickEvent, _, cx| e.apply(cx))),
                )
        });
        let root = div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .font_family(mono())
            .text_size(px(12.))
            .text_color(t.tx)
            .on_action(cx.listener(|e, _: &ApplyValue, _, cx| e.apply(cx)))
            .on_action(cx.listener(|e, _: &CancelValue, _, cx| {
                if !e.end_field(false, cx) {
                    cx.emit(ValueEvent::Cancel);
                }
            }))
            .on_action(cx.listener(|e, _: &ToggleNullValue, _, cx| e.toggle_null(cx)))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .key_context("ValueBody")
                    .track_focus(&self.body)
                    .on_action(cx.listener(|e, _: &Up, _, cx| e.arrow(0, -1, cx)))
                    .on_action(cx.listener(|e, _: &Down, _, cx| e.arrow(0, 1, cx)))
                    .on_action(cx.listener(|e, _: &Left, _, cx| e.arrow(-1, 0, cx)))
                    .on_action(cx.listener(|e, _: &Right, _, cx| e.arrow(1, 0, cx)))
                    .on_action(cx.listener(|e, _: &Confirm, window, cx| e.confirm(window, cx)))
                    .on_action(cx.listener(|e, _: &RemoveNode, _, cx| e.remove_selected(cx)))
                    .on_key_down(cx.listener(|e, ev: &KeyDownEvent, _, cx| e.on_key(ev, cx)))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(body)
                    .children(foot),
            );
        ui::popover(&t).w(px(width + 20.)).p(px(10.)).child(root).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_by_type() {
        let labels = Arc::new(vec!["new".to_string(), "paid".to_string()]);
        assert_eq!(mode_for(Kind::Bool, None, Some("t")), Some(Mode::Bool));
        assert_eq!(mode_for(Kind::Json, None, None), Some(Mode::Json));
        assert_eq!(mode_for(Kind::TimestampTz, None, None), None, "dates are typed");
        assert_eq!(mode_for(Kind::Text, Some(&labels), Some("new")), Some(Mode::Enum(labels.clone())));
        assert_eq!(mode_for(Kind::Text, Some(&Arc::new(vec![])), Some("x")), None, "a plain text column");
        assert_eq!(mode_for(Kind::Text, None, Some("a\nb")), Some(Mode::LongText));
        assert_eq!(mode_for(Kind::Text, None, Some(&"x".repeat(81))), Some(Mode::LongText));
        assert_eq!(mode_for(Kind::Int, None, Some(&"1".repeat(100))), None);
    }

    #[test]
    fn dates_typed_as_text() {
        assert!(date_text_ok(Kind::Date, "1999-12-31"));
        assert!(!date_text_ok(Kind::Date, "1999-13-01"));
        assert!(!date_text_ok(Kind::Date, "31/12/1999"));
        assert!(date_text_ok(Kind::Date, "0000-00-00"), "MySQL's zero date is the server's call");
        assert!(date_text_ok(Kind::Time, "07:30"));
        assert!(date_text_ok(Kind::Time, "07:30:15.25"));
        assert!(!date_text_ok(Kind::Time, "25:00"));
        assert!(date_text_ok(Kind::Timestamp, "2026-10-13 08:00:00"));
        assert!(date_text_ok(Kind::Timestamp, "2026-10-13T08:00"));
        assert!(date_text_ok(Kind::Timestamp, "2026-10-13"), "a date alone is midnight");
        assert!(!date_text_ok(Kind::Timestamp, "2026-10-13 8h"));
        assert!(date_text_ok(Kind::TimestampTz, "2024-02-29 13:05:07.25+00"));
        assert!(date_text_ok(Kind::TimestampTz, "2024-02-29T13:05:07-03:00"));
        assert!(date_text_ok(Kind::TimestampTz, "2024-02-29 13:05:07Z"));
        assert!(!date_text_ok(Kind::TimestampTz, "2024-02-30 13:05:07+00"));
        assert!(date_text_ok(Kind::TimestampTz, "infinity"));
        assert!(date_text_ok(Kind::Text, "anything"));
    }
}
