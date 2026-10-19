//! The sidebar: a narrow icon rail (Explorer, Favorites, History) at the window's edge and
//! the view it picks beside it, all in the explorer's dock on its glass (`dock`). Moving the
//! explorer to the other dock moves the whole sidebar; ⌘B and the title bar's toggle hide it
//! whatever it shows, rail included.
//!
//! A click on an icon shows its view (the sidebar opening if it was shut) and puts the focus
//! in it; on the icon already showing, it hides the sidebar. The Favorites and History views
//! are one panel (`history`), each with its own search. ⌘S lights the Favorites icon for a
//! moment (`motion::SAVED`); a click on it then opens the favorite just saved.

use std::time::Instant;

use dataman_core::layout::{Dock, NavView};
use gpui::{AnyElement, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, StatefulInteractiveElement, Styled, Window, actions, div, prelude::FluentBuilder};

use crate::app::{Region, Workspace};
use crate::assets::{Icon, icon};
use crate::theme::{ActiveTheme, metrics, px};
use crate::ui;

actions!(
    sidebar,
    [
        /// The sidebar open on a view, the focus in it (the palette, the View menu).
        ShowExplorer,
        ShowFavorites,
        ShowHistory,
    ]
);

/// The sidebar's commands.
pub(crate) fn workspace_actions(d: gpui::Stateful<gpui::Div>, cx: &mut Context<Workspace>) -> gpui::Stateful<gpui::Div> {
    d.on_action(cx.listener(|w, _: &ShowExplorer, window, cx| w.show_nav(NavView::Explorer, window, cx)))
        .on_action(cx.listener(|w, _: &ShowFavorites, window, cx| w.show_nav(NavView::Favorites, window, cx)))
        .on_action(cx.listener(|w, _: &ShowHistory, window, cx| w.show_nav(NavView::History, window, cx)))
}

/// A rail icon's square, and its glyph.
const ICON_BOX: f32 = 28.;
const GLYPH: f32 = 15.;

impl Workspace {
    /// What the sidebar shows. The Favorites and History views share a panel, which follows.
    pub(crate) fn set_nav(&mut self, v: NavView, cx: &mut Context<Self>) {
        match v {
            NavView::Explorer => {}
            // Read again: favorites change behind the panel (an agent, another window).
            NavView::Favorites => self.history.update(cx, |h, cx| {
                h.set_favorites(true, cx);
                h.reload_favorites(cx);
            }),
            NavView::History => self.history.update(cx, |h, cx| {
                h.set_favorites(false, cx);
                h.reload(cx);
            }),
        }
        self.view = v;
        cx.notify();
    }

    /// The sidebar open on `v`, the focus in it (the palette, the menus).
    pub(crate) fn show_nav(&mut self, v: NavView, window: &mut Window, cx: &mut Context<Self>) {
        self.set_nav(v, cx);
        self.focus_region(Region::Explorer, window, cx);
    }

    /// The sidebar's history searched for connection `id` (the explorer's menu on a
    /// connection): every connection's is listed, the search narrows it to that one's name.
    pub(crate) fn show_history_of(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.store.read(cx).profile(&id).map(|p| p.name.clone()).unwrap_or(id);
        self.set_nav(NavView::History, cx);
        self.history.update(cx, |h, cx| h.set_filter(&name, cx));
        self.focus_region(Region::Explorer, window, cx);
    }

    /// Checks: the sidebar on the favorites, re-read.
    pub(crate) fn demo_favorites_view(&mut self, cx: &mut Context<Self>) {
        self.set_explorer_open(true, cx);
        self.set_nav(NavView::Favorites, cx);
        self.history.update(cx, |h, cx| h.demo_show_favorites(cx));
    }

    /// A click on a rail icon: its view, or on the one showing, the sidebar hidden.
    pub(crate) fn rail_click(&mut self, v: NavView, window: &mut Window, cx: &mut Context<Self>) {
        // Lit after ⌘S: the favorite just saved (even with the favorites showing).
        if v == NavView::Favorites
            && let Some(id) = self.saved_flash.as_ref().map(|f| f.id.clone())
        {
            return self.show_favorite(&id, window, cx);
        }
        if self.explorer_open && self.view == v {
            let inside = self.focused_region(window, cx) == Some(Region::Explorer);
            self.set_explorer_open(false, cx);
            if inside {
                self.focus_editor(window, cx);
            }
            return;
        }
        self.show_nav(v, window, cx);
    }

