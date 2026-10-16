//! Several result sets of one run, stacked (Settings › Results grid › Several results): every
//! result with rows gets a grid, one under the other, in the grid's place; tabs (the default)
//! show one at a time.
//!
//! The result on screen (the one the result bar, find, copy, export, grid edits and the zoom
//! work on) is the tab's own grid, at its place in the stack under a highlighted header. Each of
//! the others is a grid of its own showing a copy of the state it keeps
//! (`QueryTab::kept_view`), scrolled on its own; a click on it, or on its header, makes it the
//! result on screen with what was made of it there (that first click only picks it). Results
//! without rows (a row count, an error, the messages) keep their tabs: the strip shows only for
//! them, and a click there on a stacked one brings it into view.
//!
//! Each grid is as tall as its rows (up to an even share of the room, never under a few rows);
//! past that the stack scrolls, and the wheel moves a grid until its end, then the stack.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{AnyElement, App, Context, Entity, Focusable, MouseButton, ScrollHandle, SharedString, div, prelude::*};
use dataman_core::batch::{self, Key};
use dataman_core::fknav::ForeignKeys;
use dataman_core::{ResultLayout, ResultSet};

use crate::app::Workspace;
use crate::grid::{GridView, ResultGrid, ui_count};
use crate::tab::QueryTab;
use crate::theme::{Theme, px};

static STACKED: AtomicBool = AtomicBool::new(false);

/// The setting, read by every tab as it draws (a change shows at once).
pub fn set_layout(layout: ResultLayout) {
    STACKED.store(layout == ResultLayout::Stacked, Ordering::Relaxed);
}

pub fn stacked() -> bool {
    STACKED.load(Ordering::Relaxed)
}

/// A stacked result's header.
const HEADER_H: f32 = 26.;
/// A grid is never shorter than this many rows while it has them (the stack scrolls instead).
const MIN_ROWS: usize = 5;

/// A tab's stacked grids: one per result with rows that is not on screen, kept while it is the
/// same result; and the stack's scroll.
#[derive(Default)]
pub struct Stack {
    grids: HashMap<Key, Entity<ResultGrid>>,
    scroll: ScrollHandle,
    /// The result picked last that the stack scrolled to (a pick from the strip, ⌥⌘]).
    revealed: Option<Key>,
}

impl Stack {
    /// The state of `key`'s stacked grid, when it shows `rs`, for the tab's grid as it comes on
    /// screen; the stacked grid goes either way.
    pub fn take(&mut self, key: Key, rs: &Arc<ResultSet>, cx: &mut App) -> Option<GridView> {
        let g = self.grids.remove(&key)?;
        g.update(cx, |g, _| g.result().is_some_and(|r| Arc::ptr_eq(r, rs)).then(|| g.take_view()))
    }

    /// Checks: the stacked grid of `key`, as last drawn.
    pub fn demo_grid(&self, key: Key) -> Option<Entity<ResultGrid>> {
        self.grids.get(&key).cloned()
    }

    /// References resolved for `rs` while it is stacked.
    pub fn set_foreign_keys(&self, rs: &Arc<ResultSet>, fk: &Arc<ForeignKeys>, cx: &mut App) {
        for g in self.grids.values() {
            if g.read(cx).result().is_some_and(|r| Arc::ptr_eq(r, rs)) {
                g.update(cx, |g, cx| g.set_foreign_keys(rs, fk.clone(), cx));
            }
        }
    }
}

/// The stack in the grid's place, and whether it holds every result tab (the strip is then
/// left out).
pub struct Stacked {
    pub element: AnyElement,
    pub alone: bool,
}

/// The stacked grid of `key` (made from the state it keeps when there is none, or when it shows
/// an older result).
fn grid_for(tab: &mut QueryTab, key: Key, cx: &mut Context<QueryTab>) -> Option<Entity<ResultGrid>> {
    let rs = tab.result_of(key)?;
    if let Some(g) = tab.stack.grids.get(&key)
        && g.read(cx).result().is_some_and(|r| Arc::ptr_eq(r, &rs))
    {
        return Some(g.clone());
    }
    let v = tab.kept_view(key)?;
    let rt = tab.connection().map(|c| c.handle().clone());
    let g = cx.new(ResultGrid::new);
    g.update(cx, |g, cx| g.show_view(v, rt, cx));
    // Rows arriving, a scroll: the stack is drawn again (its heights follow the rows).
    cx.observe(&g, |_, _, cx| cx.notify()).detach();
    tab.stack.grids.insert(key, g.clone());
    Some(g)
}

/// The glyph and color of a result's state (the strip's).
fn state_glyph(t: &Theme, s: batch::State) -> (&'static str, gpui::Hsla) {
    match s {
        batch::State::Running => ("▶", t.acc),
        batch::State::Paused => ("‖", t.info),
        batch::State::Done => ("■", t.ok),
        batch::State::Partial => ("◐", t.info),
        batch::State::Cancelled => ("⊘", t.warn),
        batch::State::Error => ("✕", t.err),
    }
}

