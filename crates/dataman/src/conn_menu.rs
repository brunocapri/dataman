//! Every dropdown that picks a connection (the tab's picker, History's and Favorites' filter,
//! Agents › Audit's) lists them as the explorer groups them: each folder a header that folds
//! with a click, its connections a level in, then those in none. What is folded is the same in
//! every dropdown while Dataman runs; all start open. A folded folder holding the pick says so
//! with a ✓, and as the picked row (`ui::selectable`; a menu is always focused).

use std::collections::HashSet;

use dataman_core::{ConnFolder, ConnectionProfile};
use gpui::{AnyElement, App, Global, SharedString, div, prelude::*};

use crate::theme::{Theme, px};
use crate::ui;

pub const INDENT: f32 = 12.;

#[derive(Default)]
struct Folded(HashSet<String>);

impl Global for Folded {}

pub fn is_folded(id: &str, cx: &App) -> bool {
    cx.try_global::<Folded>().is_some_and(|f| f.0.contains(id))
}

fn toggle(id: &str, cx: &mut App) {
    let f = &mut cx.default_global::<Folded>().0;
    if !f.remove(id) {
        f.insert(id.to_string());
    }
}

/// The menu's rows from `profiles` (in their order; those not to list left out), grouped. `conn`
/// gives a connection's own rows (it takes their indent); `picked` is the connection picked now.
/// `menu` names the dropdown, for the folders' element ids.
pub fn rows<'a>(
    t: &Theme,
    menu: &str,
    folders: &'a [ConnFolder],
    profiles: &[&'a ConnectionProfile],
    picked: Option<&str>,
    cx: &App,
    mut conn: impl FnMut(&'a ConnectionProfile, f32) -> Vec<AnyElement>,
) -> Vec<AnyElement> {
    let mut rows: Vec<AnyElement> = Vec::new();
    for (folder, list) in crate::conns::grouped(folders, profiles) {
        let indent = if folder.is_some() { INDENT } else { 0. };
        if let Some(f) = folder {
            let open = !is_folded(&f.id, cx);
            let holds_pick = picked.is_some_and(|c| f.holds(c));
            if !rows.is_empty() {
                rows.push(ui::menu_sep(t).into_any_element());
            }
            rows.push(header(t, menu, f, list.len(), open, holds_pick));
            if !open {
                continue;
            }
        }
        for (i, p) in list.into_iter().enumerate() {
            if !rows.is_empty() && (folder.is_none() || i > 0) {
                rows.push(ui::menu_sep(t).ml(px(6. + indent)).into_any_element());
            }
            rows.extend(conn(p, indent));
        }
    }
    rows
}

fn header(t: &Theme, menu: &str, f: &ConnFolder, n: usize, open: bool, holds_pick: bool) -> AnyElement {
    let id = SharedString::from(format!("{menu}:folder:{}", f.id));
    let (hov, folder) = (t.hov, f.id.clone());
    div()
        .children(ui::here(&id.clone().into()))
        .id(id)
        .h(px(24.))
        .px(px(6.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .text_size(px(crate::theme::text::SMALL))
        .text_color(t.tx2)
        .cursor_pointer()
        // Folded over the pick, it stands for it.
        .map(|d| ui::selectable(d, t, !open && holds_pick, true))
        .hover(move |s| s.bg(hov))
        .child(ui::disclosure_still(open, 9., t.tx3))
        .child(ui::folder_icon(open, None, 12., crate::tree::folder_color(f, t).unwrap_or(t.tx3)))
        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(f.name.clone()))
        .child(div().flex_none().text_size(px(crate::theme::text::TINY)).text_color(t.tx4).child(if !open && holds_pick {
            "✓".to_string()
        } else {
            n.to_string()
        }))
        // Folding never picks, nor closes the menu.
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            toggle(&folder, cx);
            window.refresh();
        })
        .into_any_element()
}
