use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding, MouseButton, Window,
    actions, div, prelude::*,
};
use crate::theme::px;
use dataman_core::dataview::{CountStop, Filter, Op, TableView};
use dataman_core::value::Kind;

use crate::app::Workspace;
use crate::grid::ui_count;
use crate::input::{InputEvent, TextInput};
use crate::theme::{ActiveTheme, mono};
use crate::ui::{self, Btn};

actions!(table_filter, [CloseFilter, NextOp, PrevOp]);

pub const POPOVER_CONTEXT: &str = "FilterPopover";

pub fn bind_keys(cx: &mut App) {
    let c = Some(POPOVER_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("escape", CloseFilter, c),
        KeyBinding::new("tab", NextOp, c),
        KeyBinding::new("shift-tab", PrevOp, c),
    ]);
}

pub struct TableData {
    pub view: TableView,
    pub sql: String,
    pub stmt: String,
    pub estimate: Option<u64>,
    pub total: Option<u64>,
    pub total_of: String,
    pub counting: Option<(CountStop, gpui::Task<()>)>,
    pub stopped: Option<String>,
}

impl TableData {
    pub fn new(view: TableView) -> TableData {
        let sql = view.sql();
        let stmt = sql.trim_end_matches(';').to_string();
        TableData { view, sql, stmt, estimate: None, total: None, total_of: String::new(), counting: None, stopped: None }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pager {
    pub first: u64,
    pub last: u64,
    pub total: Option<(u64, bool)>,
    pub counting: bool,
    pub prev: bool,
    pub next: bool,
}

impl Pager {
    pub fn range(&self) -> String {
        if self.last < self.first { "0".into() } else { format!("{}–{}", ui_count(self.first as usize), ui_count(self.last as usize)) }
    }

    pub fn of(&self) -> Option<String> {
        self.total.map(|(n, exact)| if exact { format!("of {}", ui_count(n as usize)) } else { format!("of ~{}", short_count(n)) })
    }
}

pub fn short_count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{:.1}k", n as f64 / 1e3).replace(".0k", "k"),
        10_000..1_000_000 => format!("{}k", n / 1000),
        1_000_000..10_000_000 => format!("{:.1}M", n as f64 / 1e6).replace(".0M", "M"),
        10_000_000..1_000_000_000 => format!("{}M", n / 1_000_000),
        _ => format!("{:.1}B", n as f64 / 1e9),
    }
}

pub fn pager(d: &TableData, rows: usize, finished: bool) -> Pager {
    let mut p = pager_of(&d.view, d.estimate, d.total.filter(|_| d.total_of == d.view.count_sql()), rows, finished);
    p.counting = d.counting.is_some();
    p
}

pub fn pager_of(view: &TableView, estimate: Option<u64>, exact: Option<u64>, rows: usize, finished: bool) -> Pager {
    let off = view.offset();
    let first = off + 1;
    let last = off + rows as u64;
    let known = if finished && (rows as u64) < view.page_size { Some(last) } else { None };
    let total = match (exact.or(known), estimate) {
        (Some(n), _) => Some((n, true)),
        (None, Some(e)) if !view.is_filtered() && e >= last => Some((e, false)),
        _ => None,
    };
    let next = match total {
        Some((n, true)) => last < n,
        _ => rows as u64 >= view.page_size,
    };
    Pager { first, last, total, counting: false, prev: view.page > 0, next }
}

pub enum FilterEvent {
    Apply(Filter, Option<usize>),
    Remove(usize),
    Close,
}

pub struct FilterPopover {
    pub column: String,
    kind: Kind,
    type_name: String,
    op: Op,
    input: Entity<TextInput>,
    focus: FocusHandle,
    editing: Option<usize>,
}

impl EventEmitter<FilterEvent> for FilterPopover {}

