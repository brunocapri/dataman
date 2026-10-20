use gpui::{SharedString, div, prelude::*};
use crate::theme::px;
use dataman_core::fknav::FkJump;

use crate::assets::{Icon, icon};
use crate::theme::Theme;
use crate::tree_menu::{ITEM_H, PAD, SEP_H};

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Go(FkJump),
    CopyValue,
    CopyWithHeaders,
    FilterValue(usize, Option<String>),
    SortAsc(usize),
    SortDesc(usize),
    Filter(usize),
    Hide(usize),
    ShowAll(usize),
    DeleteRows { n: usize, restore: bool },
}

impl Item {
    pub fn look(&self) -> (Icon, SharedString, Option<SharedString>) {
        match self {
            Item::Go(j) => (Icon::Table, j.label.clone().into(), Some(j.detail.clone().into())),
            Item::CopyValue => (Icon::Copy, "Copy value".into(), None),
            Item::CopyWithHeaders => (Icon::Copy, "Copy with headers".into(), None),
            Item::FilterValue(_, v) => {
                let d = match v {
                    None => "is null".to_string(),
                    Some(s) => {
                        let s: String = s.chars().take(24).map(|c| if c.is_control() { ' ' } else { c }).collect();
                        format!("= {s}")
                    }
                };
                (Icon::Filter, "Filter by value".into(), Some(d.into()))
            }
            Item::SortAsc(_) => (Icon::ChevronUp, "Sort ascending".into(), None),
            Item::SortDesc(_) => (Icon::ChevronDown, "Sort descending".into(), None),
            Item::Filter(_) => (Icon::Filter, "Filter…".into(), None),
            Item::Hide(_) => (Icon::Close, "Hide column".into(), None),
            Item::ShowAll(n) => (Icon::View, "Show all columns".into(), Some(format!("{n} hidden").into())),
            Item::DeleteRows { n, restore } => {
                let rows = if *n == 1 { "row".to_string() } else { format!("{n} rows") };
                (Icon::Trash, if *restore { format!("Restore {rows}") } else { format!("Delete {rows}") }.into(), None)
            }
        }
    }
}

pub fn header_entries(col: usize, table: bool, hidden: usize, can_hide: bool) -> Vec<Entry> {
    let mut v = vec![Entry::Item(Item::SortAsc(col)), Entry::Item(Item::SortDesc(col))];
    if table {
        v.push(Entry::Item(Item::Filter(col)));
    }
    if can_hide || hidden > 0 {
        v.push(Entry::Sep);
    }
    if can_hide {
        v.push(Entry::Item(Item::Hide(col)));
    }
    if hidden > 0 {
        v.push(Entry::Item(Item::ShowAll(hidden)));
    }
    v
}

pub fn with_filter(mut entries: Vec<Entry>, col: usize, value: Option<String>) -> Vec<Entry> {
    let at = entries.iter().position(|e| matches!(e, Entry::Item(Item::CopyValue))).unwrap_or(entries.len());
    entries.insert(at, Entry::Item(Item::FilterValue(col, value)));
    if at > 0 && !matches!(entries[at - 1], Entry::Sep) {
        entries.insert(at, Entry::Sep);
        return entries;
    }
    entries.insert(at + 1, Entry::Sep);
    entries
}

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Item(Item),
    Sep,
}

impl Entry {
    pub fn enabled(&self) -> bool {
        matches!(self, Entry::Item(_))
    }
}

pub const MENU_W: f32 = 300.;
pub const GAP: f32 = 2.;

pub fn entries(jumps: Vec<FkJump>, context: bool) -> Vec<Entry> {
    let mut v: Vec<Entry> = jumps.into_iter().map(|j| Entry::Item(Item::Go(j))).collect();
    if context {
        if !v.is_empty() {
            v.push(Entry::Sep);
        }
        v.push(Entry::Item(Item::CopyValue));
        v.push(Entry::Item(Item::CopyWithHeaders));
    }
    v
}

pub fn with_delete(mut entries: Vec<Entry>, n: usize, restore: bool) -> Vec<Entry> {
    if n == 0 {
        return entries;
    }
    if !entries.is_empty() {
        entries.push(Entry::Sep);
    }
    entries.push(Entry::Item(Item::DeleteRows { n, restore }));
    entries
}

