//! A JSON document as an outline: folding objects and arrays, by click (⌥ for the whole
//! subtree), by level, and by ←/→ on the focused row. Only the rows in sight are laid out.

use std::collections::{HashMap, HashSet};

use dataman_core::jsontree::{Doc, Node, Path, Row, RowKey};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla, MouseButton, MouseDownEvent, ScrollStrategy,
    UniformListScrollHandle, Window, actions, div, prelude::*, uniform_list,
};

use crate::theme::{ActiveTheme, Theme, px};
use crate::ui::{self, Btn, Tipped};

actions!(json_outline, [OutlineUp, OutlineDown, OutlineCollapse, OutlineExpand, OutlineCollapseTree, OutlineExpandTree]);

pub const CONTEXT: &str = "JsonOutline";

const LINE: f32 = 20.;

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        crate::keymap::kb("up", OutlineUp, c),
        crate::keymap::kb("down", OutlineDown, c),
        crate::keymap::kb("left", OutlineCollapse, c),
        crate::keymap::kb("right", OutlineExpand, c),
        crate::keymap::kb("alt-left", OutlineCollapseTree, c),
        crate::keymap::kb("alt-right", OutlineExpandTree, c),
    ]);
}

pub enum OutlineEvent {
    /// ← past the top level: back to whatever led here.
    Leave,
    /// A value double-clicked: the host's text, to change it.
    Edit,
}

pub struct JsonOutline {
    focus: FocusHandle,
    doc: Option<Doc>,
    key: String,
    /// What each document seen here had folded, by its key: it comes back folded the same.
    seen: HashMap<String, HashSet<Path>>,
    rows: Vec<Row>,
    cursor: Option<Path>,
    scroll: UniformListScrollHandle,
    /// A PostgreSQL array: NULL for null, strings bare.
    list: bool,
}

impl EventEmitter<OutlineEvent> for JsonOutline {}

impl Focusable for JsonOutline {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl JsonOutline {
    pub fn new(cx: &mut Context<Self>) -> Self {
        JsonOutline { focus: cx.focus_handle(), doc: None, key: String::new(), seen: HashMap::new(), rows: Vec::new(), cursor: None, scroll: UniformListScrollHandle::new(), list: false }
    }

    pub fn set_list(&mut self, list: bool) {
        self.list = list;
    }