impl Focusable for FilterPopover {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl FilterPopover {
    pub fn new(column: String, kind: Kind, type_name: String, existing: Option<(usize, &Filter)>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new(cx, ""));
        let (op, value, editing) = match existing {
            Some((i, f)) => (f.op, f.value.clone(), Some(i)),
            None => (if kind == Kind::Text { Op::Contains } else { Op::Eq }, String::new(), None),
        };
        input.update(cx, |i, cx| {
            i.set_text(value, cx);
            i.select_all(cx);
        });
        cx.subscribe(&input, |p: &mut FilterPopover, _, ev: &InputEvent, cx| {
            if let InputEvent::Submit = ev {
                p.apply(cx);
            }
        })
        .detach();
        window.focus(&input.focus_handle(cx), cx);
        FilterPopover { column, kind, type_name, op, input, focus: cx.focus_handle(), editing }
    }

    pub fn set_op(&mut self, op: Op, window: &mut Window, cx: &mut Context<Self>) {
        self.op = op;
        if op.takes_value() {
            window.focus(&self.input.focus_handle(cx), cx);
        } else {
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }

    fn step_op(&mut self, dir: isize, window: &mut Window, cx: &mut Context<Self>) {
        let all = Op::ALL;
        let i = all.iter().position(|o| *o == self.op).unwrap_or(0) as isize;
        let n = all.len() as isize;
        self.set_op(all[((i + dir).rem_euclid(n)) as usize], window, cx);
    }

    #[cfg(feature = "checks")]
    pub fn set_value(&mut self, v: &str, cx: &mut Context<Self>) {
        self.input.update(cx, |i, cx| i.set_text(v, cx));
    }

    pub fn apply(&mut self, cx: &mut Context<Self>) {
        let value = if self.op.takes_value() { self.input.read(cx).text().to_string() } else { String::new() };
        cx.emit(FilterEvent::Apply(Filter::new(self.column.clone(), self.kind, self.op, value), self.editing));
    }
}

impl Render for FilterPopover {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let op = self.op;
        let (hov, prs, acc_s) = (t.hov, t.prs, t.acc_s);
        let ops = Op::ALL.into_iter().enumerate().map(|(i, o)| {
            let on = o == op;
            div()
                .id(("filter-op", i))
                .h(px(22.))
                .px(px(7.))
                .flex()
                .items_center()
                .text_size(px(11.5))
                .when(on, |d| d.bg(acc_s).text_color(t.tx).border_1().border_color(t.acc))
                .when(!on, |d| d.text_color(t.tx2).bg(hov).hover(move |s| s.bg(prs)))
                .child(o.label())
                .on_click(cx.listener(move |p, _: &ClickEvent, window, cx| p.set_op(o, window, cx)))
        });
        let editing = self.editing;
        ui::popover(&t)
            .w(px(300.))
            .p(px(10.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .font_family(mono())
            .text_size(px(12.))
            .key_context(POPOVER_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &CloseFilter, _, cx| cx.emit(FilterEvent::Close)))
            .on_action(cx.listener(|p, _: &NextOp, window, cx| p.step_op(1, window, cx)))
            .on_action(cx.listener(|p, _: &PrevOp, window, cx| p.step_op(-1, window, cx)))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(6.))
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_color(t.tx).child(self.column.clone()))
                    .child(div().text_size(px(10.5)).text_color(t.tx3).child(self.type_name.clone())),
            )
            .child(div().flex().flex_row().flex_wrap().gap(px(4.)).children(ops))
            .when(op.takes_value(), |d| d.child(self.input.clone()))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .children(editing.map(|i| {
                        ui::button(&t, "filter-remove", Btn::Ghost)
                            .h(px(24.))
                            .text_size(px(11.5))
                            .child("Remove")
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| cx.emit(FilterEvent::Remove(i))))
                    }))
                    .child(div().flex_1())
                    .child(
                        ui::button(&t, "filter-apply", Btn::Primary)
                            .h(px(24.))
                            .text_size(px(11.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Apply")
                            .child(ui::key_hint("↩"))
                            .on_click(cx.listener(|p, _: &ClickEvent, _, cx| p.apply(cx))),
                    ),
            )
    }
}