pub fn menu_h(entries: &[Entry]) -> f32 {
    entries.iter().map(|e| if matches!(e, Entry::Sep) { SEP_H } else { ITEM_H }).sum::<f32>() + 2. * PAD
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Press {
    pub row: usize,
    pub col: usize,
}

pub fn press(click_count: usize, shift: bool, cmd: bool, in_gutter: bool, row: usize, col: usize) -> Option<Press> {
    (click_count == 1 && !shift && cmd && !in_gutter).then_some(Press { row, col })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    Nothing,
    Go,
}

pub fn release(press: Option<Press>, at: (usize, usize), single: bool, jumps: usize) -> Release {
    let Some(p) = press else { return Release::Nothing };
    if (p.row, p.col) != at || !single || jumps != 1 {
        return Release::Nothing;
    }
    Release::Go
}

pub fn row(
    t: &Theme,
    id: impl Into<gpui::ElementId>,
    ic: Icon,
    label: SharedString,
    detail: Option<SharedString>,
    key: Option<SharedString>,
    hi: bool,
) -> gpui::Stateful<gpui::Div> {
    let fg = if hi { t.on_acc } else { t.tx };
    let soft = if hi { t.on_acc.opacity(0.8) } else { t.tx3 };
    div()
        .id(id)
        .h(px(ITEM_H))
        .px(px(10.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.))
        .whitespace_nowrap()
        .text_color(fg)
        .when(hi, |d| d.bg(t.acc))
        .child(icon(ic).size(px(14.)).text_color(if hi { t.on_acc } else { t.tx3 }))
        .child(div().flex_none().child(label))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .text_size(px(11.))
                .text_color(soft)
                .children(detail),
        )
        .children(key.map(|k| div().flex_none().text_size(px(11.)).text_color(soft).child(k)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(table: &str) -> FkJump {
        FkJump {
            sql: format!("SELECT * FROM public.{table} WHERE id = 1;"),
            title: format!("{table} · id = 1"),
            label: format!("Go to \"{table}\""),
            detail: "id = 1".into(),
            key: "id".into(),
        }
    }

    #[test]
    fn items() {
        assert_eq!(entries(vec![j("customer")], false), vec![Entry::Item(Item::Go(j("customer")))]);
        assert!(entries(vec![], false).is_empty(), "no reference, no menu");
        let ctx = entries(vec![j("customer"), j("account")], true);
        let copies = [Entry::Item(Item::CopyValue), Entry::Item(Item::CopyWithHeaders)];
        assert_eq!(ctx.len(), 5);
        assert_eq!(ctx[2], Entry::Sep);
        assert_eq!(ctx[3..], copies);
        assert_eq!(entries(vec![], true), copies.to_vec());
        assert_eq!(menu_h(&ctx), 4. * ITEM_H + SEP_H + 2. * PAD);
        assert_eq!(Item::CopyWithHeaders.look().1.as_ref(), "Copy with headers");
    }

    #[test]
    fn header_and_filter_items() {
        let h = header_entries(2, true, 0, true);
        assert_eq!(h[..3], [Entry::Item(Item::SortAsc(2)), Entry::Item(Item::SortDesc(2)), Entry::Item(Item::Filter(2))]);
        assert_eq!(h[3], Entry::Sep);
        assert_eq!(h[4], Entry::Item(Item::Hide(2)));
        assert_eq!(
            header_entries(0, false, 3, false),
            vec![Entry::Item(Item::SortAsc(0)), Entry::Item(Item::SortDesc(0)), Entry::Sep, Entry::Item(Item::ShowAll(3))]
        );
        assert_eq!(header_entries(1, false, 0, false), vec![Entry::Item(Item::SortAsc(1)), Entry::Item(Item::SortDesc(1))]);
        let with = with_filter(entries(vec![j("customer")], true), 1, Some("7".into()));
        assert_eq!(with.len(), 6);
        assert_eq!((&with[1], &with[2], &with[3]), (&Entry::Sep, &Entry::Item(Item::FilterValue(1, Some("7".into()))), &Entry::Sep));
        let plain = with_filter(entries(vec![], true), 0, None);
        assert_eq!(plain[..2], [Entry::Item(Item::FilterValue(0, None)), Entry::Sep]);
        assert_eq!(Item::FilterValue(0, None).look().2.as_deref(), Some("is null"));
    }

    #[test]
    fn delete_item() {
        let d = with_delete(entries(vec![], true), 1, false);
        assert_eq!(d[2..], [Entry::Sep, Entry::Item(Item::DeleteRows { n: 1, restore: false })]);
        assert_eq!(Item::DeleteRows { n: 1, restore: false }.look().1.as_ref(), "Delete row");
        assert_eq!(Item::DeleteRows { n: 3, restore: false }.look().1.as_ref(), "Delete 3 rows");
        assert_eq!(Item::DeleteRows { n: 1, restore: true }.look().1.as_ref(), "Restore row");
        assert_eq!(with_delete(vec![], 0, false), vec![], "no rows, no item");
    }

    #[test]
    fn which_presses_count() {
        assert_eq!(press(1, false, true, false, 3, 2), Some(Press { row: 3, col: 2 }));
        assert_eq!(press(1, false, false, false, 3, 2), None, "a plain click opens nothing");
        assert_eq!(press(2, false, true, false, 3, 2), None, "a double click edits the cell");
        assert_eq!(press(1, true, true, false, 3, 2), None, "shift extends the selection");
        assert_eq!(press(1, false, true, true, 3, 0), None, "the gutter selects rows");
    }

    #[test]
    fn what_a_release_does() {
        let p = Some(Press { row: 3, col: 2 });
        assert_eq!(release(p, (3, 2), true, 1), Release::Go);
        assert_eq!(release(p, (3, 2), true, 2), Release::Nothing);
        assert_eq!(release(p, (3, 2), true, 0), Release::Nothing);
        assert_eq!(release(None, (3, 2), true, 1), Release::Nothing);
        assert_eq!(release(p, (3, 2), false, 1), Release::Nothing);
        assert_eq!(release(p, (4, 2), true, 1), Release::Nothing, "released on another cell");
    }
}
