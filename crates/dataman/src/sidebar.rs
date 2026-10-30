use std::time::Instant;

use dataman_core::layout::{Dock, NavView};
use gpui::{AnyElement, Context, InteractiveElement, IntoElement, MouseButton, ParentElement, StatefulInteractiveElement, Styled, Window, actions, div, prelude::FluentBuilder};

use crate::app::{Region, Workspace};
use crate::assets::{Icon, icon};
use crate::theme::{ActiveTheme, metrics, px};
use crate::ui;
use crate::ui::Tipped;

actions!(
    sidebar,
    [
        ShowExplorer,
        ShowFavorites,
        ShowHistory,
    ]
);

pub(crate) fn workspace_actions(d: gpui::Stateful<gpui::Div>, cx: &mut Context<Workspace>) -> gpui::Stateful<gpui::Div> {
    d.on_action(cx.listener(|w, _: &ShowExplorer, window, cx| w.show_nav(NavView::Explorer, window, cx)))
        .on_action(cx.listener(|w, _: &ShowFavorites, window, cx| w.show_nav(NavView::Favorites, window, cx)))
        .on_action(cx.listener(|w, _: &ShowHistory, window, cx| w.show_nav(NavView::History, window, cx)))
}

const ICON_BOX: f32 = 28.;
const GLYPH: f32 = 15.;

/// The rail's selected icon is drawn filled: it floods in when picked and drains when left.
#[derive(Default)]
pub(crate) struct RailFill {
    seen: Option<(NavView, bool)>,
    view: Option<(NavView, Instant)>,
    gear: Option<Instant>,
}

impl RailFill {
    fn track(&mut self, view: NavView, gear: bool, reduced: bool) {
        if let Some((v, g)) = self.seen.filter(|_| !reduced) {
            if v != view {
                self.view = Some((v, Instant::now()));
            }
            if g != gear {
                self.gear = Some(Instant::now());
            }
        }
        self.seen = Some((view, gear));
    }

    /// How full `v`'s icon is, while it still moves.
    fn level(&self, v: NavView, on: bool) -> Option<f32> {
        let (from, at) = self.view?;
        (on || v == from).then(|| crate::motion::folder_fill(at, !on)).flatten()
    }

    fn gear_level(&self, on: bool) -> Option<f32> {
        crate::motion::folder_fill(self.gear?, !on)
    }

    fn moving(&self) -> bool {
        let busy = |at: Instant| at.elapsed() < crate::motion::FLOOD;
        self.view.is_some_and(|(_, at)| busy(at)) || self.gear.is_some_and(busy)
    }
}

fn rail_icon(outline: Icon, filled: Icon, on: bool, level: Option<f32>, size: f32, color: gpui::Hsla) -> AnyElement {
    match level {
        Some(level) => ui::flood_icon(outline, filled, level, size, color),
        None => icon(if on { filled } else { outline }).size(px(size)).text_color(color).into_any_element(),
    }
}

