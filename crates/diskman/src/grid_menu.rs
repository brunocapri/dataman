//! The result grid's cell menu: on a right click (or ⌥↓ on the active cell), foreign-key
//! navigation ("Go to …") and copying; a ⌘-click on a reference goes straight there, without a
//! menu. The rules and the items are plain data, tested here; the grid (`grid.rs`) opens, draws
//! and runs it. Building a menu does no I/O: the references of a result are resolved once when
//! it arrives (`diskman_core::fknav`).

use gpui::{SharedString, div, prelude::*};
use crate::theme::px;
use diskman_core::fknav::FkJump;

use crate::assets::{Icon, icon};
use crate::theme::Theme;
use crate::tree_menu::{ITEM_H, PAD, SEP_H};

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// Open the referenced row in a new tab.
    Go(FkJump),
    /// The clicked cell's text.
    CopyValue,
    /// The selection as TSV (⌘C).
    CopyTsv,
    /// The selection as TSV under a row of its columns' names (⌥⌘C).
    CopyWithHeaders,
    /// A table's data view: rows whose column equals this cell's value (`None`: IS NULL).
    FilterValue(usize, Option<String>),
    // The header menu (result column indexes).
    SortAsc(usize),
    SortDesc(usize),
    /// The filter popover for the column (data view).
    Filter(usize),
    Hide(usize),
    /// Shows the hidden columns again (how many are hidden).
    ShowAll(usize),
}

impl Item {
    /// (icon, label, detail) as the menu draws it.
    pub fn look(&self) -> (Icon, SharedString, Option<SharedString>) {
        match self {
            Item::Go(j) => (Icon::Table, j.label.clone().into(), Some(j.detail.clone().into())),
            Item::CopyValue => (Icon::Copy, "Copy value".into(), None),
            Item::CopyTsv => (Icon::Copy, "Copy as TSV".into(), None),
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
        }
    }
}

/// The header menu of result column `col`: sort (the loaded rows; a table's data view on the
/// server) and filter (data view), then hide and show the hidden ones.
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

/// A cell's context menu in a table's data view: "Filter by value" after the references.
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

/// Wide enough for `Go to "customer"` and its `id = 42` beside it.
pub const MENU_W: f32 = 300.;
/// Gap between the clicked cell and the menu under it.
pub const GAP: f32 = 2.;

/// The menu of a cell: one "Go to" per reference it holds (usually one), then, for the context
/// menu (right click), the copy items.
pub fn entries(jumps: Vec<FkJump>, context: bool) -> Vec<Entry> {
    let mut v: Vec<Entry> = jumps.into_iter().map(|j| Entry::Item(Item::Go(j))).collect();
    if context {
        if !v.is_empty() {
            v.push(Entry::Sep);
        }
        v.push(Entry::Item(Item::CopyValue));
        v.push(Entry::Item(Item::CopyTsv));
        v.push(Entry::Item(Item::CopyWithHeaders));
    }
    v
}

pub fn menu_h(entries: &[Entry]) -> f32 {
    entries.iter().map(|e| if matches!(e, Entry::Sep) { SEP_H } else { ITEM_H }).sum::<f32>() + 2. * PAD
}

/// A ⌘-click on a cell, released on a single reference: goes straight there. A plain click
/// (shift, the gutter, or no ⌘) never opens anything by itself.
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
    /// A ⌘-click with exactly one reference: open it, no menu.
    Go,
}

/// What a mouse-up does. `press` is cleared by a drag; `single` is whether the selection is
/// still that one cell; `jumps` is how many references the cell leads to (0 for a NULL or a
/// column that is not a reference).
pub fn release(press: Option<Press>, at: (usize, usize), single: bool, jumps: usize) -> Release {
    let Some(p) = press else { return Release::Nothing };
    if (p.row, p.col) != at || !single || jumps != 1 {
        return Release::Nothing;
    }
    Release::Go
}

/// One menu row: icon, label, a muted detail (the key values), and the key column.
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
        // Left click: only the references.
        assert_eq!(entries(vec![j("customer")], false), vec![Entry::Item(Item::Go(j("customer")))]);
        assert!(entries(vec![], false).is_empty(), "no reference, no menu");
        // Right click: references, a separator, the copy items.
        let ctx = entries(vec![j("customer"), j("account")], true);
        let copies = [Entry::Item(Item::CopyValue), Entry::Item(Item::CopyTsv), Entry::Item(Item::CopyWithHeaders)];
        assert_eq!(ctx.len(), 6);
        assert_eq!(ctx[2], Entry::Sep);
        assert_eq!(ctx[3..], copies);
        // Not a reference: just the copy items, no leading separator.
        assert_eq!(entries(vec![], true), copies.to_vec());
        assert_eq!(menu_h(&ctx), 5. * ITEM_H + SEP_H + 2. * PAD);
        assert_eq!(Item::CopyWithHeaders.look().1.as_ref(), "Copy with headers");
    }

    #[test]
    fn header_and_filter_items() {
        let h = header_entries(2, true, 0, true);
        assert_eq!(h[..3], [Entry::Item(Item::SortAsc(2)), Entry::Item(Item::SortDesc(2)), Entry::Item(Item::Filter(2))]);
        assert_eq!(h[3], Entry::Sep);
        assert_eq!(h[4], Entry::Item(Item::Hide(2)));
        // Any grid: sort (local), no filter; the last visible column cannot go, but the hidden
        // ones show.
        assert_eq!(
            header_entries(0, false, 3, false),
            vec![Entry::Item(Item::SortAsc(0)), Entry::Item(Item::SortDesc(0)), Entry::Sep, Entry::Item(Item::ShowAll(3))]
        );
        assert_eq!(header_entries(1, false, 0, false), vec![Entry::Item(Item::SortAsc(1)), Entry::Item(Item::SortDesc(1))]);
        let with = with_filter(entries(vec![j("customer")], true), 1, Some("7".into()));
        assert_eq!(with.len(), 7);
        assert_eq!((&with[1], &with[2], &with[3]), (&Entry::Sep, &Entry::Item(Item::FilterValue(1, Some("7".into()))), &Entry::Sep));
        let plain = with_filter(entries(vec![], true), 0, None);
        assert_eq!(plain[..2], [Entry::Item(Item::FilterValue(0, None)), Entry::Sep]);
        assert_eq!(Item::FilterValue(0, None).look().2.as_deref(), Some("is null"));
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
        // A choice to make, or nothing to go to: nothing (no menu to disambiguate).
        assert_eq!(release(p, (3, 2), true, 2), Release::Nothing);
        assert_eq!(release(p, (3, 2), true, 0), Release::Nothing);
        // A drag (the press is cleared) or a range: nothing.
        assert_eq!(release(None, (3, 2), true, 1), Release::Nothing);
        assert_eq!(release(p, (3, 2), false, 1), Release::Nothing);
        assert_eq!(release(p, (4, 2), true, 1), Release::Nothing, "released on another cell");
    }
}