impl Workspace {
    pub(crate) fn open_filter(&mut self, col: usize, editing: Option<usize>, at: (f32, f32), window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab().cloned() else { return };
        let (column, kind, type_name, existing) = {
            let t = tab.read(cx);
            let Some(d) = &t.table else { return };
            let col_info = t.result().and_then(|r| r.columns()).and_then(|c| c.get(col).cloned());
            let editing = editing.or_else(|| {
                let name = &col_info.as_ref()?.name;
                d.view.filters.iter().position(|f| &f.column == name)
            });
            let existing = editing.and_then(|i| d.view.filters.get(i).cloned().map(|f| (i, f)));
            match (&existing, col_info) {
                (Some((_, f)), info) => (f.column.clone(), f.kind, info.filter(|c| c.name == f.column).map(|c| c.type_name).unwrap_or_default(), existing),
                (None, Some(c)) => (c.name, c.kind, c.type_name, None),
                (None, None) => return,
            }
        };
        let pop = cx.new(|cx| FilterPopover::new(column, kind, type_name, existing.as_ref().map(|(i, f)| (*i, f)), window, cx));
        cx.subscribe_in(&pop, window, |w, _, ev: &FilterEvent, window, cx| {
            let tab = w.active_tab().cloned();
            match ev {
                FilterEvent::Apply(f, i) => {
                    if let Some(tab) = &tab {
                        let (f, i) = (f.clone(), *i);
                        tab.update(cx, |t, cx| t.add_filter(f, i, cx));
                    }
                }
                FilterEvent::Remove(i) => {
                    if let Some(tab) = &tab {
                        let i = *i;
                        tab.update(cx, |t, cx| t.remove_filter(i, cx));
                    }
                }
                FilterEvent::Close => {}
            }
            w.filter_pop = None;
            if let Some(tab) = tab {
                let g = tab.read(cx).grid.clone();
                window.focus(&g.focus_handle(cx), cx);
            }
            cx.notify();
        })
        .detach();
        self.filter_pop = Some((pop, at));
        cx.notify();
    }

    pub(crate) fn render_filter_popover(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (pop, at) = self.filter_pop.clone()?;
        Some(
            gpui::deferred(
                gpui::anchored()
                    .position(gpui::point(px(at.0), px(at.1)))
                    .snap_to_window_with_margin(px(8.))
                    .child(ui::menu_in("filter-pop-in", div().on_mouse_down_out(cx.listener(|w, _, _, cx| {
                        w.filter_pop = None;
                        cx.notify();
                    })).child(pop))),
            )
            .priority(3)
            .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(page: u64, size: u64, filtered: bool) -> TableView {
        let mut v = TableView::new("public", "orders",vec!["id".into()], size);
        v.page = page;
        if filtered {
            v.filters.push(Filter::new("status", Kind::Text, Op::Eq, "paid"));
        }
        v
    }

    fn pg(view: TableView, rows: usize, finished: bool, estimate: Option<u64>, total: Option<u64>) -> Pager {
        pager_of(&view, estimate, total, rows, finished)
    }

    #[test]
    fn pages_and_totals() {
        let p = pg(data(0, 1000, false), 1000, true, Some(1_234_567), None);
        assert_eq!((p.range(), p.of()), ("1–1,000".to_string(), Some("of ~1.2M".to_string())));
        assert!(p.next && !p.prev);
        let p = pg(data(2, 1000, false), 1000, true, Some(1_234_567), Some(1_234_000));
        assert_eq!((p.range(), p.of()), ("2,001–3,000".to_string(), Some("of 1,234,000".to_string())));
        assert!(p.next && p.prev);
        let p = pg(data(3, 1000, true), 17, true, None, None);
        assert_eq!(p.of(), Some("of 3,017".to_string()));
        assert!(!p.next);
        let p = pg(data(0, 1000, true), 1000, true, Some(5_000_000), None);
        assert_eq!(p.of(), None);
        assert!(p.next);
        let p = pg(data(1, 1000, false), 1000, true, Some(10), None);
        assert_eq!(p.of(), None);
        let p = pg(data(0, 1000, true), 0, true, None, None);
        assert_eq!((p.range(), p.of()), ("0".to_string(), Some("of 0".to_string())));
        assert_eq!(short_count(950), "950");
        assert_eq!(short_count(1_000), "1k");
        assert_eq!(short_count(34_500), "34k");
        assert_eq!(short_count(12_000_000), "12M");
    }
}