impl Workspace {
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
            NavView::Agents => {}
        }
        self.agents.update(cx, |a, cx| a.set_visible(v == NavView::Agents, cx));
        if v == NavView::Agents && self.view != NavView::Agents {
            self.before_agents = self.view;
        }
        self.view = v;
        cx.notify();
    }

    pub(crate) fn show_nav(&mut self, v: NavView, window: &mut Window, cx: &mut Context<Self>) {
        self.set_nav(v, cx);
        self.focus_region(Region::Explorer, window, cx);
    }

    pub(crate) fn show_history_of(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.store.read(cx).profile(&id).map(|p| p.name.clone()).unwrap_or(id);
        self.set_nav(NavView::History, cx);
        self.history.update(cx, |h, cx| h.set_filter(&name, cx));
        self.focus_region(Region::Explorer, window, cx);
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_favorites_view(&mut self, cx: &mut Context<Self>) {
        self.set_explorer_open(true, cx);
        self.set_nav(NavView::Favorites, cx);
        self.history.update(cx, |h, cx| h.demo_show_favorites(cx));
    }

    pub(crate) fn rail_click(&mut self, v: NavView, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(crate) fn render_saved_hint(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let f = self.saved_flash.as_ref().filter(|_| self.explorer_open)?;
        let a = crate::motion::SAVED.at(f.at, Instant::now(), cx.reduce_motion())?;
        let star = self.saved_bounds.get()?;
        window.request_animation_frame();
        let t = cx.theme();
        let right = self.layout.explorer_dock() == Dock::Right;
        let text = if right { format!("{} →", f.label) } else { format!("← {}", f.label) };
        let (x, y) = (star.origin.x, star.origin.y + star.size.height / 2. - gpui::px(9.));
        let hint = div().absolute().top(y).h(gpui::px(18.)).flex().items_center().whitespace_nowrap().text_size(px(12.)).text_color(t.tx).opacity(a).child(text);
        let hint = if right { hint.right(window.viewport_size().width - x + gpui::px(8.)) } else { hint.left(star.right() + gpui::px(8.)) };
        Some(hint.into_any_element())
    }

    /// Showing a tab uncovers it: the Agents area gives the tabs back to the sidebar's earlier view.
    pub(crate) fn leave_agents(&mut self, cx: &mut Context<Self>) {
        if self.view == NavView::Agents {
            self.set_nav(self.before_agents, cx);
        }
    }

    /// The Agents area on one of its pages (the robot at the foot of the rail, Settings' MCP log).
    pub(crate) fn show_agents(&mut self, page: crate::agents::Page, window: &mut Window, cx: &mut Context<Self>) {
        self.agents.update(cx, |a, cx| a.show(page, cx));
        self.show_nav(NavView::Agents, window, cx);
    }

    pub(crate) fn render_sidebar(&mut self, d: Dock, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if matches!(self.view, NavView::Favorites | NavView::History) {
            self.view = if self.history.read(cx).showing_favorites() { NavView::Favorites } else { NavView::History };
        }
        let view: AnyElement = match self.view {
            NavView::Explorer => self.tree.clone().into_any_element(),
            NavView::Agents => self.agents.update(cx, |a, cx| a.render_nav(cx)),
            _ => self.history.clone().into_any_element(),
        };
        let rail = self.render_rail(d, window, cx);
        let view = div().flex_1().min_w_0().h_full().child(view);
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
        let settings_on = self.settings_view.is_some();
        self.rail_fill.track(self.view, settings_on, crate::motion::still(cx));
        if self.rail_fill.moving() {
            window.request_animation_frame();
        }
        let fill = &self.rail_fill;
        let icons = NavView::ALL.map(|v| {
            let on = self.view == v;
            let flash = lit.filter(|_| v == NavView::Favorites);
            let (outline, filled) = match v {
                NavView::Explorer => (Icon::Database, Icon::DatabaseFill),
                NavView::Favorites => (Icon::Star, Icon::StarFill),
                NavView::History => (Icon::History, Icon::HistoryFill),
                NavView::Agents => (Icon::Robot, Icon::RobotFill),
            };
            let color = match flash {
                Some(_) => t.tx,
                None if on => t.tx,
                None => t.tx3,
            };
            let glyph = rail_icon(outline, filled, on || flash.is_some(), fill.level(v, on).filter(|_| flash.is_none()), GLYPH, color);
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
                        .cursor_pointer()
                        .when(on, |e| e.bg(t.prs))
                        .when_some(flash, |e, k| e.bg(t.tx.opacity(0.14 * k)))
                        .when(!on && flash.is_none(), move |e| e.hover(move |s| s.bg(hov)))
                        .tip(ui::tooltip(tip, None, None))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |w, _, window, cx| w.rail_click(v, window, cx)))
                        .child(glyph),
                )
        });
        let on = self.view == NavView::Agents;
        let hov = t.hov;
        let robot = div()
            .flex_none()
            .on_children_prepainted({
                let bounds = bounds.clone();
                move |b, _, _| {
                    if let Some(b) = b.first() {
                        bounds.borrow_mut().insert(NavView::Agents, *b);
                    }
                }
            })
            .child(
                div()
                    .children(ui::here(&"rail-agents".into()))
                    .id("rail-agents")
                    .size(px(ICON_BOX))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .when(on, |e| e.bg(t.prs))
                    .when(!on, move |e| e.hover(move |s| s.bg(hov)))
                    .tip(ui::tooltip("Agents", None, None))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |w, _, window, cx| {
                        if w.view == NavView::Agents {
                            w.show_nav(NavView::Explorer, window, cx);
                        } else {
                            w.show_nav(NavView::Agents, window, cx);
                        }
                    }))
                    .child(rail_icon(Icon::Robot, Icon::RobotFill, on, fill.level(NavView::Agents, on), 16., if on { t.tx } else { t.tx3 })),
            );
        let gear = div()
            .children(ui::here(&"rail-settings".into()))
            .id("rail-settings")
            .flex_none()
            .size(px(ICON_BOX))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .when(settings_on, |e| e.bg(t.prs))
            .when(!settings_on, move |e| e.hover(move |s| s.bg(hov)))
            .tip(ui::tooltip("Settings", None, crate::keymap::key("settings").as_deref()))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|w, _, window, cx| w.toggle_settings(window, cx)))
            .child(rail_icon(Icon::Gear, Icon::GearFill, settings_on, fill.gear_level(settings_on), GLYPH, if settings_on { t.tx } else { t.tx3 }));
        div()
            .id("sidebar-rail")
            .w(px(metrics::RAIL_W))
            .flex_none()
            .h_full()
            .pt(px(4.))
            .pb(px(8.))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(4.))
            .border_color(t.line)
            .when(d == Dock::Left, |e| e.border_r_1())
            .when(d == Dock::Right, |e| e.border_l_1())
            .children(icons)
            .child(div().flex_1())
            .child(robot)
            .child(gear)
    }
}
