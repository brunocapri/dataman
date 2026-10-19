//! The window chrome, drawn from the design boards: title bar with the query tabs
//! (UiTitlebar), toolbar (UiToolbar), result bar (UiResultBar), status bar (UiStatusBar), the
//! work surface layout, the side panel container, the error view (EditorError), toasts and the
//! shortcuts sheet.

use std::sync::Arc;

use gpui::{
    AnyElement, ClipboardItem, Context, Div, Focusable, FontWeight, Hsla, MouseButton, SharedString, Stateful, Window, div,
    prelude::*,
};
use crate::theme::{Pt, px};
use dataman_core::batch::{self, Key};
use dataman_core::groups::Pane;
use dataman_core::result::{CopyFormat, Phase};
use dataman_core::{Connection, TxState, TxStatus};

use crate::app::{Drag, Region, Side, Workspace, fmt_dur};
use crate::assets::{Icon, icon};
use crate::grid::{fmt_clock, statement_verb, ui_count};
use crate::messages::{self, Of, Row as MessageRow};
use crate::panels::{LAR_BUTTONS, PanelRegistry};
use crate::result_bar::{ResultState, role_colors};
use crate::tab::{ItemState, QueryTab};
use crate::theme::{ActiveTheme, PIXEL, Theme, metrics};
use crate::ui::{self, Btn};

/// Below this width a group's toolbar drops its keycaps (Run's, History's): the connection
/// picker keeps room for its name.
const COMPACT_W: f32 = 440.;

/// The connection picker's least width: its chip, a few letters, the chevron.
const PICKER_MIN: f32 = 64.;

/// The least width a query tab gives way to before its strip scrolls.
const TAB_MIN: f32 = 120.;

/// What the toolbar and result bar need from a group's tab in front, read once per frame.
struct TabView {
    /// The editor group it is shown in, and whether that group has the focus (the popups, the
    /// bounds the checks read and the focus ring are the focused group's).
    pane: Pane,
    focused: bool,
    /// The group's editor and results are narrow (its toolbar drops the keycaps).
    compact: bool,
    conn: Option<Arc<Connection>>,
    /// The tab's database, the connection a restored tab waits for, and the agent whose tab it
    /// is (as its MCP client introduced itself).
    db: String,
    want: Option<(String, Option<String>)>,
    agent: Option<String>,
    running: bool,
    cancelling: bool,
    elapsed: Option<std::time::Duration>,
    batch: Vec<crate::tab::BatchItem>,
    /// The result tabs of the run, and the one on screen.
    results: Vec<batch::Tab>,
    shown: Option<Key>,
    /// The session's transaction (a typed BEGIN), and a COMMIT / ROLLBACK on its way.
    tx: TxState,
    ending: bool,
}

/// "Fetch all", with the planner's estimate of the rows left when there is one (`~30k`).
pub(crate) fn fetch_all_label(rs: Option<&dataman_core::ResultSet>) -> SharedString {
    let left = rs.and_then(|r| r.row_estimate().map(|e| e.saturating_sub(r.rows() as u64))).filter(|n| *n > 0);
    match left {
        Some(n) => format!("Fetch all {}", dataman_core::schema::objects::approx_count(n as f64)).into(),
        None => "Fetch all".into(),
    }
}

fn tab_view(tab: &QueryTab, pane: Pane, focused: bool, compact: bool) -> TabView {
    let results = tab.result_tabs();
    TabView {
        pane,
        focused,
        compact,
        conn: tab.connection().cloned(),
        db: tab.database().to_string(),
        want: tab.want.clone(),
        agent: tab.agent.as_ref().map(|a| agent_name(&a.client)),
        running: tab.is_running(),
        cancelling: tab.is_cancelling(),
        elapsed: tab.elapsed(),
        batch: tab.batch_items().to_vec(),
        results,
        shown: tab.shown_key(),
        tx: tab.tx(),
        ending: tab.ending_tx(),
    }
}

/// How long a transaction has been open, coarse enough that it changes once a minute: `<1m`,
/// `12m`, `1h 05m`.
pub(crate) fn fmt_tx_age(d: std::time::Duration) -> String {
    let m = d.as_secs() / 60;
    match m {
        0 => "<1m".into(),
        1..=59 => format!("{m}m"),
        _ => format!("{}h {:02}m", m / 60, m % 60),
    }
}

/// An open transaction this long reads as a warning (locks held, a forgotten COMMIT).
const TX_LONG: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// The TX chip: `TX · 3 · 2m`, the accent while open, warning after a while, error fill when
/// the transaction failed.
pub(crate) fn tx_chip(t: &Theme, tx: &TxState, compact: bool) -> Div {
    let age = tx.since.map(|s| s.elapsed()).unwrap_or_default();
    let (fg, bg) = match tx.status {
        TxStatus::Failed => (t.on_err, t.err),
        _ if age >= TX_LONG => (t.warn, t.warn_s),
        _ => (t.acc_t, t.acc_s),
    };
    let label = if tx.status == TxStatus::Failed { "TX FAILED" } else { "TX" };
    let mut text = if compact { format!("TX {}", tx.statements) } else { format!("{label} · {}", tx.statements) };
    if !compact {
        text.push_str(&format!(" · {}", fmt_tx_age(age)));
    }
    div()
        .flex_none()
        .h(px(if compact { 16. } else { 20. }))
        .px(px(if compact { 5. } else { 7. }))
        .flex()
        .items_center()
        .bg(bg)
        .text_color(fg)
        .text_size(px(if compact { 10. } else { 10.5 }))
        .font_weight(FontWeight::SEMIBOLD)
        .whitespace_nowrap()
        .child(text)
}

/// What a statement of the messages tab produced: `120 rows`, `UPDATE 3`, `CREATE TABLE`,
/// `42P01`.
fn outcome_text(rs: &dataman_core::ResultSet, t: &Theme) -> (String, Hsla) {
    let st = rs.status();
    let rows = rs.rows();
    let has_cols = rs.columns().is_some_and(|c| !c.is_empty());
    match rs.phase() {
        Phase::Failed => (st.error.and_then(|e| e.code).unwrap_or_else(|| "error".into()), t.err),
        Phase::Cancelled => ("cancelled".into(), t.warn),
        Phase::Paused => (format!("{}+ rows", ui_count(rows)), t.tx2),
        _ if has_cols => {
            let unit = if rows == 1 { "row" } else { "rows" };
            let kept = if st.partial { " kept" } else { "" };
            (format!("{} {unit}{kept}", ui_count(rows)), t.tx2)
        }
        Phase::Running => (String::new(), t.tx3),
        _ => (batch::tag(&rs.sql, st.affected), t.tx2),
    }
}

/// What the results on screen printed: the run's when the messages tab is shown, else the
/// shown result's.
fn shown_messages(tab: &QueryTab) -> String {
    if tab.shown_key() == Some(Key::Messages) {
        messages::text(tab.batch_items().iter().filter_map(|b| b.rs.as_deref()))
    } else {
        messages::text(tab.result().map(|r| &**r).into_iter())
    }
}

/// The glyph and color of a result tab's state.
fn state_glyph(t: &Theme, s: batch::State) -> (&'static str, Hsla) {
    match s {
        batch::State::Running => ("▶", t.acc),
        batch::State::Paused => ("‖", t.info),
        batch::State::Done => ("■", t.ok),
        batch::State::Partial => ("◐", t.info),
        batch::State::Cancelled => ("⊘", t.warn),
        batch::State::Error => ("✕", t.err),
    }
}

impl Workspace {
    // -----------------------------------------------------------------------------------------
    // Title bar.

    pub(crate) fn render_titlebar(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = cx.theme();
        // The bar's geometry follows the docks as they slide (⌘B, a panel moved; `dock_pass`):
        // the header sits at the explorer's end of the bar, the tabs between.
        let title = self.title_now;
        let mirrored = title.header == Some(dataman_core::layout::Dock::Right);
        // The lights keep their size at any zoom: in zoomed points they take less room.
        let lights = metrics::lights_right_at(crate::theme::zoom());
        // The toggle fills with the explorer, wherever it is; the tabs line up past the seam.
        let open = (self.explorer_now() / self.explorer_w.max(1.)).clamp(0., 1.);
        let logo = self.title_logo();
        let logo_bounds = self.logo_bounds.clone();
        if logo.is_none() {
            logo_bounds.set(None);
        }
        // What the checks measure: the toggle, the arrows, the search, as laid out.
        let marks = self.title_marks.clone();
        marks.borrow_mut().clear();
        let mark = move |name: &'static str, el: AnyElement| {
            let marks = marks.clone();
            div().flex_none().on_children_prepainted(move |b, _, _| {
                if let Some(b) = b.first() {
                    marks.borrow_mut().insert(name, *b);
                }
            })
            .child(el)
            .into_any_element()
        };
        // First run: no query tabs until there is a connection.
        let first_run = self.first_run(cx);
        // The query tabs: one strip, or one over each editor group (`split`).
        let strips: Vec<AnyElement> = if first_run { Vec::new() } else { self.render_strips(title.lead, title.seam, &t, cx) };
        let stop = |_: &gpui::MouseDownEvent, _: &mut Window, cx: &mut gpui::App| cx.stop_propagation();
        let tab_menu = self.render_tab_menu(&t, cx);
        // The window's header, carried by the explorer: its toggle (the glyph mirrored on the
        // right) and the back / forward arrows, by the traffic lights or at the right edge.
        let toggle = mark(
            "toggle",
            explorer_toggle(&t, open, mirrored)
                .on_mouse_down(MouseButton::Left, stop)
                .on_click(cx.listener(|w, _, _, cx| w.set_explorer_open(!w.explorer_open, cx)))
                .into_any_element(),
        );
        let arrows: Vec<AnyElement> = [false, true]
            .map(|fwd| {
                let can = self.can_go(fwd);
                let (id, i) = if fwd { ("nav-forward", Icon::ChevronRight) } else { ("nav-back", Icon::ChevronLeft) };
                let b = if can {
                    ui::icon_button(&t, id, i, 20., 14.).on_click(cx.listener(move |w, _, window, cx| w.go(fwd, window, cx)))
                } else {
                    ui::icon_button_inert(&t, id, i, 20., 14.).opacity(0.35)
                };
                let b = b.on_mouse_down(MouseButton::Left, stop);
                mark(if fwd { "forward" } else { "back" }, b.into_any_element())
            })
            .into_iter()
            .collect();
        // The search (⌘K): at the far right, or past the lights when the header is there.
        let search = mark(
            "search",
            div()
                .id("palette-hint")
                .flex_none()
                .h(px(26.))
                .pl(px(6.))
                .pr(px(4.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .text_color(t.tx3)
                .on_mouse_down(MouseButton::Left, stop)
                .on_click(cx.listener(|w, _, window, cx| w.open_palette(crate::palette::Scope::All, window, cx)))
                .child(icon(Icon::Search).size(px(14.)).text_color(t.tx3))
                .when_some(crate::keymap::key("command_palette"), |d, k| d.child(ui::keycap(&t, k)))
                .into_any_element(),
        );
        let (mut toggle, mut arrows, mut search) = (Some(toggle), arrows, Some(search));
        // The left end: the traffic lights (16, 14; `metrics::LIGHTS_AT`), then the header (the
        // toggle 14 pt after them, the arrows) or, mirrored, the search in the toggle's place.
        let left = div()
            .w(px(title.lead))
            .flex_none()
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .child(div().w(px(lights)).flex_none())
            .map(|d| {
                if mirrored {
                    d.child(div().ml(px(14.)).flex_none().children(search.take()))
                } else {
                    d.child(div().ml(px(14.)).flex_none().children(toggle.take())).child(div().ml(px(4.)).flex_none().flex().flex_row().children(std::mem::take(&mut arrows)))
                }
            })
            .child(div().flex_1());
        // The right end, mirrored: the header over the explorer's column, its toggle at the
        // window's edge; else the search.
        let right = if mirrored {
            div()
                .w(px(title.trail))
                .flex_none()
                .h_full()
                .flex()
                .flex_row()
                .items_center()
                .child(div().flex_1())
                .child(div().flex_none().flex().flex_row().children(std::mem::take(&mut arrows)))
                .child(div().ml(px(4.)).mr(px(10.)).flex_none().children(toggle.take()))
                .into_any_element()
        } else {
            div().ml(px(12.)).mr(px(10.)).flex_none().children(search.take()).into_any_element()
        };
        // Laid over the top of the docks and the work column (app.rs), which run up to the
        // window's top edge: the header sits on the explorer's glass, the tabs on the band's fill.
        div()
            .id("titlebar")
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(metrics::TITLE_H))
            .flex()
            .flex_row()
            .items_center()
            .text_color(t.tx2)
            // The editor groups' seam, from the window's top edge down (under the bar's own).
            .children(self.render_seam_line(cx))
            .on_mouse_down(MouseButton::Left, cx.listener(|w, _, _, _| w.title_drag = true))
            .on_mouse_up(MouseButton::Left, cx.listener(|w, _, _, _| w.title_drag = false))
            .on_mouse_move(cx.listener(|w, _, window, _| {
                if w.title_drag {
                    w.title_drag = false;
                    window.start_window_move();
                }
            }))
            .on_click(|ev, window, _| {
                if ev.click_count() == 2 {
                    window.titlebar_double_click();
                }
            })
            .child(left)
            .children(logo.map(|r| {
                // At the explorer column's outer side: its right edge on the left, its left edge
                // on the right (the header's controls at the window's edge).
                div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .left(px(r.x))
                    .w(px(r.w))
                    .flex()
                    .flex_row()
                    .items_center()
                    .when(!mirrored, |d| d.justify_end().pr(px(14.)))
                    .when(mirrored, |d| d.justify_start().pl(px(14.)))
                    .on_children_prepainted(move |b, _, _| logo_bounds.set(b.first().copied()))
                    .child(div().font_family(PIXEL).text_size(px(13.)).text_color(t.tx3).child("DATAMAN"))
            }))
            .children(strips)
            .when(first_run, |d| d.child(div().flex_1()))
            // An update on offer: its notice, which opens the update sheet.
            .children(self.render_update_pill(&t, cx))
            .child(right)
            .children(tab_menu)
    }