    /// ⌘S's "← added to favorites": over the whole window beside the rail's lit Favorites
    /// icon, pointing at it, fading with it (no fill: nothing moves, nothing is covered).
    pub(crate) fn render_saved_hint(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let f = self.saved_flash.as_ref().filter(|_| self.explorer_open)?;
        let a = crate::motion::SAVED.at(f.at, Instant::now(), cx.reduce_motion())?;
        let star = self.saved_bounds.get()?;
        window.request_animation_frame();
        let t = cx.theme();
        let right = self.layout.explorer_dock() == Dock::Right;
        let text = if right { format!("{} →", f.label) } else { format!("← {}", f.label) };
        let (x, y) = (star.origin.x, star.origin.y + star.size.height / 2. - gpui::px(9.));
        let hint = div().absolute().top(y).h(gpui::px(18.)).flex().items_center().whitespace_nowrap().text_size(px(12.)).text_color(t.warn).opacity(a).child(text);
        let hint = if right { hint.right(window.viewport_size().width - x + gpui::px(8.)) } else { hint.left(star.right() + gpui::px(8.)) };
        Some(hint.into_any_element())
    }

    /// The sidebar for the explorer's dock `d`: the rail at the window's edge, the view
    /// beside it.
    pub(crate) fn render_sidebar(&mut self, d: Dock, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // A check may switch the panel between its views directly: the rail follows.
        if self.view != NavView::Explorer {
            self.view = if self.history.read(cx).showing_favorites() { NavView::Favorites } else { NavView::History };
        }
        let view: AnyElement = match self.view {
            NavView::Explorer => self.tree.clone().into_any_element(),
            _ => self.history.clone().into_any_element(),
        };
        let rail = self.render_rail(d, window, cx);
        let view = div().flex_1().min_w_0().h_full().child(view);
        // A hairline under the title bar's header ("DATAMAN"), over the rail and the view.
        let row = div().size_full().flex().flex_row().border_t_1().border_color(cx.theme().line);
        match d {
            Dock::Left => row.child(rail).child(view),
            Dock::Right => row.child(view).child(rail),
        }
        .into_any_element()
    }

    fn render_rail(&mut self, d: Dock, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = cx.theme();
        let lit = self.saved_flash.as_ref().and_then(|f| crate::motion::SAVED.at(f.at, Instant::now(), cx.reduce_motion()));
        let said = self.saved_flash.as_ref().map_or("", |f| f.label);
        if lit.is_some() {
            window.request_animation_frame();
        }
        let bounds = self.rail_bounds.clone();
        let saved = self.saved_bounds.clone();
        let icons = NavView::ALL.map(|v| {
            let on = self.view == v;
            let flash = lit.filter(|_| v == NavView::Favorites);
            let glyph = match v {
                NavView::Explorer => Icon::Database,
                NavView::Favorites if flash.is_some() => Icon::StarFill,
                NavView::Favorites => Icon::Star,
                NavView::History => Icon::History,
            };
            let color = match flash {
                Some(_) => t.warn,
                None if on => t.tx,
                None => t.tx3,
            };
            let hov = t.hov;
            let (bounds, saved) = (bounds.clone(), saved.clone());
            let tip = if flash.is_some() { said } else { v.label() };
            div()
                .flex_none()
                .on_children_prepainted(move |b, _, _| {
                    if let Some(b) = b.first() {
                        bounds.borrow_mut().insert(v, *b);
                        if v == NavView::Favorites {
                            saved.set(Some(*b));
                        }
                    }
                })
                .child(
                    div()
                        .id(("rail", v as usize))
                        .size(px(ICON_BOX))
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(on, |e| e.bg(t.prs))
                        .when_some(flash, |e, k| e.bg(t.warn.opacity(0.22 * k)))
                        .when(!on && flash.is_none(), move |e| e.hover(move |s| s.bg(hov)))
                        .tooltip(ui::tooltip(tip, None, None))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |w, _, window, cx| w.rail_click(v, window, cx)))
                        .child(icon(glyph).size(px(GLYPH)).text_color(color)),
                )
        });
        div()
            .id("sidebar-rail")
            .w(px(metrics::RAIL_W))
            .flex_none()
            .h_full()
            .pt(px(4.))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(4.))
            .border_color(t.line)
            .when(d == Dock::Left, |e| e.border_r_1())
            .when(d == Dock::Right, |e| e.border_l_1())
            .children(icons)
    }
}