/// The active tab's results, stacked: `None` in tabs mode, with fewer than two results with
/// rows, or while the result on screen is not one of them (the messages, an error, a count).
pub fn render(tab: &Entity<QueryTab>, t: &Theme, cx: &mut Context<Workspace>) -> Option<Stacked> {
    let tabs = tab.read(cx).result_tabs();
    let keys: Vec<&batch::Tab> = tabs.iter().filter(|r| r.kind == batch::Kind::Rows).collect();
    let shown = tab.read(cx).shown_key();
    let on = stacked() && keys.len() > 1 && keys.iter().any(|r| Some(r.key) == shown);
    // Grids no longer stacked go, and let their results go (a new run, a closed tab, tabs
    // again): what was made of a result still there stays with it.
    if !tab.read(cx).stack.grids.is_empty() {
        tab.update(cx, |tb, cx| {
            for (k, g) in std::mem::take(&mut tb.stack.grids) {
                let same = tb.result_of(k).is_some_and(|rs| g.read(cx).result().is_some_and(|r| Arc::ptr_eq(r, &rs)));
                if same && on && Some(k) != shown && keys.iter().any(|r| r.key == k) {
                    tb.stack.grids.insert(k, g);
                } else if same {
                    let v = g.update(cx, |g, _| g.take_view());
                    tb.keep_view(k, v);
                }
            }
        });
    }
    if !on {
        return None;
    }
    let shown = shown?;
    // A result the user picked (the strip, ⌥⌘]) scrolls into view, once.
    let picked = tab.read(cx).picked().filter(|k| *k == shown);
    let scroll = tab.read(cx).stack.scroll.clone();
    if picked.is_some() && tab.read(cx).stack.revealed != picked {
        if let Some(i) = keys.iter().position(|r| r.key == shown) {
            scroll.scroll_to_item(i);
        }
        tab.update(cx, |tb, _| tb.stack.revealed = picked);
    }
    let main = tab.read(cx).grid.clone();
    let hov = t.hov;
    let mut sections: Vec<AnyElement> = Vec::new();
    for (i, r) in keys.iter().enumerate() {
        let key = r.key;
        let active = key == shown;
        let grid = if active { Some(main.clone()) } else { tab.update(cx, |tb, cx| grid_for(tb, key, cx)) };
        let Some(grid) = grid else { continue };
        let (fit, rows) = {
            let g = grid.read(cx);
            (g.fit_height(), g.result().map_or(0, |rs| rs.rows()))
        };
        let min = fit.min(crate::grid::fit_rows(MIN_ROWS));
        let (glyph, gc) = state_glyph(t, r.state);
        let group: SharedString = format!("stacked-{i}").into();
        let tx3 = t.tx3;
        let tx = t.tx;
        let close = div()
            .id(("stacked-close", i))
            .flex_none()
            .size(px(14.))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .text_color(gpui::transparent_black())
            .group_hover(group.clone(), move |s| s.text_color(tx3))
            .hover(move |s| s.text_color(tx))
            .child("×")
            .on_click(cx.listener(move |w, _, _, cx| {
                cx.stop_propagation();
                w.close_result(key, cx);
            }));
        let header = div()
            .id(("stacked-header", i))
            .group(group)
            .relative()
            .flex_none()
            .h(px(HEADER_H))
            .pl(px(10.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .text_size(px(11.5))
            .whitespace_nowrap()
            .when(i > 0, |d| d.border_t_1())
            .border_b_1()
            .border_color(t.line)
            .when(active, |d| d.bg(t.prs))
            .when(!active, |d| d.hover(move |s| s.bg(hov)))
            // The result on screen: an accent bar on its header's edge.
            .child(div().absolute().left_0().top_0().bottom_0().w(px(2.)).when(active, |d| d.bg(t.acc)))
            .child(div().w(px(10.)).text_center().text_size(px(9.5)).text_color(gc).child(glyph))
            .children(r.number.map(|n| div().text_color(t.tx3).child(format!("{n} ·"))))
            .child(div().min_w_0().overflow_hidden().text_ellipsis().text_color(if active { t.tx } else { t.tx2 }).child(r.label.clone()))
            .child(div().flex_1())
            .child(div().flex_none().text_color(t.tx3).child(format!("{} {}", ui_count(rows), if rows == 1 { "row" } else { "rows" })))
            .child(close)
            .on_click(cx.listener(move |w, _, window, cx| pick(w, key, window, cx)));
        let body = div()
            .flex_1()
            .min_h_0()
            .relative()
            .overflow_hidden()
            .child(grid)
            // Not on screen: the first click picks it (scrolling it stays its own).
            .when(!active, |d| {
                d.capture_any_mouse_down(cx.listener(move |w, ev: &gpui::MouseDownEvent, window, cx| {
                    if ev.button == MouseButton::Left || ev.button == MouseButton::Right {
                        cx.stop_propagation();
                        pick(w, key, window, cx);
                    }
                }))
            });
        sections.push(
            div()
                .relative()
                .flex_1()
                .min_h(px(HEADER_H + min))
                .max_h(px(HEADER_H + fit))
                .flex()
                .flex_col()
                .child(header)
                .child(body)
                .into_any_element(),
        );
    }
    let alone = keys.len() == tabs.len();
    let element = div().id("result-stack").size_full().flex().flex_col().overflow_y_scroll().track_scroll(&scroll).children(sections).into_any_element();
    Some(Stacked { element, alone })
}

/// A stacked result picked: on screen, its grid focused.
fn pick(w: &mut Workspace, key: Key, window: &mut gpui::Window, cx: &mut Context<Workspace>) {
    let Some(tab) = w.active_tab().cloned() else { return };
    // It is in view already: the stack stays where it is.
    tab.update(cx, |t, _| t.stack.revealed = Some(key));
    w.select_result(key, cx);
    let grid = tab.read(cx).grid.clone();
    window.focus(&grid.focus_handle(cx), cx);
}