    /// Where the title bar's "DATAMAN" goes: it is the explorer's header, at the outer side of
    /// the explorer's column wherever that is (the left dock's first column, the right dock's
    /// last; stacked with the history or beside it), never over the history's own column.
    /// `None` while the explorer is shut or too narrow for it beside the header's controls.
    pub(crate) fn title_logo(&self) -> Option<crate::dock::Rect> {
        use dataman_core::layout::{Dock, PanelId};
        let d = self.layout.place_of(PanelId::Explorer).dock;
        let explorer = |c: &&crate::dock::ColFrame| c.slots.iter().any(|(s, _)| s.panel == PanelId::Explorer && !s.ghost && s.weight >= 0.5);
        let col = self.frame_now.dock(d).cols.iter().find(explorer).map(|c| c.rect);
        match d {
            Dock::Left => col.filter(|r| r.w >= 120. && r.right() >= 230.),
            // The arrows and the toggle at its right end.
            Dock::Right => col.filter(|r| r.w >= 190.),
        }
    }

    /// The tab strips: one, or one over each editor group (`split`), the left one ending at the
    /// groups' seam (`render_seam_line` paints its hairline between them). `side`: the room the
    /// title bar keeps on the left (the traffic lights, the explorer toggle), `seam`: how open the left dock is (the first tab lines up past it).
    fn render_strips(&self, side: f32, seam: f32, t: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        // Past the seam the first tab lines up with the toolbar's Run button (the hairline + its
        // 10 pt padding).
        let pad = 2. + 9. * seam;
        let Some(f) = self.split_now else {
            return vec![self.render_strip(Pane::Left, self.group(Pane::Left), None, pad, t, cx)];
        };
        let c = self.frame_now.center;
        let x = self.surface_x() + f.widths(c.w).0.0;
        let left_w = (x - side).max(0.);
        let (left, right) = match &self.split {
            Some(_) => (self.group(Pane::Left), self.group(Pane::Right)),
            // Folding away: every tab is in the group that stays, over its side.
            None if f.leaving == Pane::Right => (self.group(Pane::Left), Vec::new()),
            None => (Vec::new(), self.group(Pane::Left)),
        };
        let (lp, rp) = if self.split.is_some() { (Pane::Left, Pane::Right) } else { (Pane::Left, Pane::Left) };
        let mut v = Vec::new();
        v.push(if left.is_empty() { div().w(px(left_w)).flex_none().into_any_element() } else { self.render_strip(lp, left, Some(left_w), pad, t, cx) });
        if !right.is_empty() {
            v.push(self.render_strip(rp, right, None, 2. + 9. + crate::split::SEAM, t, cx));
        } else {
            v.push(div().flex_1().into_any_element());
        }
        v
    }