    /// The document under `key`, folded as it was when last shown here, or as a fresh one opens.
    pub fn show(&mut self, key: &str, root: Option<Node>, cx: &mut Context<Self>) {
        if self.key == key && self.doc.as_ref().map(|d| &d.root) == root.as_ref() {
            return;
        }
        if let Some(d) = self.doc.take() {
            self.seen.insert(std::mem::take(&mut self.key), d.collapsed);
        }
        let again = self.key == key;
        self.key = key.to_string();
        self.doc = root.map(|r| {
            let mut d = Doc::new(r);
            match self.seen.remove(key) {
                Some(c) => d.collapsed = c,
                None => d.open_default(),
            }
            d
        });
        if !again {
            self.cursor = None;
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        self.refresh();
        cx.notify();
    }

    /// Focus arriving from elsewhere: the cursor on the first row unless it has one.
    pub fn enter(&mut self, cx: &mut Context<Self>) {
        if self.at().is_none() {
            self.step(0, cx);
        }
    }

    pub fn has_doc(&self) -> bool {
        self.doc.is_some()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn foldable(&self) -> bool {
        self.rows.iter().any(|r| r.len > 0)
    }

    fn refresh(&mut self) {
        self.rows = self.doc.as_ref().map(|d| d.rows().into_iter().skip(1).collect()).unwrap_or_default();
        // A folded row keeps the cursor that was inside it.
        if let Some(c) = &mut self.cursor {
            while !c.is_empty() && !self.rows.iter().any(|r| r.path == *c) {
                c.pop();
            }
            if c.is_empty() {
                self.cursor = None;
            }
        }
    }

    fn at(&self) -> Option<usize> {
        let c = self.cursor.as_ref()?;
        self.rows.iter().position(|r| r.path == *c)
    }

    fn put(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(r) = self.rows.get(ix) {
            self.cursor = Some(r.path.clone());
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    fn fold(&mut self, path: &[usize], deep: bool, cx: &mut Context<Self>) {
        let Some(d) = &mut self.doc else { return };
        let open = d.collapsed.contains(path);
        if deep { d.set_subtree(path, open) } else { d.toggle(path) }
        self.refresh();
        cx.notify();
    }

    pub fn all(&mut self, open: bool, cx: &mut Context<Self>) {
        if let Some(d) = &mut self.doc {
            if open { d.expand_all() } else { d.collapse_all() }
        }
        self.refresh();
        cx.notify();
    }

    pub fn level(&mut self, n: usize, cx: &mut Context<Self>) {
        if let Some(d) = &mut self.doc {
            d.collapse_to_level(n);
        }
        self.refresh();
        cx.notify();
    }

    fn step(&mut self, dy: i64, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        let ix = match self.at() {
            Some(i) => (i as i64 + dy).clamp(0, self.rows.len() as i64 - 1) as usize,
            None => 0,
        };
        self.put(ix, cx);
    }

    /// ←: folds the row, else goes to its parent; past the top level, `Leave`.
    fn left(&mut self, deep: bool, cx: &mut Context<Self>) {
        let Some(ix) = self.at() else {
            cx.emit(OutlineEvent::Leave);
            return;
        };
        let r = self.rows[ix].clone();
        if r.len > 0 && (r.expanded || deep) {
            self.fold_at(&r.path, false, deep, cx);
        } else if r.depth > 1 {
            let parent = &r.path[..r.path.len() - 1];
            if let Some(p) = self.rows.iter().position(|x| x.path == parent) {
                self.put(p, cx);
            }
        } else {
            cx.emit(OutlineEvent::Leave);
        }
    }

    /// →: unfolds the row, else goes to its first member.
    fn right(&mut self, deep: bool, cx: &mut Context<Self>) {
        let Some(ix) = self.at() else {
            self.step(0, cx);
            return;
        };
        let r = self.rows[ix].clone();
        if r.len == 0 {
            return;
        }
        if !r.expanded || deep {
            self.fold_at(&r.path, true, deep, cx);
        } else {
            self.put(ix + 1, cx);
        }
    }

    fn fold_at(&mut self, path: &[usize], open: bool, deep: bool, cx: &mut Context<Self>) {
        let Some(d) = &mut self.doc else { return };
        if deep {
            d.set_subtree(path, open);
        } else if open {
            d.collapsed.remove(path);
        } else {
            d.collapsed.insert(path.to_vec());
        }
        self.refresh();
        if let Some(ix) = self.at() {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub fn demo_lines(&self) -> Vec<String> {
        self.rows.iter().map(|r| format!("{}{}{}", "  ".repeat(r.depth - 1), key_text(r).map(|k| format!("{k}: ")).unwrap_or_default(), self.value_text(r))).collect()
    }

    #[cfg(feature = "checks")]
    pub fn demo_put(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.put(ix, cx);
    }

    /// A click on row `ix`'s chevron, with ⌥ or not.
    #[cfg(feature = "checks")]
    pub fn demo_click(&mut self, ix: usize, alt: bool, cx: &mut Context<Self>) {
        if let Some(p) = self.rows.get(ix).map(|r| r.path.clone()) {
            self.fold(&p, alt, cx);
        }
    }

    fn value_text(&self, r: &Row) -> String {
        if r.container {
            return if r.expanded && r.len > 0 { r.count_text() } else { r.preview() };
        }
        if !self.list {
            return r.value.clone();
        }
        match self.doc.as_ref().and_then(|d| d.root.get(&r.path)) {
            Some(Node::Null) => "NULL".into(),
            Some(n) => n.edit_text(),
            None => r.value.clone(),
        }
    }

    fn render_row(&self, ix: usize, t: &Theme, focused: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let r = &self.rows[ix];
        let on = self.cursor.as_ref() == Some(&r.path);
        let null = self.list && !r.container && r.type_name == "null";
        let color = if r.container { if r.expanded { t.tx4 } else { t.tx3 } } else if null { t.null } else { value_color(t, r.type_name) };
        let (hov, sel) = (t.hov, if focused { t.sel } else { t.hov });
        let (p_row, p_chev) = (r.path.clone(), r.path.clone());
        let foldable = r.len > 0;
        div()
            .id(("jo-row", ix))
            .h(px(LINE))
            .pl(px(10. + (r.depth - 1) as f32 * 16.))
            .pr(px(12.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .whitespace_nowrap()
            .overflow_hidden()
            .when(on, |d| d.bg(sel))
            .when(!on, |d| d.hover(move |s| s.bg(hov)))
            .child(
                div()
                    .id(("jo-chev", ix))
                    .w(px(12.))
                    .h(px(LINE))
                    .flex_none()
                    .flex()
                    .items_center()
                    .when(foldable, |d| {
                        // On the press, and not the row's: a double click on it is two folds.
                        d.cursor_pointer().child(ui::disclosure(("jo-turn", ix), r.expanded, 10., t.tx3)).on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |o, ev: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                window.focus(&o.focus, cx);
                                o.cursor = Some(p_chev.clone());
                                o.fold(&p_chev, ev.modifiers.alt, cx);
                            }),
                        )
                    }),
            )
            .children(key_text(r).map(|k| {
                let index = matches!(r.key, Some(RowKey::Index(_)));
                div().flex_none().text_color(if index { t.tx4 } else { t.typ }).child(format!("{k}:"))
            }))
            .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().text_color(color).when(null, |d| d.italic()).child(self.value_text(r)))
            .on_mouse_down(MouseButton::Left, cx.listener(move |o, ev: &MouseDownEvent, window, cx| {
                window.focus(&o.focus, cx);
                o.cursor = Some(p_row.clone());
                if ev.click_count == 2 {
                    if foldable { o.fold(&p_row, ev.modifiers.alt, cx) } else { cx.emit(OutlineEvent::Edit) }
                }
                cx.notify();
            }))
            .into_any_element()
    }
}

/// Expand all, Collapse all and the levels, for the host's toolbar; nothing when nothing folds.
pub fn controls(o: &Entity<JsonOutline>, t: &Theme, cx: &App) -> Option<gpui::AnyElement> {
    if !o.read(cx).foldable() {
        return None;
    }
    let ghost = |id: gpui::ElementId| ui::button(t, id, Btn::Ghost).h(px(24.)).px(px(7.)).text_size(px(11.5));
    let (a, b) = (o.clone(), o.clone());
    let levels = (1..=3usize).map(|n| {
        let o = o.clone();
        ghost(("jo-level", n).into())
            .w(px(22.))
            .px(px(0.))
            .justify_center()
            .child(n.to_string())
            .tip(ui::tooltip(format!("Collapse to level {n}"), None, None))
            .on_click(move |_, _, cx| o.update(cx, |o, cx| o.level(n, cx)))
    });
    Some(
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.))
            .child(ghost("jo-expand".into()).child("Expand all").on_click(move |_, _, cx| a.update(cx, |o, cx| o.all(true, cx))))
            .child(ghost("jo-collapse".into()).child("Collapse all").on_click(move |_, _, cx| b.update(cx, |o, cx| o.all(false, cx))))
            .child(div().pl(px(6.)).pr(px(2.)).text_size(px(11.)).text_color(t.tx4).child("Level"))
            .children(levels)
            .into_any_element(),
    )
}

fn key_text(r: &Row) -> Option<String> {
    match &r.key {
        Some(RowKey::Key(k)) => Some(k.clone()),
        Some(RowKey::Index(i)) => Some(i.to_string()),
        None => None,
    }
}

/// JSON's types and the shell's (MongoDB's int, long, objectId, date…).
pub fn value_color(t: &Theme, type_name: &str) -> Hsla {
    match type_name {
        "string" | "symbol" => t.str,
        "number" | "int" | "long" | "double" | "decimal" => t.num,
        "bool" => t.kw,
        "null" | "undefined" => t.tx3,
        _ => t.func,
    }
}

impl Render for JsonOutline {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let focused = self.focus.contains_focused(window, cx);
        let theme = t.clone();
        div()
            .id("json-outline")
            .relative()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|o, _: &OutlineUp, _, cx| o.step(-1, cx)))
            .on_action(cx.listener(|o, _: &OutlineDown, _, cx| o.step(1, cx)))
            .on_action(cx.listener(|o, _: &OutlineCollapse, _, cx| o.left(false, cx)))
            .on_action(cx.listener(|o, _: &OutlineExpand, _, cx| o.right(false, cx)))
            .on_action(cx.listener(|o, _: &OutlineCollapseTree, _, cx| o.left(true, cx)))
            .on_action(cx.listener(|o, _: &OutlineExpandTree, _, cx| o.right(true, cx)))
            .size_full()
            .flex()
            .flex_col()
            .font_family(crate::theme::mono())
            .text_size(px(12.))
            .line_height(px(LINE))
            .child(
                uniform_list("json-outline-rows", self.rows.len(), cx.processor(move |o, range: std::ops::Range<usize>, _, cx| range.map(|ix| o.render_row(ix, &theme, focused, cx)).collect()))
                    .track_scroll(&self.scroll)
                    .flex_1(),
            )
            .when(self.rows.is_empty() && self.doc.is_some(), |d| d.child(div().absolute().top(px(8.)).left(px(14.)).text_color(t.tx3).child("empty")))
    }
}