    /// A group's strip: its tabs (`tabs`, indexes into `tabs`), scrolling sideways on their
    /// own when they overflow, + for a new tab in that group, and at its end the group's × while
    /// there are two. `width`: a fixed width (the left group's, up to the seam), else the room
    /// left.
    fn render_strip(&self, pane: Pane, tabs: Vec<usize>, width: Option<f32>, pad: f32, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let stop = |_: &gpui::MouseDownEvent, _: &mut Window, cx: &mut gpui::App| cx.stop_propagation();
        let n = tabs.len();
        let els: Vec<AnyElement> = tabs.iter().enumerate().map(|(gi, &i)| self.render_tab(i, gi, n, pane, t, cx).into_any_element()).collect();
        let bounds = if pane == Pane::Left { self.tab_bounds.clone() } else { self.tab_bounds_right.clone() };
        let scroll = self.strip_scroll[pane.index()].clone();
        // The tab in front came to the front since the last frame (a click, a new tab, ⌃Tab, a
        // move): scrolled into view.
        let front = match &self.split {
            Some(_) => self.front_of(pane),
            None => self.active_tab(),
        }
        .map(|t| t.entity_id());
        let seen = &self.strip_front[pane.index()];
        if seen.get() != front {
            seen.set(front);
            if let Some(gi) = front.and_then(|f| tabs.iter().position(|&i| self.tabs[i].entity_id() == f)) {
                scroll.scroll_to_item(gi);
            }
        }
        // Pressed once, the × asks "Sure?" in its place.
        let control = self.split.is_some().then(|| {
            if self.close_arm.as_ref().is_some_and(|a| a.pane == pane) {
                self.render_close_ask(pane, cx).into_any_element()
            } else {
                ui::icon_button(t, "close-group", Icon::Close, 22., 12.)
                    .tooltip(ui::tooltip("Close group", Some("closes its tabs"), None))
                    .on_mouse_down(MouseButton::Left, stop)
                    .on_click(cx.listener(move |w, _, window, cx| w.ask_close_group(pane, window, cx)))
                    .into_any_element()
            }
        });
        let parts = self.strip_parts.clone();
        let offset = scroll.clone();
        let strip_tabs = div()
            // Where each tab is, scrolled: the gap a dragged one points at (and the checks). The
            // scroll handle has them (a tracked scroller hands its children's bounds nowhere else).
            .on_children_prepainted(move |_, _, _| {
                let dx = offset.offset().x;
                *bounds.borrow_mut() = (0..offset.children_count())
                    .filter_map(|i| offset.bounds_for_item(i))
                    .map(|mut b| {
                        b.origin.x += dx;
                        b
                    })
                    .collect();
            })
            .id(("strip-tabs", pane.index()))
            .min_w_0()
            .flex_shrink_1()
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.))
            // The wheel scrolls it sideways too (a vertical turn reads as a sideways one).
            .overflow_x_scroll()
            .track_scroll(&scroll)
            .children(els);
        div()
            .on_children_prepainted(move |b, _, _| parts.borrow_mut()[pane.index()] = b)
            .id(("strip", pane.index()))
            .when_some(width, |d, w| d.w(px(w)).flex_none())
            .when(width.is_none(), |d| d.flex_1())
            .min_w_0()
            .h_full()
            .pl(px(pad))
            .pr(px(4.))
            .flex()
            .flex_row()
            .items_center()
            .overflow_hidden()
            // A press on a group's strip gives that group the focus.
            .capture_any_mouse_down(cx.listener(move |w, _, window, cx| w.focus_pane(pane, window, cx)))
            .on_drag_move(cx.listener(move |w, ev: &gpui::DragMoveEvent<crate::app::DraggedTab>, _, cx| {
                let pos = ev.event.position;
                if !ev.bounds.contains(&pos) {
                    // Over the other strip, or neither: this one's gap goes.
                    if w.tab_drop_pane == pane && w.tab_drop.is_some() {
                        w.tab_drop = None;
                        cx.notify();
                    }
                    return;
                }
                let slot = (|| {
                    let id = ev.dragged_item().downcast_ref::<crate::app::DraggedTab>()?.0;
                    let members = w.group(pane);
                    // From the other group, any gap moves it.
                    let from = members.iter().position(|&i| w.tabs[i].entity_id() == id);
                    let bounds = if pane == Pane::Left { w.tab_bounds.borrow() } else { w.tab_bounds_right.borrow() };
                    let mids: Vec<f32> = bounds.iter().take(members.len()).map(|b| b.center().x.pt()).collect();
                    let s = dataman_core::order::slot(&mids, pos.x.pt());
                    from.is_none_or(|f| dataman_core::order::moves(f, s)).then_some(s)
                })();
                if slot != w.tab_drop || w.tab_drop_pane != pane {
                    w.tab_drop = slot;
                    w.tab_drop_pane = pane;
                    cx.notify();
                }
            }))
            .on_drop(cx.listener(|w, d: &crate::app::DraggedTab, window, cx| w.drop_tab(d.0, window, cx)))
            .child(strip_tabs)
            .child(
                ui::icon_button(t, "new-tab", Icon::Plus, 26., 14.)
                    .ml(px(6.))
                    .on_mouse_down(MouseButton::Left, stop)
                    .on_click(cx.listener(move |w, _, window, cx| {
                        w.focus_pane(pane, window, cx);
                        w.new_tab(window, cx);
                    })),
            )
            .child(div().flex_1())
            .children(control)
            .into_any_element()
    }

    /// Tab `i` of the strip of group `pane`, `gi` of its `n` there.
    fn render_tab(&self, i: usize, gi: usize, n: usize, pane: Pane, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        // In front of the focused group (the active tab), or of the other one.
        let focused_front = i == self.active;
        let other_front = self.split.as_ref().is_some_and(|s| s.other == self.tabs[i].entity_id());
        let active = focused_front || other_front;
        let tab = self.tabs[i].read(cx);
        let title = tab.title(cx);
        let conn = tab.connection().cloned();
        // A restored tab waiting for its connection wears that connection's color already.
        let wanted = tab.want.as_ref().and_then(|(id, _)| self.store.read(cx).profile(id)).map(|p| t.profile_color(p));
        let color = conn.as_ref().map(|c| t.profile_color(&c.profile)).or(wanted).unwrap_or(t.tx4);
        let lar = conn.as_ref().is_some_and(|c| c.info.is_lar);
        let agent = tab.agent.is_some();
        let phase = tab.run_phase();
        let tx = tab.tx();
        let lar_ind = crate::lar::tab_indicator(tab, t, cx);
        let has_lar_ind = lar_ind.is_some();
        let (ind, ind_color): (Option<SharedString>, Hsla) = if let Some((s, c)) = lar_ind {
            (Some(s), c)
        } else if tab.is_cancelling() {
            (Some("◌ cancel…".into()), t.warn)
        } else if tab.is_running() {
            (Some(format!("▶ {}", tab.elapsed().map(fmt_clock).unwrap_or_else(|| "00:00.0".into())).into()), t.acc)
        } else {
            match phase {
                Some(Phase::Failed) => (Some("✕".into()), t.err),
                Some(Phase::Paused) => (Some("‖".into()), t.info),
                _ => (None, t.tx3),
            }
        };
        // An open transaction outranks the last statement's glyph (a failed one says so itself).
        let show_tx = tx.status.is_open() && !has_lar_ind && !tab.is_running() && !tab.is_cancelling();
        let (ind, ind_color) = if show_tx { (None, ind_color) } else { (ind, ind_color) };
        // A still glyph (the run failed, a paused result) gives way to the × under the pointer;
        // a running clock stays (⌘W still closes the tab).
        let still = !has_lar_ind && !tab.is_running() && !tab.is_cancelling() && ind.is_some();
        let (hov, tx2) = (t.hov, t.tx2);
        // Identity, not position: tabs move when dragged.
        let entity = self.tabs[i].clone();
        let key = entity.entity_id().as_u64() as usize;
        let group: SharedString = format!("tab-{key}").into();
        let (tx3, tx_c) = (t.tx3, t.tx);
        let close = {
            let entity = entity.clone();
            div()
                .id(("close-tab", key))
                .flex_none()
                .size(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.))
                .hover(move |s| s.text_color(tx_c))
                .child("×")
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |w, _, window, cx| {
                    cx.stop_propagation();
                    if let Some(ix) = w.tabs.iter().position(|t| *t == entity) {
                        w.close_tab(ix, window, cx);
                    }
                }))
        };
        let end: AnyElement = match ind {
            // The glyph, and the × in its place while the tab is hovered.
            Some(s) if still => div()
                .flex_none()
                .relative()
                .size(px(16.))
                .child(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(11.))
                        .text_color(ind_color)
                        .group_hover(group.clone(), |s| s.invisible())
                        .child(s),
                )
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .invisible()
                        .group_hover(group.clone(), |s| s.visible())
                        .child(close.text_color(tx3)),
                )
                .into_any_element(),
            Some(s) => div().flex_none().text_size(px(11.)).text_color(ind_color).child(s).into_any_element(),
            None => close
                .text_color(if active { tx3 } else { gpui::transparent_black() })
                .group_hover(group.clone(), move |s| s.text_color(tx3))
                .into_any_element(),
        };
        let activate = entity.clone();
        let middle = entity.clone();
        let drag_id = entity.entity_id();
        let ghost_label = title.clone();
        // The gap a dragged tab would land in: before this tab, or after it when it is the last.
        let line = self.tab_drop.filter(|_| cx.has_active_drag() && self.tab_drop_pane == pane).and_then(|s| {
            if s == gi { Some(false) } else if s == n && gi + 1 == n { Some(true) } else { None }
        });
        div()
            .id(("tab", key))
            .group(group.clone())
            .relative()
            .h(px(metrics::TAB_H))
            // They give way down to a readable width, then the strip scrolls.
            .min_w(px(TAB_MIN))
            .max_w(px(220.))
            .flex_shrink_1()
            .pl(px(10.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.))
            // The tabs sit on the work column's own fill: the active one is a wash over it
            // (Zeron's selected tab), the others light up on hover. With two editor groups the
            // other group's tab in front is a lighter wash: the focus is told by the wash alone,
            // never by a line.
            .when(focused_front, |d| d.bg(t.prs).text_color(t.tx))
            .when(other_front, |d| d.bg(t.hov).text_color(t.tx2))
            .when(!active, |d| d.text_color(t.tx3).hover(move |s| s.bg(hov).text_color(tx2)))
            // Not the title bar's window move: a press may start a drag (`dnd`); ⌃-click is the
            // right click.
            .on_mouse_down(MouseButton::Left, cx.listener(move |w, ev: &gpui::MouseDownEvent, _, cx| {
                cx.stop_propagation();
                if ev.modifiers.control {
                    w.tab_menu = Some((drag_id, ev.position));
                    cx.notify();
                }
            }))
            .on_mouse_down(MouseButton::Right, cx.listener(move |w, ev: &gpui::MouseDownEvent, _, cx| {
                cx.stop_propagation();
                w.tab_menu = Some((drag_id, ev.position));
                cx.notify();
            }))
            .on_drag(crate::app::DraggedTab(drag_id), move |_, _, _, cx| crate::dnd::ghost(ghost_label.clone(), Some(color), cx))
            .on_click(cx.listener(move |w, ev: &gpui::ClickEvent, window, cx| {
                if !ev.modifiers().control {
                    w.activate_tab(&activate, window, cx)
                }
            }))
            // A middle click (the wheel) closes it, like ⌘W and its ×, when released on it.
            .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Middle, cx.listener(move |w, _, window, cx| {
                cx.stop_propagation();
                if let Some(ix) = w.tabs.iter().position(|t| *t == middle) {
                    w.close_tab(ix, window, cx);
                }
            }))
            .child(ui::chip(color, 8.))
            .when(lar, |d| d.child(ui::lar_dot(t).ml(px(-3.))))
            .when(agent, |d| d.child(icon(Icon::Agent).size(px(11.)).ml(px(-2.)).text_color(if active { t.tx2 } else { t.tx3 })))
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(title))
            .when(show_tx, |d| d.child(tx_chip(t, &tx, true)))
            .child(end)
            .children(line.map(|after| crate::dnd::drop_line(t, true, after)))
    }

    /// A tab's right-click menu: Close, Close Others, Close to the Right (in its group), Open to
    /// the Side, Move to the other group.
    fn render_tab_menu(&self, t: &Theme, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let (id, at) = self.tab_menu?;
        let ix = self.tabs.iter().position(|t| t.entity_id() == id)?;
        let pane = self.pane_of(id);
        let group = self.group(pane);
        let gi = group.iter().position(|&i| i == ix)?;
        let n = group.len();
        // An activity monitor moves rather than copies: with one tab there is nothing to split.
        let alone = self.split.is_none() && self.tabs.len() < 2;
        let side_ok = !(alone && self.tabs[ix].read(cx).monitor.is_some());
        let move_label = match (self.split.is_some(), pane) {
            (false, _) => "Move to the Side",
            (true, Pane::Left) => "Move to the Right Group",
            (true, Pane::Right) => "Move to the Left Group",
        };
        let item = |key: &'static str, label: &'static str, enabled: bool, f: fn(&mut Self, usize, &mut Window, &mut Context<Self>)| {
            ui::menu_item(t, key, label, None, enabled).when(enabled, |d| {
                d.on_click(cx.listener(move |w, _, window, cx| {
                    w.tab_menu = None;
                    if let Some(ix) = w.tabs.iter().position(|t| t.entity_id() == id) {
                        f(w, ix, window, cx);
                    }
                    cx.notify();
                }))
            })
        };
        let menu = ui::popover(t)
            .w(px(200.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(|w, _, _, cx| {
                w.tab_menu = None;
                cx.notify();
            }))
            .child(item("tm-close", "Close", true, |w, ix, window, cx| w.close_tab(ix, window, cx)))
            .child(item("tm-others", "Close Others", n > 1, |w, ix, window, cx| {
                let group = w.group(w.pane_of(w.tabs[ix].entity_id()));
                let close = group.into_iter().filter(|j| *j != ix).map(|j| w.tabs[j].clone()).collect();
                w.close_tabs(close, window, cx)
            }))
            .child(item("tm-right", "Close to the Right", gi + 1 < n, |w, ix, window, cx| {
                let group = w.group(w.pane_of(w.tabs[ix].entity_id()));
                let close = group.into_iter().filter(|j| *j > ix).map(|j| w.tabs[j].clone()).collect();
                w.close_tabs(close, window, cx)
            }))
            .child(ui::menu_sep(t))
            .child(
                ui::menu_item(t, "tm-side", "Open to the Side", crate::keymap::key("open_to_side").as_deref(), side_ok).when(side_ok, |d| {
                    d.on_click(cx.listener(move |w, _, window, cx| {
                        w.tab_menu = None;
                        if let Some(tab) = w.tabs.iter().find(|t| t.entity_id() == id).cloned() {
                            w.open_to_side(&tab, window, cx);
                        }
                        cx.notify();
                    }))
                }),
            )
            .child(item("tm-move", move_label, !alone, |w, ix, window, cx| {
                let id = w.tabs[ix].entity_id();
                let to = w.pane_of(id).other();
                w.move_into(id, to, None, window, cx)
            }));
        Some(gpui::deferred(gpui::anchored().position(at).snap_to_window_with_margin(px(8.)).child(menu)).priority(3))
    }

    // -----------------------------------------------------------------------------------------
    // The work surface.

    pub(crate) fn render_work(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        // A tab drag that ended anywhere (dropped elsewhere, cancelled): no preview left.
        if !cx.has_active_drag() {
            self.editor_drop = None;
            self.editor_drop_glide = None;
        }
        let c = self.frame_now.center;
        let Some(f) = self.split_now else {
            let Some(tab) = self.active_tab().cloned() else { return div().size_full() };
            let body = self.render_group(tab, Pane::Left, true, c.w, window, cx);
            let preview = self.render_editor_drop(Pane::Left, window, cx);
            let group = div().id(("group", 0usize)).size_full().relative().child(body).children(preview);
            return div().size_full().child(self.editor_drop_target(group, Pane::Left, cx));
        };
        // Two editor groups (`split`) side by side, a hairline between them. While the split
        // opens or folds, the moving group keeps its full width inside, clipped, and rides the
        // seam; the other one narrows or widens.
        let ((lw, _), (full_l, full_r)) = f.widths(c.w);
        let moving = f.open < 1.;
        let bounds = self.pane_bounds.clone();
        let mut row = div().size_full().relative().flex().flex_row().on_children_prepainted(move |b, _, _| *bounds.borrow_mut() = b);
        for pane in [Pane::Left, Pane::Right] {
            let (tab, focused) = self.pane_view(pane);
            let full = if pane == Pane::Left { full_l } else { full_r };
            // Folding away, the group that stays is the only one already: its state (its strip)
            // is the left group's; the folding one shows what it showed.
            let ghost = self.split.is_none() && pane == f.leaving;
            let state = if self.split.is_none() && !ghost { Pane::Left } else { pane };
            let body = tab.map(|tab| self.render_group(tab, state, focused, full, window, cx));
            let clip = moving && pane == f.leaving;
            let preview = self.render_editor_drop(pane, window, cx);
            let group = div()
                .id(("group", pane.index()))
                .h_full()
                .relative()
                .overflow_hidden()
                .map(|d| if pane == Pane::Left { d.w(px(lw)).flex_none() } else { d.flex_1().min_w_0() })
                // A press anywhere in a group gives it the focus.
                .capture_any_mouse_down(cx.listener(move |w, _, window, cx| w.focus_pane(pane, window, cx)))
                .children(body.map(|b| {
                    if clip {
                        // Anchored on the seam side: it slides in and out with it.
                        div().absolute().top_0().bottom_0().w(px(full)).when(pane == Pane::Right, |d| d.left_0()).when(pane == Pane::Left, |d| d.right_0()).child(b).into_any_element()
                    } else {
                        b.into_any_element()
                    }
                }))
                .children(preview);
            row = row.child(self.editor_drop_target(group, pane, cx));
            if pane == Pane::Left {
                // Room for the seam; the title bar paints its hairline over it, from the window's
                // top edge (`render_seam_line`).
                row = row.child(div().w(px(crate::split::SEAM)).h_full().flex_none());
            }
        }
        // The seam's grabber, over both groups (not while they move).
        if !moving && self.split.is_some() {
            row = row.child(self.render_seam(lw, cx));
        }
        row
    }

    /// The messages list of group `pane` (each group has its own).
    fn messages_of(&self, pane: Pane) -> &crate::messages::Messages {
        if pane == Pane::Left { &self.messages } else { &self.messages_right }
    }

    /// An editor group: `tab`'s Run row, editor and results (or its activity monitor);
    /// `focused`: the group has the focus; `group_w`: its width.
    fn render_group(&mut self, tab: gpui::Entity<QueryTab>, pane: Pane, focused: bool, group_w: f32, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let t = cx.theme();
        let (editor, grid, has_run) = {
            let t = tab.read(cx);
            (t.editor.clone(), t.grid.clone(), t.has_run)
        };
        let region = (self.focus_visible && focused).then(|| self.focused_region(window, cx)).flatten();
        // The work column paints the translucent fill once (app.rs); the regions here are
        // transparent, separated by straight hairlines, and flush: no corner to keep clear.
        let monitor = tab.read(cx).monitor.clone();
        let view = tab_view(tab.read(cx), pane, focused, group_w < COMPACT_W);
        // The TX chip's age changes once a minute: one wake-up per minute while a transaction is
        // open, none otherwise.
        if focused {
            self.schedule_tx_tick(view.tx.since.filter(|_| view.tx.status.is_open()), cx);
        }
        // The result zoom: another side view opening ends it (that view shows).
        let side_now = crate::app::side_id(&self.side);
        if focused && tab.read(cx).zoomed && side_now.is_some() && side_now != tab.read(cx).zoom_side {
            self.toggle_result_zoom(window, cx);
        }
        // 0: in place; 1: the result covers the Run row, the editor and the side panel.
        let zoom = if has_run && monitor.is_none() { self.zoom_frame(&tab, window, cx) } else { 0. };
        let rest = 1. - zoom;
        let covered = zoom >= 0.999;
        let mut main = div().flex_1().min_w_0().h_full().flex().flex_col();
        if let Some(m) = monitor {
            // The activity monitor has the whole work surface (its own header and bar).
            main = main.child(m);
        } else if !has_run {
            main = main.child(self.render_toolbar(&view, &t, cx));
            main = main.child(
                div()
                    .flex_1()
                    .min_h(px(metrics::EDITOR_MIN))
                    .relative()
                    .overflow_hidden()
                    .child(editor)
                    .children((region == Some(Region::Editor)).then(|| focus_ring(&t))),
            );
        } else {
            let failed_view = self.error_view_needed(&tab, cx);
            let live = crate::lar::grid_override(&tab, cx);
            let shows_plan = !failed_view && tab.read(cx).shows_plan(cx);
            // A result without rows that printed: what it printed, in the grid's place.
            let log = match view.shown {
                Some(Key::Stmt(i)) => tab.read(cx).result().filter(|r| crate::messages::shows_log(r)).map(|r| (i, r.clone())),
                _ => None,
            };
            let strip = (view.results.len() > 1).then(|| self.render_result_strip(&view, &t, cx));
            let below = if strip.is_some() { metrics::STRIP_H } else { 0. };
            let editor_h = self.editor_h(window, below);
            let grid_area: AnyElement = if view.shown == Some(Key::Messages) {
                self.render_messages(&tab, &view, &t, cx).into_any_element()
            } else if failed_view {
                self.render_error_view(&tab, &t, cx).into_any_element()
            } else if let Some(g) = live {
                // LAR: live rows replace the result grid.
                g
            } else if shows_plan {
                // An EXPLAIN: the plan in the grid's place.
                tab.read(cx).plan.clone().into_any_element()
            } else if let Some((i, rs)) = log {
                self.render_log(i, &rs, pane, &t, cx).into_any_element()
            } else {
                grid.clone().into_any_element()
            };
            let bar = if shows_plan { self.render_plan_bar(&tab, &t, cx).into_any_element() } else { self.render_result_bar(&tab, &view, &t, cx).into_any_element() };
            // Zooming, the Run row, the editor and the divider fold up under the rising result
            // (each keeps its height inside and is clipped from below); zoomed, they are gone.
            if !covered {
                main = main
                    // The Run row fades first (it is the thinnest), the editor with the fold.
                    .child(div().h(px(metrics::TOOLBAR_H * rest)).flex_none().overflow_hidden().when(zoom > 0., |d| d.opacity(rest * rest)).child(self.render_toolbar(&view, &t, cx)))
                    .child(
                        div()
                            .h(px(editor_h * rest))
                            .flex_none()
                            .relative()
                            .overflow_hidden()
                            .when(zoom > 0., |d| d.opacity(rest))
                            .child(div().h(px(editor_h)).flex_none().child(editor))
                            .children((region == Some(Region::Editor) && zoom == 0.).then(|| focus_ring(&t))),
                    )
                    .child(
                        div()
                            .h(px(metrics::DIVIDER * rest))
                            .flex_none()
                            .relative()
                            .overflow_hidden()
                            .flex()
                            .items_center()
                            .justify_center()
                            .border_t_1()
                            .border_b_1()
                            .border_color(t.line)
                            .child(div().w(px(32.)).h(px(3.)).bg(t.line2))
                            .when(zoom == 0., |d| {
                                d.child(
                                    div()
                                        .id("splitter")
                                        .absolute()
                                        .left_0()
                                        .right_0()
                                        .top(px(-3.))
                                        .h(px(metrics::DIVIDER_HIT))
                                        .cursor_row_resize()
                                        .on_mouse_down(MouseButton::Left, cx.listener(|w, _, _, cx| {
                                            w.drag = Some(Drag::Split);
                                            cx.stop_propagation();
                                        })),
                                )
                            }),
                    );
            }
            let bounds = focused.then(|| self.result_bounds.clone());
            main = main.child(
                div().flex_1().min_h_0().flex().flex_col().on_children_prepainted(move |b, _, _| {
                    if let Some(bounds) = &bounds {
                        bounds.set(b.first().copied());
                    }
                })
                .child(
                    div()
                        .size_full()
                        .flex()
                        .flex_col()
                        .children(strip)
                        .child(bar)
                        .child(
                            div()
                                .flex_1()
                                .min_h(px(metrics::GRID_MIN))
                                .relative()
                                .overflow_hidden()
                                .child(grid_area)
                                // Grid-edit cards (read-only reason, key picker) float over the grid.
                                .child(tab.read(cx).edit.clone())
                                .children((region == Some(Region::Grid)).then(|| focus_ring(&t))),
                        ),
                ),
            );
        }
        // The side panel is a dock's (`dock`): the zoom folds it there.
        div().size_full().flex().flex_row().child(main)
    }

    // -----------------------------------------------------------------------------------------
    // Toolbar.

    fn render_toolbar(&self, v: &TabView, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let timer = v.elapsed.map(fmt_clock).unwrap_or_else(|| "00:00.0".into());
        let busy = v.running || v.cancelling;
        // Run: idle (accent), running (soft accent + timer), cancelling (muted).
        let run = {
            let b = if busy { ui::button(t, "tb-run", Btn::Ghost) } else { ui::button(t, "tb-run", Btn::Primary) };
            let run_key = crate::keymap::key("run_all");
            let (label, key): (SharedString, Option<Div>) = if v.cancelling {
                (format!("Running {timer}").into(), run_key.map(|k| ui::keycap_sm(t.key, t.tx3, k)))
            } else if v.running {
                (format!("Running {timer}").into(), run_key.map(|k| ui::keycap_sm(t.key, t.acc_t, k)))
            } else {
                ("Run".into(), run_key.map(|k| ui::keycap_sm(gpui::hsla(0., 0., 1., 0.18), t.on_acc, k)))
            };
            b.pl(px(9.))
                .pr(px(6.))
                .font_weight(FontWeight::SEMIBOLD)
                .when(v.running && !v.cancelling, |d| d.bg(t.acc_s).text_color(t.acc_t))
                .when(v.cancelling, |d| d.bg(t.hov).text_color(t.tx3))
                .child(icon(Icon::Run).size(px(14.)).text_color(if v.cancelling {
                    t.tx3
                } else if v.running {
                    t.acc_t
                } else {
                    t.on_acc
                }))
                .child(label)
                .children(key.filter(|_| !v.compact))
                .on_click(cx.listener(|w, _, window, cx| w.run_active(window, cx)))
        };
        // Cancel: only while something runs (it sits last in the row, so nothing moves).
        let cancel = busy.then(|| {
            let (glyph, label) = if v.cancelling { ("◌", "Cancel requested…") } else { ("■", "Cancel") };
            let b = if v.cancelling { ui::button(t, "tb-cancel", Btn::Ghost) } else { ui::button(t, "tb-cancel", Btn::Danger) };
            b.pl(px(9.))
                .pr(px(6.))
                .when(v.cancelling, |d| d.bg(t.warn_s).text_color(t.warn).border_1().border_color(t.warn))
                .child(div().text_size(px(11.)).child(glyph))
                .child(label)
                .when_some(crate::keymap::key("cancel"), |d, k| d.child(ui::keycap_sm(t.key, t.tx2, k)))
                .on_click(cx.listener(|w, _, _, cx| w.cancel_active(cx)))
        });
        let picker = self.render_conn_picker(v, t, cx);
        // An agent's tab: the agent's name (as its MCP client introduced itself) by the picker.
        let agent = v.agent.clone().map(|name| {
            let tip = format!("Made by {}", cut_chars(&name, 48));
            div()
                .id("tb-agent")
                // Its whole name (up to the cap): the picker beside it gives way first.
                .flex_none()
                .max_w(px(140.))
                .h(px(18.))
                .px(px(6.))
                .flex()
                .items_center()
                .bg(t.hov)
                .border_1()
                .border_color(t.line2)
                .text_size(px(10.5))
                .text_color(t.tx3)
                .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(name))
                .tooltip(ui::tooltip(tip, None, None))
        });
        // Statements autocommit. A transaction opened with a typed BEGIN holds locks until it
        // ends: while it is open, the TX chip with COMMIT and ROLLBACK.
        let tx_open = v.tx.status.is_open();
        let failed = v.tx.status == TxStatus::Failed;
        let tx_controls = tx_open.then(|| {
            let live = !busy && !v.ending;
            let small = |b: gpui::Stateful<Div>| b.h(px(24.)).px(px(8.)).gap(px(6.)).text_size(px(11.5));
            let commit = small(ui::button(t, "tb-commit", Btn::Secondary))
                .when(!live || failed, |d| d.text_color(t.tx4))
                .child("Commit")
                .when_some(crate::keymap::key("commit_tx"), |d, k| d.child(ui::key_hint(k)))
                .on_click(cx.listener(|w, _, _, cx| w.end_active_tx(true, cx)));
            let rollback = small(ui::button(t, "tb-rollback", if failed && live { Btn::Danger } else { Btn::Ghost }))
                .when(!live, |d| d.text_color(t.tx4))
                .child("Rollback")
                .when_some(crate::keymap::key("rollback_tx"), |d, k| d.child(ui::key_hint(k)))
                .on_click(cx.listener(|w, _, _, cx| w.end_active_tx(false, cx)));
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .child(tx_chip(t, &v.tx, false))
                .child(commit)
                .child(rollback)
        });
        let lar = v.conn.as_ref().filter(|c| c.info.is_lar).map(|c| self.render_lar_buttons(c, t, cx));
        let (parts, pane) = (self.toolbar_parts.clone(), v.pane);
        div()
            .on_children_prepainted(move |b, _, _| parts.borrow_mut()[pane.index()] = b)
            // Never over the next group: what does not fit is cut at the group's edge.
            .overflow_hidden()
            .h(px(metrics::TOOLBAR_H))
            .flex_none()
            .pl(px(10.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .border_b_1()
            .border_color(t.line)
            .text_color(t.tx2)
            .child(run)
            .child(picker)
            .children(agent)
            .when(tx_controls.is_some(), |d| d.child(ui::vsep(t, 18.).mx(px(4.))))
            .children(tx_controls)
            .children(cancel)
            .child(div().flex_1())
            .children(lar)
    }

    /// The connection and database the tab runs on, beside Run: the connection's color,
    /// `name · database` cut with an ellipsis inside the control, a chevron. Its popup lists
    /// the connected connections and their databases, the tab's checked.
    fn render_conn_picker(&self, v: &TabView, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let db = v.db.clone();
        // A restored tab names the connection it waits for (it connects on the first run).
        let want = v.want.clone();
        let wanted = want.as_ref().and_then(|(id, db)| self.store.read(cx).profile(id).map(|p| (p.clone(), db.clone().unwrap_or_else(|| p.database.clone()))));
        let (color, label): (Hsla, SharedString) = match (&v.conn, wanted) {
            (Some(c), _) if db.is_empty() => (t.profile_color(&c.profile), c.profile.name.clone().into()),
            (Some(c), _) => (t.profile_color(&c.profile), format!("{} · {db}", c.profile.name).into()),
            (None, Some((p, db))) if db.is_empty() => (t.profile_color(&p), p.name.into()),
            (None, Some((p, db))) => (t.profile_color(&p), format!("{} · {db}", p.name).into()),
            (None, None) => (t.tx4, "No connection".into()),
        };
        // A read-only database: a small lock after its name.
        let locked = match (&v.conn, &want) {
            (Some(c), _) => c.read_only_for(&db),
            (None, Some((id, d))) => self.store.read(cx).profile(id).is_some_and(|p| p.read_only_for(d.as_deref().unwrap_or(&p.database))),
            (None, None) => false,
        };
        let open = self.conn_menu && v.focused;
        let (hov, prs) = (t.hov, t.prs);
        let button = div()
            .id("tb-conn")
            .h(px(24.))
            .max_w(px(220.))
            .min_w_0()
            .flex_shrink_1()
            .overflow_hidden()
            .pl(px(8.))
            .pr(px(7.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .text_size(px(11.5))
            .text_color(if v.conn.is_some() { t.tx2 } else { t.tx3 })
            .when(open, |d| d.bg(prs))
            .when(!open, |d| d.hover(move |s| s.bg(hov)))
            .child(div().flex_none().size(px(7.)).bg(color))
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(label))
            .when(locked, |d| d.child(icon(Icon::Lock).flex_none().size(px(10.)).text_color(t.tx3)))
            .child(icon(Icon::ChevronDown).flex_none().size(px(9.)).text_color(t.tx3))
            .on_click(cx.listener(|w, _, _, cx| w.toggle_conn_menu(cx)));
        let menu = open.then(|| {
            let store = self.store.read(cx);
            let cur = v.conn.as_ref().map(|c| c.profile.id.clone());
            let mut rows: Vec<AnyElement> = Vec::new();
            for p in &store.profiles {
                let Some(conn) = store.connection(&p.id) else { continue };
                if !rows.is_empty() {
                    rows.push(ui::menu_sep(t).into_any_element());
                }
                rows.push(
                    div()
                        .h(px(24.))
                        .px(px(10.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(11.))
                        .text_color(t.tx3)
                        .child(div().flex_none().size(px(7.)).bg(t.profile_color(p)))
                        .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(p.name.clone()))
                        .into_any_element(),
                );
                let dbs = conn.schema.cached_databases().unwrap_or_else(|| vec![conn.database().to_string()]);
                for d in dbs {
                    let on = cur.as_deref() == Some(p.id.as_str()) && d == db;
                    let (id, pick) = (p.id.clone(), d.clone());
                    rows.push(
                        ui::menu_item(t, SharedString::from(format!("pick:{}:{d}", p.id)), d, on.then_some("✓"), true)
                            .pl(px(25.))
                            .on_click(cx.listener(move |w, _, window, cx| w.pick_target(&id, &pick, window, cx)))
                            .into_any_element(),
                    );
                }
            }
            // Under the button, its left edge on the button's.
            div().absolute().left_0().top(px(26.)).child(
                gpui::deferred(
                    gpui::anchored().snap_to_window_with_margin(px(8.)).child(
                        ui::popover(t)
                            .w(px(240.))
                            .on_mouse_down_out(cx.listener(|w, _, _, cx| {
                                if w.conn_menu {
                                    w.conn_menu = false;
                                    w.conn_menu_shut = Some(std::time::Instant::now());
                                    cx.notify();
                                }
                            }))
                            .p(px(0.))
                            .overflow_hidden()
                            .child(div().id("conn-menu").max_h(px(440.)).overflow_y_scroll().p(px(4.)).children(rows)),
                    ),
                )
                .priority(3),
            )
        });
        let bounds = v.focused.then(|| self.picker_bounds.clone());
        // It gives way first when the toolbar is narrow (cut with an ellipsis inside, down to
        // its chip, a few letters and the chevron), never wider than its room: the wrapper is a
        // flex row so the button shrinks with it.
        div()
            .relative()
            .min_w(px(PICKER_MIN))
            .flex_shrink_1()
            .flex()
            .flex_row()
            .on_children_prepainted(move |b, _, _| {
                if let Some(bounds) = &bounds {
                    bounds.set(b.first().copied());
                }
            })
            .child(button)
            .children(menu)
    }

    /// The LAR buttons, shown only on a LAR server: one violet button per registered panel
    /// (today Live, ⌥1), lit while its panel is open.
    fn render_lar_buttons(&self, conn: &Arc<Connection>, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let open_id = match &self.side {
            Some(Side::Server(id, ..)) => Some(*id),
            _ => None,
        };
        let panels = cx.try_global::<PanelRegistry>().map(|r| r.for_server(&conn.info)).unwrap_or_default();
        div().flex().flex_row().items_center().gap(px(4.)).children(panels.into_iter().map(|p| {
            let id = p.id();
            let known = LAR_BUTTONS.iter().find(|b| b.0 == id);
            let on = open_id == Some(id);
            let hov = t.lar_r;
            div()
                .id(id)
                .h(px(26.))
                .pl(px(8.))
                .pr(px(7.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .border_1()
                .text_size(px(11.5))
                .when(on, |d| d.bg(t.lar).border_color(t.lar).text_color(t.on_lar).font_weight(FontWeight::SEMIBOLD))
                .when(!on, |d| d.bg(t.lar_s).border_color(t.lar_r).text_color(t.lar_t).hover(move |s| s.bg(hov)))
                .children(known.map(|b| icon(b.2).size(px(14.)).text_color(if on { t.on_lar } else { t.lar_t })))
                .child(p.title())
                .children(known.and_then(|b| crate::keymap::key(&format!("lar_{}", b.0))).map(|k| div().text_size(px(11.)).opacity(0.7).child(k)))
                .on_click(cx.listener(move |w, _, window, cx| w.toggle_server_panel(id, window, cx)))
        }))
    }

    // -----------------------------------------------------------------------------------------
    // Result bar.

    fn result_state(&self, tab: &QueryTab, grid_sorting: bool, edit_pending: usize) -> ResultState {
        if tab.shown_key() == Some(Key::Messages) {
            return ResultState::Messages;
        }
        let Some(rs) = tab.result() else { return ResultState::Idle };
        let has_cols = rs.columns().is_some_and(|c| !c.is_empty());
        let status = rs.status();
        if grid_sorting {
            return ResultState::Sorting;
        }
        if edit_pending > 0 {
            return ResultState::Edit;
        }
        if tab.is_cancelling() && tab.shows_current() {
            return ResultState::Cancelling;
        }
        match rs.phase() {
            Phase::Running => ResultState::Running,
            Phase::Paused => ResultState::Paused,
            Phase::Done if status.partial => ResultState::Partial,
            Phase::Done if !has_cols => ResultState::Affected,
            Phase::Done if status.text_mode => ResultState::Text,
            Phase::Done => ResultState::Done,
            Phase::Cancelled => ResultState::Cancelled,
            Phase::Failed if rs.rows() > 0 => ResultState::ErrPartial,
            Phase::Failed => ResultState::Error,
        }
    }

    /// The result bar's `+`: a row at the end of the active result, its first field open.
    pub fn insert_row(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab() {
            let grid = tab.read(cx).grid.clone();
            grid.update(cx, |g, cx| g.insert_row(cx));
        }
    }

    fn render_result_bar(&self, tab: &gpui::Entity<QueryTab>, v: &TabView, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let grid = tab.read(cx).grid.clone();
        let sorting = grid.read(cx).is_sorting();
        let lar_bar = crate::lar::result_bar(tab, cx);
        let edit = tab.read(cx).edit.read(cx);
        let edit_pending = edit.pending_count();
        let edit_note = edit.target_label().map(|l| format!("{l} · {}", edit.summary())).unwrap_or_else(|| edit.summary());
        let state = lar_bar.as_ref().map(|b| b.state).unwrap_or_else(|| self.result_state(tab.read(cx), sorting, edit_pending));
        let rs = tab.read(cx).result().cloned();
        // Rows to add to (`+`): a result with columns, not a live grid's.
        let can_insert = lar_bar.is_none() && rs.as_ref().is_some_and(|r| r.columns().is_some_and(|c| !c.is_empty()));
        let summary = grid.update(cx, |g, _| g.selection_summary());
        let spec = state.spec();
        let (fg, bg) = role_colors(t, spec.role);
        let status = rs.as_ref().map(|r| r.status());
        let rows = rs.as_ref().map(|r| r.rows()).unwrap_or(0);
        let mut label: SharedString = match state {
            ResultState::Affected => rs.as_ref().map(|r| statement_verb(&r.sql)).unwrap_or_else(|| "DONE".into()).into(),
            ResultState::Edit => format!("{edit_pending} PENDING").into(),
            _ => spec.label.into(),
        };
        let mut rows_text = match state {
            ResultState::Idle | ResultState::Error => None,
            ResultState::Paused => Some(format!("{}+", ui_count(rows))),
            // What a batch that only printed left in @@ROWCOUNT means nothing.
            ResultState::Affected if rs.as_ref().is_some_and(|r| crate::messages::shows_log(r) && batch::tag(&r.sql, Some(0)) == statement_verb(&r.sql)) => None,
            ResultState::Affected => status.as_ref().and_then(|s| s.affected).map(|a| ui_count(a as usize)),
            ResultState::Running if rows == 0 => None,
            _ => Some(ui_count(rows)),
        };
        let mut metrics_: Vec<(&str, String)> = Vec::new();
        if state == ResultState::Messages {
            let folded = v.results.iter().find(|r| r.key == Key::Messages).map_or(0, |r| r.folded);
            rows_text = (folded > 0).then(|| folded.to_string());
            let total: std::time::Duration = v.batch.iter().filter_map(|b| b.elapsed).sum();
            metrics_.push(("total", fmt_dur(total)));
        }
        if let Some(s) = &status {
            // A partial result: what the server had in all.
            if state == ResultState::Partial
                && let Some(n) = s.affected
            {
                metrics_.push(("of", ui_count(n as usize)));
            }
            match state {
                ResultState::Running | ResultState::Cancelling => metrics_.push(("elapsed", v.elapsed.map(fmt_clock).unwrap_or_default())),
                _ => {
                    if let Some(e) = s.elapsed.or(v.elapsed) {
                        metrics_.push(("total", fmt_dur(e)));
                    }
                }
            }
        }
        let failed_ix = tab.read(cx).failed_index();
        let n = v.batch.len();
        let (mut note, mut note_err): (String, bool) = match state {
            ResultState::Done | ResultState::Text => (summary.map(|s| s.to_string()).unwrap_or_default(), false),
            ResultState::Edit => (edit_note, false),
            ResultState::Error => {
                let code = status.as_ref().and_then(|s| s.error.as_ref()).and_then(|e| e.code.clone()).unwrap_or_else(|| "error".into());
                let tail = match failed_ix {
                    Some(i) if n > 1 => format!(" · statement {} of {n}", i + 1),
                    _ => String::new(),
                };
                (format!("{code}{tail}"), true)
            }
            ResultState::ErrPartial => {
                (status.as_ref().and_then(|s| s.error.as_ref()).and_then(|e| e.code.clone()).unwrap_or_else(|| "error".into()), true)
            }
            _ => (String::new(), false),
        };
        // `command`: the keymap id whose current key the button shows.
        let action = |id: &'static str, label: SharedString, command: &'static str| {
            ui::button(t, id, Btn::Secondary)
                .h(px(24.))
                .px(px(8.))
                .gap(px(6.))
                .text_size(px(11.5))
                .child(label)
                .when_some(crate::keymap::key(command), |d, k| d.child(ui::key_hint(k)))
        };
        let mut actions: Vec<AnyElement> = Vec::new();
        match state {
            ResultState::Paused => {
                actions.push(action("rb-more", format!("Fetch next {}", crate::grid::fetch_step()).into(), "fetch_more").on_click(cx.listener(|w, _, _, cx| {
                    if let Some(t) = w.active_tab().cloned() {
                        t.update(cx, |t, cx| t.fetch_more(cx));
                    }
                })).into_any_element());
                actions.push(action("rb-all", fetch_all_label(rs.as_deref()), "fetch_all").on_click(cx.listener(|w, _, _, cx| {
                    if let Some(t) = w.active_tab().cloned() {
                        t.update(cx, |t, cx| t.fetch_all(cx));
                    }
                })).into_any_element());
            }
            ResultState::Cancelled => {
                actions.push(action("rb-rerun", "Re-run".into(), "run_all").on_click(cx.listener(|w, _, window, cx| w.run_active(window, cx))).into_any_element());
            }
            ResultState::Error | ResultState::ErrPartial => {
                let label = if state == ResultState::Error { "Show in editor" } else { "Show error" };
                actions.push(action("rb-show", label.into(), "jump_to_error").on_click(cx.listener(|w, _, window, cx| {
                    if let Some(tab) = w.active_tab().cloned() {
                        // The editor must be in sight.
                        if tab.read(cx).zoomed {
                            w.toggle_result_zoom(window, cx);
                        }
                        let e = tab.read(cx).editor.clone();
                        e.update(cx, |e, cx| e.jump_to_error(cx));
                        window.focus(&e.focus_handle(cx), cx);
                    }
                })).into_any_element());
            }
            ResultState::Sorting => {
                actions.push(action("rb-stop", "Stop".into(), "cancel").on_click(cx.listener(|w, _, _, cx| w.cancel_active(cx))).into_any_element());
            }
            ResultState::Edit => {
                actions.push(
                    ui::button(t, "rb-discard", Btn::Ghost)
                        .h(px(24.))
                        .px(px(8.))
                        .gap(px(6.))
                        .text_size(px(11.5))
                        .child("Discard")
                        .when_some(crate::keymap::key("discard_changes"), |d, k| d.child(ui::key_hint(k)))
                        .on_click(cx.listener(|_, _, window, cx| window.dispatch_action(Box::new(crate::app::DiscardChanges), cx)))
                        .into_any_element(),
                );
                actions.push(
                    ui::button(t, "rb-review", Btn::Primary)
                        .h(px(24.))
                        .px(px(8.))
                        .gap(px(6.))
                        .text_size(px(11.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Review & commit")
                        .when_some(crate::keymap::key("review_changes"), |d, k| d.child(ui::key_hint(k)))
                        .on_click(cx.listener(|_, _, window, cx| window.dispatch_action(Box::new(crate::app::ReviewChanges), cx)))
                        .into_any_element(),
                );
            }
            _ => {}
        }
        let mut rows_label: Option<&'static str> = None;
        if let Some(b) = lar_bar {
            label = b.label;
            rows_text = b.rows;
            rows_label = b.rows_label;
            metrics_ = b.metrics;
            note = b.note;
            note_err = b.note_err;
            actions = b
                .actions
                .into_iter()
                .map(|(id, label, key, act)| {
                    action(id, label.into(), key)
                        .on_click(move |_, window, cx| window.dispatch_action(act.boxed_clone(), cx))
                        .into_any_element()
                })
                .collect();
        }
        // A table's page: `1–1,000 of ~1.2M` and the page arrows instead of the row count.
        let pager = {
            let tr = tab.read(cx);
            let finished = rs.as_ref().is_some_and(|r| r.phase().is_finished());
            tr.table.as_ref().filter(|_| tr.shows_table() && !matches!(state, ResultState::Error | ResultState::Idle)).map(|d| crate::table_view::pager(d, rows, finished))
        };
        let pager_el = pager.as_ref().map(|p| {
            rows_text = None;
            let exact = p.total.is_some_and(|(_, e)| e);
            let counting = p.counting;
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap(px(5.))
                .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(t.tx).child(p.range()))
                .children(p.of().map(|o| {
                    let hov = t.tx;
                    div()
                        .id("rb-total")
                        .text_color(t.tx3)
                        .when(!exact || counting, |d| d.cursor_pointer().hover(move |s| s.text_color(hov)))
                        .child(o)
                        // An estimate: a click counts; while counting, a click stops it.
                        .on_click(cx.listener(move |w, _, _, cx| {
                            if let Some(tab) = w.active_tab().cloned() {
                                tab.update(cx, |t, cx| if counting { t.stop_count(cx) } else if !exact { t.start_count(cx) });
                            }
                        }))
                }))
                .when(counting, |d| d.child(div().text_color(t.tx4).child("counting")))
        });
        if let Some(p) = &pager {
            let arrow = |id: &'static str, ic: Icon, on: bool| {
                ui::button(t, id, Btn::Ghost)
                    .h(px(24.))
                    .w(px(24.))
                    .px(px(0.))
                    .justify_center()
                    .child(icon(ic).size(px(13.)).text_color(if on { t.tx2 } else { t.tx4 }))
            };
            actions.insert(
                0,
                div()
                    .flex()
                    .flex_row()
                    .gap(px(2.))
                    .child(arrow("rb-prev", Icon::ChevronLeft, p.prev).on_click(cx.listener(|w, _, _, cx| {
                        if let Some(t) = w.active_tab().cloned() {
                            t.update(cx, |t, cx| t.table_step(false, cx));
                        }
                    })))
                    .child(arrow("rb-next", Icon::ChevronRight, p.next).on_click(cx.listener(|w, _, _, cx| {
                        if let Some(t) = w.active_tab().cloned() {
                            t.update(cx, |t, cx| t.table_step(true, cx));
                        }
                    })))
                    .into_any_element(),
            );
        }
        let has_rows = rows > 0;
        let copy_open = self.copy_menu && v.focused;
        let printed = copy_open && {
            let tr = tab.read(cx);
            if tr.shown_key() == Some(Key::Messages) { tr.batch_items().iter().any(|b| b.rs.as_ref().is_some_and(|r| r.has_messages())) } else { rs.as_ref().is_some_and(|r| r.has_messages()) }
        };
        // A copy a moment ago: "Copied", fading in and out (`motion::copied`).
        let copied = grid.read(cx).copied_at.and_then(|at| crate::motion::copied(at, std::time::Instant::now(), cx.reduce_motion()));
        // Under the Copy button, its right edge on the button's (a zero-width point at the
        // button's bottom-right corner anchors it; it flips above when there is no room).
        let copy_menu = copy_open.then(|| {
            div().absolute().right_0().top(px(26.)).child(gpui::deferred(
                gpui::anchored().anchor(gpui::Anchor::TopRight).snap_to_window_with_margin(px(8.)).child(
                    ui::popover(t)
                        .w(px(236.))
                        .on_mouse_down_out(cx.listener(|w, _, _, cx| {
                            w.copy_menu = false;
                            cx.notify();
                        }))
                        .child(ui::menu_item(t, "cp-tsv", "Copy as TSV", crate::keymap::key("copy_tsv").as_deref(), has_rows).on_click(cx.listener(|w, _, _, cx| w.copy_selection(CopyFormat::Tsv, false, cx))))
                        // ⇧⌘C ("Copy with headers") is this one: TSV with its header row.
                        .child(ui::menu_item(t, "cp-tsv-hdr", "Copy as TSV with Headers", crate::keymap::key("copy_with_headers").as_deref(), has_rows).on_click(cx.listener(|w, _, _, cx| w.copy_selection(CopyFormat::Tsv, true, cx))))
                        .child(ui::menu_item(t, "cp-csv", "Copy as CSV", crate::keymap::key("copy_csv").as_deref(), has_rows).on_click(cx.listener(|w, _, _, cx| w.copy_selection(CopyFormat::Csv, false, cx))))
                        .child(ui::menu_item(t, "cp-csv-hdr", "Copy as CSV with Headers", None, has_rows).on_click(cx.listener(|w, _, _, cx| w.copy_selection(CopyFormat::Csv, true, cx))))
                        .when(printed, |d| d.child(ui::menu_sep(t)).child(ui::menu_item(t, "cp-msg", "Copy Messages", None, true).on_click(cx.listener(|w, _, _, cx| w.copy_messages(cx))))),
                ),
            )
            .priority(3))
        });
        let ghost = |id: &'static str| ui::button(t, id, Btn::Ghost).h(px(24.)).px(px(7.)).gap(px(5.)).text_size(px(11.5));
        div()
            .h(px(metrics::RESULT_BAR_H))
            .flex_none()
            .pl(px(10.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .border_b_1()
            .border_color(t.line)
            .text_color(t.tx2)
            .whitespace_nowrap()
            // The left group gives way first when the bar is narrow (side panel open).
            .child(div().flex_1().min_w_0().overflow_hidden().flex().flex_row().items_center().gap(px(10.))
            .child(ui::pill(spec.glyph, label, fg, bg).when(state == ResultState::Cancelling, |d| d.border_1().border_color(t.warn)))
            .children(pager_el)
            .children(rows_text.map(|r| {
                // "1 row", "1 row affected".
                let rl = rows_label.unwrap_or(spec.rows_label);
                let what = if r == "1" { rl.replacen("rows", "row", 1) } else { rl.to_string() };
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(5.))
                    .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(t.tx).child(r))
                    .child(div().text_color(t.tx3).child(what))
            }))
            .children(metrics_.into_iter().map(|(k, v)| {
                div()
                    .flex()
                    .flex_row()
                    .gap(px(5.))
                    .child(div().text_color(t.tx4).child("·"))
                    .child(div().text_color(t.tx3).child(k))
                    .child(div().text_color(t.tx).child(v))
            }))
            .child(div().min_w_0().overflow_hidden().text_ellipsis().text_color(if note_err { t.err } else { t.tx3 }).child(note)))
            .children(actions)
            .child(ui::vsep(t, 16.))
            .when(can_insert, |d| {
                d.child(
                    ui::button(t, "rb-insert", Btn::Ghost)
                        .h(px(24.))
                        .w(px(26.))
                        .px(px(0.))
                        .justify_center()
                        .child(icon(Icon::Plus).size(px(13.)).text_color(t.tx2))
                        .tooltip(ui::tooltip("Add row", None, crate::keymap::key("insert_row").as_deref()))
                        .on_click(cx.listener(|w, _, _, cx| w.insert_row(cx))),
                )
            })
            .child(
                div()
                    .relative()
                    .child(
                        ghost("rb-copy")
                            // "Copied" takes the button's place for a moment: the button keeps
                            // its room (invisible), so nothing moves.
                            .when(copied.is_some(), |d| d.invisible())
                            .child(icon(Icon::Copy).size(px(13.)).text_color(t.tx2))
                            .child("Copy")
                            .child(icon(Icon::ChevronDown).size(px(9.)).text_color(t.tx2))
                            .on_click(cx.listener(|w, _, _, cx| {
                                w.copy_menu = !w.copy_menu;
                                cx.notify();
                            })),
                    )
                    .children(copied.map(|a| {
                        div()
                            .id("rb-copied")
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
                            .on_click(cx.listener(|w, _, _, cx| {
                                w.copy_menu = !w.copy_menu;
                                cx.notify();
                            }))
                    }))
                    .children(copy_menu),
            )
            .child(
                ghost("rb-export")
                    .when(!has_rows, |d| d.text_color(t.tx4))
                    .child(icon(Icon::Export).size(px(13.)).text_color(if has_rows { t.tx2 } else { t.tx4 }))
                    .child("Export")
                    .on_click(cx.listener(|w, _, window, cx| w.open_export(window, cx))),
            )
            .child({
                let zoomed = tab.read(cx).zoomed;
                let key = if zoomed { Some("esc".to_string()) } else { crate::keymap::key("zoom_result") };
                ui::button(t, "rb-zoom", Btn::Ghost)
                    .h(px(24.))
                    .w(px(26.))
                    .px(px(0.))
                    .justify_center()
                    .when(zoomed, |d| d.bg(t.acc_s).text_color(t.acc_t).border_1().border_color(t.acc))
                    .child(icon(if zoomed { Icon::Collapse } else { Icon::Expand }).size(px(13.)).text_color(if zoomed { t.acc_t } else { t.tx2 }))
                    .tooltip(ui::tooltip(if zoomed { "Exit zoom" } else { "Zoom result" }, None, key.as_deref()))
                    .on_click(cx.listener(|w, _, window, cx| w.toggle_result_zoom(window, cx)))
            })
    }

    // -----------------------------------------------------------------------------------------
    // Result tabs: one per statement of a script that returned rows, a count, an error or a
    // cancel, and the folded commands; over the result bar, only when there are several.

    fn render_result_strip(&self, v: &TabView, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let hov = t.hov;
        let bounds = v.focused.then(|| self.result_tab_bounds.clone());
        div()
            .on_children_prepainted(move |b, _, _| {
                if let Some(bounds) = &bounds {
                    *bounds.borrow_mut() = b;
                }
            })
            .id("result-strip")
            .h(px(metrics::STRIP_H))
            .flex_none()
            .px(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.))
            .border_b_1()
            .border_color(t.line)
            .overflow_x_scroll()
            .whitespace_nowrap()
            .text_size(px(11.5))
            .children(v.results.iter().enumerate().map(|(i, r)| {
                let active = v.shown == Some(r.key);
                let key = r.key;
                let messages = r.key == Key::Messages;
                let (g, gc) = if messages && r.state != batch::State::Running { ("≡", t.tx3) } else { state_glyph(t, r.state) };
                let label_c = if r.kind == batch::Kind::Error {
                    t.err
                } else if active {
                    t.tx
                } else {
                    t.tx2
                };
                let group: SharedString = format!("result-tab-{i}").into();
                let (tx3, tx) = (t.tx3, t.tx);
                // The editor tabs' close: a × that shows on hover.
                let close = div()
                    .id(("close-result", i))
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
                div()
                    .id(("result-tab", i))
                    .group(group)
                    .flex_none()
                    .h(px(22.))
                    .pl(px(8.))
                    .pr(px(5.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .when(active, |d| d.bg(t.prs))
                    .when(!active, |d| d.hover(move |s| s.bg(hov)))
                    .child(div().w(px(10.)).text_center().text_size(px(if messages { 12. } else { 9.5 })).text_color(gc).child(g))
                    .children(r.number.map(|n| div().text_color(t.tx3).child(format!("{n} ·"))))
                    .child(div().text_color(label_c).child(r.label.clone()))
                    .when(r.folded > 0, |d| d.child(div().text_color(t.tx3).child(r.folded.to_string())))
                    .child(close)
                    .on_click(cx.listener(move |w, _, _, cx| w.select_result(key, cx)))
                    .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
                    .on_mouse_up(MouseButton::Middle, cx.listener(move |w, _, _, cx| {
                        cx.stop_propagation();
                        w.close_result(key, cx);
                    }))
            }))
    }

    /// The messages tab: every statement of the run, its outcome and time, and what it printed;
    /// a click on one that has a tab of its own shows it.
    fn render_messages(&self, tab: &gpui::Entity<QueryTab>, v: &TabView, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let running = v.batch.iter().any(|b| b.state == ItemState::Running);
        self.messages_of(v.pane).sync(Of::Tab(tab.entity_id()), messages::tab_rows(&v.batch), running);
        self.render_message_list("messages", v.pane, t, cx)
    }

    /// What a result without rows printed (statement `i`'s), in the grid's place.
    fn render_log(&self, i: usize, rs: &Arc<dataman_core::ResultSet>, pane: Pane, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        self.messages_of(pane).sync(Of::log(rs), messages::log_rows(i, rs), rs.phase() == Phase::Running);
        self.render_message_list("log", pane, t, cx)
    }

    fn render_message_list(&self, id: &'static str, pane: Pane, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = t.clone();
        div()
            .id(id)
            .size_full()
            .px(px(20.))
            .py(px(14.))
            .text_size(px(12.))
            .child(gpui::list(self.messages_of(pane).list.clone(), cx.processor(move |w, ix: usize, _, cx| w.render_message_row(pane, ix, &theme, cx))).size_full())
    }

    /// A row of the messages list: a statement of the messages tab, or a line it printed.
    fn render_message_row(&mut self, pane: Pane, ix: usize, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (Some(row), Some(tab)) = (self.messages_of(pane).row(ix), self.pane_view(pane).0) else { return div().into_any_element() };
        let tr = tab.read(cx);
        let batch = tr.batch_items();
        // Under a statement, a line lines up with its text.
        let indent = if matches!(self.messages_of(pane).of(), Some(Of::Tab(_))) { 64. } else { 0. };
        let line = |text: String, c: Hsla| div().pl(px(indent)).pr(px(8.)).py(px(2.)).text_color(c).child(text).into_any_element();
        let i = match row {
            MessageRow::Stmt(i) => i,
            MessageRow::Line(i, j) => {
                let text = batch.get(i).and_then(|b| b.rs.as_ref()).and_then(|rs| rs.messages().0.get(j).cloned()).unwrap_or_default();
                let c = if indent > 0. { t.tx2 } else { t.tx };
                return line(text.replace("\r\n", "\n").replace('\r', "\n"), c);
            }
            MessageRow::More(_, n) => return line(format!("{} more", ui_count(n)), t.tx3),
        };
        let Some(b) = batch.get(i) else { return div().into_any_element() };
        let stopped = batch.iter().any(|b| matches!(b.state, ItemState::Failed | ItemState::Cancelled)) || !batch.iter().any(|b| b.state == ItemState::Running);
        let o = b.rs.as_deref().map(batch::Outcome::of);
        let (g, gc) = match o {
            Some(o) if b.state != ItemState::Pending => state_glyph(t, o.state),
            _ => ("·", t.tx4),
        };
        let (what, what_c) = match (&b.rs, b.state) {
            (_, ItemState::Pending) => (if stopped { "not run".to_string() } else { String::new() }, t.tx4),
            (Some(rs), _) => outcome_text(rs, t),
            (None, _) => (String::new(), t.tx3),
        };
        let sql: String = b.label.as_deref().unwrap_or(&b.sql).split_whitespace().filter(|w| !w.starts_with("--")).collect::<Vec<_>>().join(" ");
        let has_tab = tr.result_tabs().iter().any(|r| r.key == Key::Stmt(i));
        let pending = b.state == ItemState::Pending;
        let hov = t.hov;
        div()
            .pb(px(2.))
            .child(
                div()
                    .id(("message", i))
                    .h(px(28.))
                    .px(px(8.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .when(has_tab, |d| d.cursor_pointer().hover(move |s| s.bg(hov)))
                    .child(div().w(px(14.)).text_center().text_size(px(10.)).text_color(gc).child(g))
                    .child(div().w(px(22.)).text_color(t.tx3).child((i + 1).to_string()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_color(if pending { t.tx4 } else { t.tx })
                            .child(sql),
                    )
                    .child(div().flex_none().whitespace_nowrap().text_color(what_c).child(what))
                    .child(
                        div()
                            .w(px(64.))
                            .flex_none()
                            .text_right()
                            .text_size(px(11.))
                            .text_color(t.tx3)
                            .child(b.elapsed.map(fmt_dur).unwrap_or_default()),
                    )
                    .when(has_tab, |d| d.on_click(cx.listener(move |w, _, _, cx| w.select_result(Key::Stmt(i), cx)))),
            )
            .into_any_element()
    }

    /// Copies what the results on screen printed: the run's (the messages tab), else the
    /// shown result's.
    pub(crate) fn copy_messages(&mut self, cx: &mut Context<Self>) {
        self.copy_menu = false;
        if let Some(tab) = self.active_tab().cloned() {
            let text = shown_messages(tab.read(cx));
            if !text.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                let grid = tab.read(cx).grid.clone();
                grid.update(cx, |g, cx| g.mark_copied(cx));
            }
        }
        cx.notify();
    }

    pub(crate) fn copy_selection(&mut self, fmt: CopyFormat, header: bool, cx: &mut Context<Self>) {
        self.copy_menu = false;
        if let Some(tab) = self.active_tab().cloned() {
            let grid = tab.read(cx).grid.clone();
            grid.update(cx, |g, cx| g.copy(fmt, header, cx));
        }
        cx.notify();
    }

    // -----------------------------------------------------------------------------------------
    // Status bar.

    pub(crate) fn render_status(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = cx.theme();
        let tab = self.active_tab().map(|t| t.read(cx));
        let conn = tab.and_then(|t| t.connection().cloned());
        let notice = tab.and_then(|t| t.notice.clone());
        let badge: Option<AnyElement> = conn.as_ref().map(|c| {
            if c.info.is_lar {
                ui::lar_badge(&t, "◆ LAR").into_any_element()
            } else {
                div()
                    .h(px(17.))
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .bg(t.hov)
                    .border_1()
                    .border_color(t.edge)
                    .text_color(t.tx2)
                    .child(match c.profile.engine {
                        dataman_core::Engine::Postgres => "PG",
                        dataman_core::Engine::MySql => "MY",
                        dataman_core::Engine::SqlServer => "MS",
                    })
                    .into_any_element()
            }
        });
        let version: SharedString = match &conn {
            Some(c) if c.info.is_lar => format!("{} (LAR)", c.info.short_version()).into(),
            Some(c) => c.info.short_version().into(),
            None => "not connected".into(),
        };
        let lar_status = tab.and_then(|t| crate::lar::status_text(t, cx));
        // The connection's environment and read-only flag, when set.
        let env = conn.as_ref().map(|c| c.profile.env).filter(|e| !e.is_none()).map(|e| {
            div().text_color(if e.is_production() { t.err } else { t.tx2 }).child(e.label())
        });
        let db = tab.map(|t| t.database().to_string()).unwrap_or_default();
        let read_only = conn.as_ref().filter(|c| c.read_only_for(&db)).map(|_| div().text_color(t.tx2).child("read-only"));
        // A strip of its own across the whole window, on the glass under both columns.
        div()
            .h(px(metrics::STATUS_H))
            .flex_none()
            .border_t_1()
            .border_color(t.line)
            .pl(px(14.))
            .pr(px(12.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.))
            .text_size(px(11.))
            .text_color(t.tx3)
            .whitespace_nowrap()
            .overflow_hidden()
            .child(div().flex().flex_row().items_center().gap(px(8.)).children(badge).child(version))
            .children(env)
            .children(read_only)
            .children(lar_status.map(|(s, c)| div().text_color(c).child(s)))
            .child(div().flex_1())
            .children(self.flash_notice.as_ref().map(|(m, _)| div().text_color(t.tx2).child(m.clone())))
            .children(self.export.as_ref().filter(|e| !e.read(cx).visible).and_then(|e| e.read(cx).status_line()).map(|l| {
                div()
                    .id("export-status")
                    .text_color(t.acc_t)
                    .child(l)
                    .on_click(cx.listener(|w, _, window, cx| w.open_export(window, cx)))
            }))
            .children(self.data_import.as_ref().filter(|v| !v.read(cx).visible).and_then(|v| v.read(cx).status_line()).map(|l| {
                div()
                    .id("import-status")
                    .text_color(t.acc_t)
                    .child(l)
                    .on_click(cx.listener(|w, _, window, cx| w.import_data(None, None, window, cx)))
            }))
            .children(notice.map(|n| div().text_color(t.warn).child(n)))
    }

    // -----------------------------------------------------------------------------------------
    // Error view (EditorError): the server's message in full, the statement around the
    // error, and the batch that stopped.

    fn error_view_needed(&self, tab: &gpui::Entity<QueryTab>, cx: &gpui::App) -> bool {
        tab.read(cx)
            .result()
            .is_some_and(|r| r.phase() == Phase::Failed && r.rows() == 0 && r.columns().is_none_or(|c| c.is_empty()))
    }

    fn render_error_view(&self, tab: &gpui::Entity<QueryTab>, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let tabr = tab.read(cx);
        let rs = tabr.result().cloned();
        let status = rs.as_ref().map(|r| r.status()).unwrap_or_default();
        let err = status.error.clone().unwrap_or_default();
        let batch = tabr.batch_items().to_vec();
        let failed = tabr.failed_index();
        let editor = tabr.editor.read(cx);
        let text = editor.text().to_string();
        let diag_at = editor.diagnostic().and_then(|d| d.at);
        let token = editor.error_range();
        let code = err.code.clone().unwrap_or_default();
        // Line and column of the error in the editor (1-based).
        // The offsets come from before any edit since: snapped to a character of the text now.
        let fc = crate::tab::floor_char;
        let place = diag_at.map(|at| {
            let at = fc(&text, at);
            let line = text[..at].matches('\n').count() + 1;
            let col = text[..at].rsplit('\n').next().map(|s| s.chars().count()).unwrap_or(0) + 1;
            (line, col, at)
        });
        let mut where_ = Vec::new();
        if let (Some(i), n) = (failed, batch.len())
            && n > 1
        {
            where_.push(format!("statement {} of {n}", i + 1));
        }
        if let Some((l, c, _)) = place {
            where_.push(format!("line {l}, col {c}"));
        }
        // The message, with its quoted names in the error color.
        let message = err.message.split('"').enumerate().map(|(i, part)| {
            if i % 2 == 1 { div().text_color(t.err).child(format!("\"{part}\"")) } else { div().child(part.to_string()) }
        });
        // What the server added, when it did.
        let grid_rows: Vec<(&str, String)> =
            [("DETAIL", err.detail.clone()), ("HINT", err.hint.clone())].into_iter().filter_map(|(k, v)| v.map(|v| (k, v))).collect();
        // The statement around the error: the line before, the line, and a caret line.
        let excerpt = place.map(|(line, _, at)| {
            let lines: Vec<&str> = text.split('\n').collect();
            let mut out: Vec<AnyElement> = Vec::new();
            let num = |n: usize, c: Hsla| div().w(px(40.)).flex_none().text_right().text_color(c).child(n.to_string());
            if line >= 2 {
                out.push(div().flex().flex_row().gap(px(16.)).child(num(line - 1, t.tx4)).child(div().text_color(t.tx2).child(lines[line - 2].to_string())).into_any_element());
            }
            let ltxt = lines.get(line - 1).copied().unwrap_or("");
            let lstart = at - text[..at].rsplit('\n').next().map(|s| s.len()).unwrap_or(0);
            let (ts, te) = token.clone().map(|r| (r.start.saturating_sub(lstart), r.end.saturating_sub(lstart))).unwrap_or((at - lstart, at - lstart));
            let ts = fc(ltxt, ts);
            let te = fc(ltxt, te).max(ts);
            out.push(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(16.))
                    .child(num(line, t.err))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .child(ltxt[..ts].to_string())
                            .child(div().text_color(t.err).border_b_1().border_color(t.err).child(ltxt[ts..te].to_string()))
                            .child(ltxt[te..].to_string()),
                    )
                    .into_any_element(),
            );
            let pad = ltxt[..ts].chars().count();
            let carets = ltxt[ts..te].chars().count().max(1);
            out.push(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(16.))
                    .child(div().w(px(40.)).flex_none())
                    .child(div().text_color(t.err).child(format!("{}{}", " ".repeat(pad), "^".repeat(carets))))
                    .into_any_element(),
            );
            // A long line stays in the card (the batch column is beside it).
            div()
                .py(px(10.))
                .overflow_hidden()
                .bg(t.card)
                .border_1()
                .border_color(t.line)
                .text_size(px(12.5))
                .line_height(px(20.))
                .flex()
                .flex_col()
                .children(out)
        });
        let suggestion = err.hint.as_deref().and_then(|h| h.split('"').nth(1)).map(str::to_string).filter(|_| token.is_some());
        let err_copy = err.to_string();
        let btn = |id: &'static str, label: &'static str, key: Option<String>| {
            ui::button(t, id, Btn::Secondary).h(px(28.)).px(px(10.)).gap(px(8.)).child(label).children(key.map(|k| div().text_size(px(10.5)).text_color(t.tx3).child(k)))
        };
        let left = div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .px(px(28.))
            .py(px(22.))
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .h(px(20.))
                            .px(px(7.))
                            .flex()
                            .items_center()
                            .bg(t.err)
                            .text_color(t.on_err)
                            .text_size(px(10.5))
                            .font_weight(FontWeight::BOLD)
                            .child("✕ ERROR"),
                    )
                    .child(div().text_color(t.err).child(code.clone()))
                    .child(div().text_color(t.tx3).child(condition_name(&code)))
                    .child(div().flex_1())
                    .child(div().text_color(t.tx3).child(where_.join(" · "))),
            )
            .child(div().flex().flex_row().flex_wrap().text_size(px(18.)).line_height(px(24.)).text_color(t.tx).children(message))
            .when(!grid_rows.is_empty(), |d| {
                d.child(div().flex().flex_col().gap(px(7.)).line_height(px(18.)).children(grid_rows.into_iter().map(|(k, v)| {
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(14.))
                        .child(div().w(px(96.)).flex_none().text_color(t.tx3).child(k))
                        .child(div().flex_1().min_w_0().child(v))
                })))
            })
            .children(excerpt)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.))
                    .when(place.is_some(), |d| {
                        d.child(btn("ev-jump", "Jump to error", crate::keymap::key("jump_to_error")).on_click(cx.listener(|w, _, window, cx| w.jump_to_error_pub(window, cx))))
                    })
                    .children(suggestion.map(|s| {
                        btn("ev-hint", "Apply hint", None).on_click(cx.listener(move |w, _, window, cx| {
                            if let Some(tab) = w.active_tab().cloned() {
                                let e = tab.read(cx).editor.clone();
                                e.update(cx, |e, cx| e.replace_error_token(&s, cx));
                                window.focus(&e.focus_handle(cx), cx);
                            }
                        }))
                    }))
                    .child(
                        ui::button(t, "ev-copy", Btn::Ghost)
                            .h(px(28.))
                            .px(px(10.))
                            .child("Copy error")
                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(err_copy.clone()))),
                    ),
            );
        let right = (batch.len() > 1).then(|| {
            let from = failed.unwrap_or(0);
            div()
                .w(px(330.))
                .flex_none()
                .p(px(22.))
                .flex()
                .flex_col()
                .gap(px(12.))
                .border_l_1()
                .border_color(t.line)
                .child(ui::caps(t, "BATCH"))
                .child(div().flex().flex_col().gap(px(2.)).children(batch.iter().enumerate().map(|(i, b)| {
                    let (g, gc) = match b.state {
                        ItemState::Ok => ("✓", t.ok),
                        ItemState::Failed => ("✕", t.err),
                        ItemState::Cancelled => ("⊘", t.warn),
                        _ => ("·", t.tx4),
                    };
                    let meta = match (b.state, b.elapsed) {
                        (ItemState::Pending, _) => "not run".to_string(),
                        (_, Some(e)) => fmt_dur(e),
                        _ => String::new(),
                    };
                    div()
                        .h(px(30.))
                        .px(px(8.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.))
                        .when(b.state == ItemState::Failed, |d| d.bg(t.err_s).border_l_2().border_color(t.err))
                        .child(div().w(px(16.)).text_center().text_color(gc).child(g))
                        .child(div().w(px(14.)).text_color(t.tx3).child((i + 1).to_string()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_color(if b.state == ItemState::Pending { t.tx3 } else { t.tx })
                                .child(b.summary()),
                        )
                        .child(div().text_size(px(11.)).text_color(t.tx3).child(meta))
                })))
                .child(div().flex_1())
                .child(
                    ui::button(t, "ev-runfrom", Btn::Primary)
                        .h(px(30.))
                        .justify_center()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("Run from statement {}", from + 1))
                        // No shortcut hint: ⌥⌘↩ in the editor runs from the cursor, not from the
                        // statement that failed.
                        .on_click(cx.listener(move |w, _, _, cx| {
                            if let Some(tab) = w.active_tab().cloned() {
                                tab.update(cx, |t, cx| t.run_from(from, cx));
                            }
                        })),
                )
        });
        div().size_full().flex().flex_row().text_size(px(12.)).child(left).children(right)
    }

    pub(crate) fn jump_to_error_pub(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab().cloned() {
            let editor = tab.read(cx).editor.clone();
            if editor.update(cx, |e, cx| e.jump_to_error(cx)) {
                window.focus(&editor.focus_handle(cx), cx);
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Toasts (bottom-right, 4–6 s unless they need you) and the shortcuts sheet.

    pub(crate) fn render_toasts(&self, _window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = cx.theme();
        let mut out = Vec::new();
        let tab = self.active_tab().map(|t| t.read(cx));
        if let Some(at) = tab.and_then(|t| t.cancel_requested_at())
            && at.elapsed().as_millis() > 300
            && self.cancel_toast_hidden != Some(at)
        {
            out.push(
                toast(&t, t.warn)
                    .right(px(28.))
                    .bottom(px(48.))
                    .w(px(380.))
                    .child(div().text_color(t.warn).line_height(px(18.)).child("◌"))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .text_size(px(11.5))
                            .line_height(px(17.))
                            .child(div().text_color(t.tx).font_weight(FontWeight::SEMIBOLD).child(format!("Cancel sent · {:.1} s ago", at.elapsed().as_secs_f64())))
                            .child(
                                div().mt(px(4.)).flex().flex_row().gap(px(8.)).child(
                                    ui::button(&t, "toast-hide", Btn::Ghost)
                                        .h(px(24.))
                                        .px(px(9.))
                                        .text_size(px(11.))
                                        .text_color(t.tx3)
                                        .child("Hide")
                                        .on_click(cx.listener(move |w, _, _, cx| {
                                            w.cancel_toast_hidden = Some(at);
                                            cx.notify();
                                        })),
                                ),
                            ),
                    )
                    .into_any_element(),
            );
        }
        if let Some((name, _)) = &self.lar_toast {
            let top = metrics::TITLE_H + metrics::TOOLBAR_H + 12.;
            out.push(
                toast(&t, t.lar)
                    .right(px(22.))
                    .top(px(top))
                    .w(px(360.))
                    .border_1()
                    .border_color(t.lar)
                    .child(ui::lar_badge(&t, "◆").h(px(18.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .text_size(px(11.5))
                            .line_height(px(17.))
                            .child(div().text_color(t.tx).font_weight(FontWeight::SEMIBOLD).child(format!("LAR detected on {name}")))
                            .child(div().flex().flex_row().gap(px(10.)).text_size(px(10.5)).text_color(t.tx3).children(LAR_BUTTONS.iter().map(|(id, _, _, _, what)| {
                                let name = what.split_once(':').map(|(n, _)| n).unwrap_or(what).to_lowercase();
                                let key = crate::keymap::key_label(&format!("lar_{id}"));
                                div().flex().flex_row().gap(px(4.)).child(div().text_color(t.lar_t).child(key)).child(name)
                            }))),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    /// "Discard N pending changes?": something would replace the result the grid edits were
    /// made on (a statement, a resubscribe, a history entry). Nothing is lost
    /// silently: esc / ↩ keep them, ⌘⌫ discards and runs.
    pub(crate) fn render_confirm_discard(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tab = self.active_tab()?.read(cx);
        let p = tab.pending_run.as_ref()?;
        let t = cx.theme();
        let n = p.edits;
        let title = format!("Discard {n} pending change{}?", if n == 1 { "" } else { "s" });
        let go = format!("Discard & {}", p.verb);
        Some(
            div()
                .id("confirm-discard")
                .absolute()
                .inset_0()
                .bg(t.scrim)
                .occlude()
                .key_context("ConfirmDiscard Modal")
                .track_focus(&self.confirm_focus)
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(MouseButton::Left, cx.listener(|w, _, window, cx| w.resolve_pending_run(false, window, cx)))
                .child(
                    ui::modal(&t, "confirm-discard-card")
                        .w(px(440.))
                        .child(ui::modal_header(
                            &t,
                            None,
                            title,
                            ui::modal_close(&t, "cd-close").on_click(cx.listener(|w, _, window, cx| w.resolve_pending_run(false, window, cx))),
                        ))
                        .child(
                            ui::modal_footer(&t)
                                .child(div().flex_1())
                                .child(
                                    ui::button(&t, "cd-cancel", Btn::Secondary)
                                        .h(px(28.))
                                        .child("Keep edits")
                                        .child(ui::key_hint("esc"))
                                        .on_click(cx.listener(|w, _, window, cx| w.resolve_pending_run(false, window, cx))),
                                )
                                .child(
                                    ui::button(&t, "cd-discard", Btn::Danger)
                                        .h(px(28.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(go)
                                        .child(ui::key_hint("⌘⌫"))
                                        .on_click(cx.listener(|w, _, window, cx| w.resolve_pending_run(true, window, cx))),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The confirmation sheets: open transactions in the way of a close, a disconnect, a switch
    /// of connection or database, a connection saved or deleted, or a quit (Commit / Rollback /
    /// Cancel), or the active tab's run waiting for an answer: dangerous
    /// statements (Run / Cancel) or a failed transaction (Rollback & run / Rollback / Cancel).
    pub(crate) fn render_tx_sheet(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::tab::{ConfirmChoice, TabConfirm};
        let t = cx.theme();
        let answer = |c: ConfirmChoice| cx.listener(move |w: &mut Workspace, _: &gpui::ClickEvent, window, cx| w.sheet_answer(c, window, cx));
        let btn = |id: &'static str, kind: Btn, label: SharedString, key: &'static str| {
            ui::button(&t, id, kind).h(px(28.)).child(label).child(ui::key_hint(key))
        };
        let cancel = btn("tx-cancel", Btn::Secondary, "Cancel".into(), "esc").on_click(answer(ConfirmChoice::Cancel)).into_any_element();
        let mut body: Vec<AnyElement> = Vec::new();
        let mut buttons = vec![cancel];
        let mut footer: Option<AnyElement> = None;
        let title: SharedString;
        if let Some(p) = &self.tx_prompt {
            let n = p.tabs.len();
            title = if n == 1 { "Open transaction".into() } else { format!("{n} open transactions").into() };
            for tab in &p.tabs {
                let tr = tab.read(cx);
                let conn = tr.connection().cloned();
                body.push(
                    div()
                        .h(px(28.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .child(ui::chip(conn.as_ref().map(|c| t.profile_color(&c.profile)).unwrap_or(t.tx4), 8.))
                        .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(tr.title(cx)))
                        .children(conn.map(|c| div().flex_none().ml(px(6.)).text_color(t.tx3).child(c.profile.name.clone())))
                        .child(div().flex_1())
                        .child(tx_chip(&t, &tr.tx(), false))
                        .into_any_element(),
                );
            }
            if let Some(e) = &p.error {
                body.push(div().pt(px(4.)).text_color(t.err).line_height(px(17.)).child(e.clone()).into_any_element());
            }
            let verb = p.then.verb();
            buttons.push(btn("tx-rollback", Btn::Secondary, format!("Rollback & {verb}").into(), "⌘⌫").on_click(answer(ConfirmChoice::Rollback)).into_any_element());
            let commit: SharedString = if p.busy { "Committing…".into() } else { format!("Commit & {verb}").into() };
            buttons.push(
                btn("tx-commit", if p.busy { Btn::Ghost } else { Btn::Primary }, commit, "⌘↩")
                    .font_weight(FontWeight::SEMIBOLD)
                    .on_click(answer(ConfirmChoice::Run))
                    .into_any_element(),
            );
        } else {
            let tab = self.active_tab()?.read(cx);
            let conn = tab.connection().cloned();
            match tab.confirm.as_ref()? {
                TabConfirm::Guard { stmts, findings } => {
                    let n = findings.len();
                    title = match findings.first() {
                        Some(f) if n == 1 && matches!(f.rule, "write" | "ddl") => format!("{} on production", f.label).into(),
                        Some(f) if n == 1 => f.label.clone().into(),
                        _ => format!("Confirm {n} statements").into(),
                    };
                    for f in findings.iter().take(4) {
                        let sql = stmts.get(f.index).map(|s| s.sql.as_str()).unwrap_or("");
                        body.push(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(6.))
                                .when(n > 1, |d| {
                                    d.child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .gap(px(8.))
                                            .child(div().text_color(t.err).font_weight(FontWeight::SEMIBOLD).child(f.label.clone()))
                                            .child(div().text_color(t.tx3).child(format!("statement {}", f.index + 1))),
                                    )
                                })
                                .child(code_block(&t, sql, f.target.as_deref()))
                                .into_any_element(),
                        );
                    }
                    if n > 4 {
                        body.push(div().text_color(t.tx3).child(format!("+{}", n - 4)).into_any_element());
                    }
                    buttons.push(
                        btn("tx-run", Btn::Danger, "Run".into(), "⌘↩").font_weight(FontWeight::SEMIBOLD).on_click(answer(ConfirmChoice::Run)).into_any_element(),
                    );
                }
                TabConfirm::Failed { .. } => {
                    title = "Transaction failed".into();
                    let err = tab
                        .batch_items()
                        .iter()
                        .rev()
                        .find_map(|b| b.rs.as_ref().filter(|r| r.phase() == Phase::Failed).and_then(|r| r.status().error));
                    body.push(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.))
                            .child(tx_chip(&t, &tab.tx(), false))
                            .children(err.map(|e| {
                                div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.err).child(match e.code {
                                    Some(c) => format!("{c} {}", e.message),
                                    None => e.message,
                                })
                            }))
                            .into_any_element(),
                    );
                    buttons.push(btn("tx-rollback", Btn::Secondary, "Rollback".into(), "⌘⌫").on_click(answer(ConfirmChoice::Rollback)).into_any_element());
                    buttons.push(
                        btn("tx-rerun", Btn::Primary, "Rollback & run".into(), "⌘↩")
                            .font_weight(FontWeight::SEMIBOLD)
                            .on_click(answer(ConfirmChoice::Run))
                            .into_any_element(),
                    );
                }
                // The activity monitor: Cancel / Terminate a session (marked when it is one of this
                // app's), Reset the statistics. On production, Terminate reads like the guard's
                // production sheet.
                TabConfirm::Monitor(p) => {
                    use crate::activity::Pending;
                    use dataman_core::activity::Signal;
                    buttons[0] = btn("tx-cancel", Btn::Secondary, "Back".into(), "esc").on_click(answer(ConfirmChoice::Cancel)).into_any_element();
                    let production = conn.as_ref().is_some_and(|c| c.profile.env.is_production());
                    match p {
                        Pending::Signal { signal, session, own } => {
                            let on_prod = production && *signal == Signal::Terminate;
                            title = format!("{} pid {}{}", signal.label(), session.pid, if on_prod { " on production" } else { "" }).into();
                            let facts: Vec<String> =
                                [session.user.clone(), session.app.clone(), session.db.clone(), session.state.clone()].into_iter().flatten().filter(|s| !s.is_empty()).collect();
                            body.push(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.))
                                    .when(*own, |d| d.child(crate::activity::own_tag(&t)))
                                    .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx2).child(facts.join(" · ")))
                                    .into_any_element(),
                            );
                            if let Some(q) = session.visible_query() {
                                body.push(code_block(&t, q, None).into_any_element());
                            }
                            let (kind, label) = match signal {
                                Signal::Cancel => (Btn::Primary, "Cancel query"),
                                Signal::Terminate => (Btn::Danger, "Terminate session"),
                            };
                            buttons.push(btn("tx-run", kind, label.into(), "⌘↩").font_weight(FontWeight::SEMIBOLD).on_click(answer(ConfirmChoice::Run)).into_any_element());
                        }
                        Pending::Reset => {
                            title = "Reset pg_stat_statements".into();
                            body.push(code_block(&t, dataman_core::activity::RESET_PGSS, None).into_any_element());
                            buttons.push(btn("tx-run", Btn::Danger, "Reset".into(), "⌘↩").font_weight(FontWeight::SEMIBOLD).on_click(answer(ConfirmChoice::Run)).into_any_element());
                        }
                    }
                }
            }
            // Where it runs: the connection, and its environment when set.
            footer = conn.map(|c| {
                let env = c.profile.env;
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .min_w_0()
                    .text_size(px(11.))
                    .child(ui::chip(t.profile_color(&c.profile), 8.))
                    .child(div().text_color(t.tx2).overflow_hidden().whitespace_nowrap().text_ellipsis().child(c.profile.name.clone()))
                    .when(!env.is_none(), |d| d.child(div().text_color(if env.is_production() { t.err } else { t.tx3 }).child(env.label())))
                    .into_any_element()
            });
        }
        Some(
            div()
                .id("tx-sheet")
                .absolute()
                .inset_0()
                .bg(t.scrim)
                .occlude()
                .key_context("TxSheet Modal")
                .track_focus(&self.sheet_focus)
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(MouseButton::Left, cx.listener(|w, _, window, cx| w.sheet_answer(ConfirmChoice::Cancel, window, cx)))
                .child(
                    ui::modal(&t, "tx-sheet-card")
                        .w(px(520.))
                        .child(ui::modal_header(&t, None, title, ui::modal_close(&t, "tx-close").on_click(answer(ConfirmChoice::Cancel))))
                        .when(!body.is_empty(), |d| d.child(div().px(px(20.)).py(px(16.)).flex().flex_col().gap(px(10.)).children(body)))
                        .child(
                            ui::modal_footer(&t)
                                .children(footer)
                                .child(div().flex_1())
                                .children(buttons),
                        ),
                )
                .into_any_element(),
        )
    }

    pub(crate) fn render_shortcuts(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = cx.theme();
        div()
            .id("shortcuts")
            .absolute()
            .top(px(metrics::TITLE_H))
            .left_0()
            .right_0()
            .bottom_0()
            .bg(t.scrim)
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, cx.listener(|w, _, _, cx| {
                w.shortcuts_open = false;
                cx.notify();
            }))
            .child(crate::shortcuts::sheet(
                &t,
                ui::modal_close(&t, "shortcuts-close").on_click(cx.listener(|w, _, _, cx| {
                    w.shortcuts_open = false;
                    cx.notify();
                })),
            ))
    }
}

/// An MCP client's name as the app shows it: its own words, control characters out, never
/// empty (the client chooses it: it is shown, never trusted).
pub(crate) fn agent_name(client: &str) -> String {
    let s: String = client.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.is_empty() { "agent".into() } else { cut_chars(&s, 64) }
}

/// At most `n` characters, an ellipsis for the rest.
pub(crate) fn cut_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s.to_string(),
    }
}

/// The title bar's explorer toggle (`icon_button`'s look): the sidebar glyph, its left part
/// filled as far as the explorer is open (`open`, 0 to 1), so the fill sweeps in and out with
/// the explorer's own slide (same tween, same easing; at once with "Reduce motion"). `mirrored`
/// (the explorer in the right dock): the glyph and its fill from the right.
fn explorer_toggle(t: &Theme, open: f32, mirrored: bool) -> gpui::Stateful<Div> {
    let (hov, prs, ink) = (t.hov, t.prs, t.tx3);
    // The glyph's outer pane in its 16 pt box (`sidebar.svg`: the frame from x 2.5 to 13.5 and y
    // 3 to 13, its divider at x 6.5; `sidebar-right.svg` the same mirrored, at x 9.5).
    let pane = div().absolute().top(px(3.)).h(px(10.)).w(px(4. * open.clamp(0., 1.))).bg(ink);
    let (pane, glyph) = if mirrored { (pane.right(px(2.5)), Icon::SidebarRight) } else { (pane.left(px(2.5)), Icon::Sidebar) };
    div()
        .id("toggle-explorer")
        .flex_none()
        .size(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .hover(move |s| s.bg(hov))
        .active(move |s| s.bg(prs))
        .child(div().relative().size(px(16.)).child(icon(glyph).size(px(16.)).text_color(ink)).child(pane))
}

/// The focus ring of a region: a 1 pt neutral line and a faint 2 pt band inside it (square:
/// the regions are flush with each other and with the window's edges, so a ring outside would
/// be cut off). Neutral, not the accent: blue is the selection's.
pub(crate) fn focus_ring(t: &Theme) -> Div {
    div()
        .absolute()
        .inset_0()
        .border_1()
        .border_color(t.focus)
        .child(div().absolute().inset_0().border_2().border_color(t.focus_s))
}

/// A statement on a sheet: its first lines in a card (an ellipsis when there are more), and
/// the object it acts on.
pub(crate) fn code_block(t: &Theme, sql: &str, target: Option<&str>) -> Div {
    const LINES: usize = 5;
    let lines: Vec<&str> = sql.trim().lines().map(str::trim_end).collect();
    let more = lines.len() > LINES;
    div()
        .px(px(12.))
        .py(px(9.))
        .bg(t.card)
        .border_1()
        .border_color(t.line)
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .flex()
                .flex_col()
                .text_size(px(12.))
                .line_height(px(18.))
                .text_color(t.tx)
                .children(lines.into_iter().take(LINES).map(|l| div().whitespace_nowrap().overflow_hidden().text_ellipsis().child(l.replace('\t', "    "))))
                .when(more, |d| d.child(div().text_color(t.tx3).child("…"))),
        )
        .children(target.map(|tg| {
            div()
                .flex()
                .flex_row()
                .gap(px(8.))
                .text_size(px(11.))
                .child(div().text_color(t.tx4).child("target"))
                .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx2).child(tg.to_string()))
        }))
}

pub(crate) fn toast(t: &Theme, accent: Hsla) -> Stateful<Div> {
    div()
        .id("toast")
        .absolute()
        .flex()
        .flex_row()
        .gap(px(12.))
        .pl(px(16.))
        .pr(px(14.))
        .py(px(14.))
        .bg(t.pop)
        .shadow(t.ring())
        .border_l_2()
        .border_color(accent)
        .occlude()
}

/// SQLSTATE → condition name, for the codes people meet most.
fn condition_name(code: &str) -> &'static str {
    match code {
        "42703" => "undefined_column",
        "42P01" => "undefined_table",
        "42601" => "syntax_error",
        "42883" => "undefined_function",
        "42P07" => "duplicate_table",
        "42701" => "duplicate_column",
        "42501" => "insufficient_privilege",
        "42804" => "datatype_mismatch",
        "23505" => "unique_violation",
        "23503" => "foreign_key_violation",
        "23502" => "not_null_violation",
        "23514" => "check_violation",
        "22P02" => "invalid_text_representation",
        "22012" => "division_by_zero",
        "22001" => "string_data_right_truncation",
        "22003" => "numeric_value_out_of_range",
        "57014" => "query_canceled",
        "25P02" => "in_failed_sql_transaction",
        "25006" => "read_only_sql_transaction",
        "40001" => "serialization_failure",
        "40P01" => "deadlock_detected",
        "28P01" => "invalid_password",
        "3D000" => "invalid_catalog_name",
        "0A000" => "feature_not_supported",
        _ => "",
    }
}
