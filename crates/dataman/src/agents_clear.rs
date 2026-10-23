//! Audit › Clear…: the whole log, what came before a day picked on a calendar, or all but the
//! last 30 days, 3 or 6 months. A sheet counts the calls that go (with what their agents
//! received) and nothing goes before the user says so. Agents have no way to do this.

use chrono::{Datelike, Local, NaiveDate};
use dataman_mcp::audit::{Clear, Recent};
use gpui::{AnyElement, App, Context, FontWeight, SharedString, Window, div, prelude::*};

use crate::agents::{Act, AgentsPage};
use crate::assets::Icon;
use crate::theme::{Theme, px};
use crate::ui;

gpui::actions!(agents_clear, [KeepCalls, DeleteCalls]);

const SHEET: &str = "AgentsClear";

/// As the other destructive sheets: Esc and ↩ keep, ⌘⌫ deletes.
pub fn bind_keys(cx: &mut App) {
    let c = Some(SHEET);
    cx.bind_keys([
        crate::keymap::kb("escape", KeepCalls, c),
        crate::keymap::kb("enter", KeepCalls, c),
        crate::keymap::kb("cmd-backspace", DeleteCalls, c),
    ]);
}

pub(crate) struct ClearSheet {
    pub(crate) what: Clear,
    /// When the sheet opened: today, and for All the moment after which calls stay.
    opened: chrono::DateTime<Local>,
    /// The calls that go; None while counting.
    pub(crate) count: Option<usize>,
    /// The calendar's month (its first day).
    pub(crate) month: NaiveDate,
    busy: bool,
    error: Option<String>,
}

impl ClearSheet {
    pub(crate) fn cutoff(&self) -> chrono::DateTime<chrono::Utc> {
        self.what.cutoff(&self.opened)
    }

    fn today(&self) -> NaiveDate {
        self.opened.date_naive()
    }
}

fn first_of(d: NaiveDate) -> NaiveDate {
    d.with_day(1).unwrap_or(d)
}

fn long_day(d: NaiveDate) -> String {
    d.format("%a, %b %-d, %Y").to_string()
}

fn calls(n: usize) -> String {
    if n == 1 { "1 call".into() } else { format!("{} calls", crate::grid::ui_count(n)) }
}

/// The Clear… menu's items.
pub(crate) fn menu_items() -> Vec<(SharedString, bool, Act)> {
    let keep = |r: Recent| match r {
        Recent::Days30 => "Keep Only the Last 30 Days…",
        Recent::Months3 => "Keep Only the Last 3 Months…",
        Recent::Months6 => "Keep Only the Last 6 Months…",
    };
    let mut v: Vec<(SharedString, bool, Act)> = vec![("Clear All…".into(), false, Act::Clear(None)), ("Clear Before a Date…".into(), false, Act::Clear(Some(Clear::Before(NaiveDate::MIN))))];
    v.extend(Recent::ALL.map(|r| (keep(r).into(), false, Act::Clear(Some(Clear::Keep(r))))));
    v
}

impl AgentsPage {
    /// Opens the sheet; `None` clears all, `Before(MIN)` asks for the day (a month back at first).
    pub(crate) fn open_clear(&mut self, what: Option<Clear>, window: &mut Window, cx: &mut Context<Self>) {
        let opened = Local::now();
        let what = match what {
            None => Clear::All,
            Some(Clear::Before(d)) if d == NaiveDate::MIN => Clear::Before(Recent::Days30.since(opened.date_naive())),
            Some(w) => w,
        };
        let month = first_of(what.first_kept(opened.date_naive()).unwrap_or(opened.date_naive()));
        self.clear = Some(ClearSheet { what, opened, count: None, month, busy: false, error: None });
        self.count_clear(cx);
        window.focus(&self.clear_focus, cx);
        cx.notify();
    }

    fn count_clear(&mut self, cx: &mut Context<Self>) {
        let Some(s) = self.clear.as_mut() else { return };
        s.count = None;
        let (what, cutoff, path) = (s.what, s.cutoff(), self.core.dirs.mcp_audit_file());
        let task = cx.background_spawn(async move { dataman_mcp::audit::count_before(&path, cutoff) });
        cx.spawn(async move |this, cx| {
            let n = task.await;
            let _ = this.update(cx, |v, cx| {
                if let Some(s) = v.clear.as_mut().filter(|s| s.what == what) {
                    s.count = Some(n);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn pick_clear_day(&mut self, d: NaiveDate, cx: &mut Context<Self>) {
        let Some(s) = self.clear.as_mut().filter(|s| matches!(s.what, Clear::Before(_)) && !s.busy) else { return };
        if d > s.today() || s.what == Clear::Before(d) {
            return;
        }
        s.what = Clear::Before(d);
        s.month = first_of(d);
        s.error = None;
        self.count_clear(cx);
        cx.notify();
    }

    pub(crate) fn turn_clear_month(&mut self, by: i32, cx: &mut Context<Self>) {
        let Some(s) = self.clear.as_mut() else { return };
        let m = if by < 0 { s.month.checked_sub_months(chrono::Months::new(1)) } else { s.month.checked_add_months(chrono::Months::new(1)) };
        if let Some(m) = m.filter(|m| *m <= first_of(s.today())) {
            s.month = m;
            cx.notify();
        }
    }

    pub(crate) fn close_clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.clear.as_ref().is_some_and(|s| s.busy) {
            return;
        }
        self.clear = None;
        self.focus_list(window, cx);
        cx.notify();
    }

    /// Clears what the sheet counted, then reads the log again.
    pub(crate) fn confirm_clear(&mut self, cx: &mut Context<Self>) {
        let Some(s) = self.clear.as_mut().filter(|s| !s.busy && s.count.is_some_and(|n| n > 0)) else { return };
        s.busy = true;
        s.error = None;
        let (cutoff, path) = (s.cutoff(), self.core.dirs.mcp_audit_file());
        let task = cx.background_spawn(async move { dataman_mcp::audit::clear_before(&path, cutoff) });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |v, cx| {
                match r {
                    Ok(done) => {
                        log::info!("audit cleared before {cutoff}: {} calls, {} replies", done.calls, done.replies);
                        v.clear = None;
                        v.cleared();
                        v.reload(cx);
                    }
                    Err(e) => {
                        if let Some(s) = v.clear.as_mut() {
                            s.busy = false;
                            s.error = Some(format!("Could not clear the audit: {e}"));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn calendar(&self, t: &Theme, s: &ClearSheet, cx: &mut Context<Self>) -> AnyElement {
        let Clear::Before(on) = s.what else { return div().into_any_element() };
        let today = s.today();
        let first = s.month;
        let can_next = first < first_of(today);
        let turn = |id: &'static str, i: Icon, by: i32, enabled: bool, cx: &mut Context<Self>| {
            if enabled {
                ui::icon_button(t, id, i, 24., 12.).on_click(cx.listener(move |v, _, _, cx| v.turn_clear_month(by, cx))).into_any_element()
            } else {
                ui::icon_button_inert(t, id, i, 24., 12.).opacity(0.4).into_any_element()
            }
        };
        let head = div()
            .h(px(28.))
            .flex()
            .flex_row()
            .items_center()
            .child(turn("agents-clear-prev", Icon::ChevronLeft, -1, true, cx))
            .child(div().flex_1().text_center().font_weight(FontWeight::SEMIBOLD).child(first.format("%B %Y").to_string()))
            .child(turn("agents-clear-next", Icon::ChevronRight, 1, can_next, cx));
        let names = div().flex().flex_row().children(["Su", "Mo", "Tu", "We", "Th", "Fr", "Sa"].map(|d| div().w(px(32.)).h(px(22.)).flex().items_center().justify_center().text_size(px(10.5)).text_color(t.tx3).child(d)));
        let lead = first.weekday().num_days_from_sunday() as usize;
        let days = first.checked_add_months(chrono::Months::new(1)).map_or(31, |n| (n - first).num_days() as usize);
        let weeks = (lead + days).div_ceil(7);
        let rows = (0..weeks).map(|w| {
            div().flex().flex_row().children((0..7).map(|k| {
                let n = w * 7 + k;
                let cell = div().w(px(32.)).h(px(26.)).flex().items_center().justify_center();
                if n < lead || n - lead >= days {
                    return cell.into_any_element();
                }
                let d = first + chrono::Days::new((n - lead) as u64);
                let id = SharedString::from(format!("agents-clear-day-{d}"));
                let (sel, future) = (d == on, d > today);
                let hov = t.hov;
                cell.children(ui::here(&id.clone().into()))
                    .id(id)
                    .text_color(if future { t.tx4 } else if d < on { t.err } else { t.tx })
                    .when(d == today, |c| c.font_weight(FontWeight::BOLD))
                    .when(sel, |c| c.bg(t.prs).border_1().border_color(t.line2))
                    .when(!sel && !future, move |c| c.hover(move |s| s.bg(hov)))
                    .child(d.day().to_string())
                    .when(!future, |c| c.on_click(cx.listener(move |v, _, _, cx| v.pick_clear_day(d, cx))))
                    .into_any_element()
            }))
        });
        div()
            .children(ui::here(&"agents-clear-calendar".into()))
            .flex_none()
            .w(px(7. * 32. + 16.))
            .p(px(8.))
            .bg(t.field)
            .border_1()
            .border_color(t.line)
            .flex()
            .flex_col()
            .child(head)
            .child(names)
            .children(rows)
            .into_any_element()
    }

    pub(crate) fn render_clear(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let s = self.clear.as_ref()?;
        let title = match s.what {
            Clear::All => "Clear the whole audit?".to_string(),
            Clear::Before(_) => "Clear the audit before a date?".to_string(),
            Clear::Keep(r) => format!("Keep only {}?", r.label()),
        };
        let first = s.what.first_kept(s.today());
        let what = match (s.count, first) {
            (None, _) => "Counting the calls…".to_string(),
            (Some(0), None) => "The audit holds no calls.".to_string(),
            (Some(0), Some(d)) => format!("No call was logged before {}.", long_day(d)),
            (Some(n), None) => format!("All {} in the audit go, with what their agents received.", calls(n)),
            (Some(n), Some(d)) => format!("{} logged before {} go, with what their agents received. Calls from that day on stay.", calls(n), long_day(d)),
        };
        let ready = !s.busy && s.count.is_some_and(|n| n > 0);
        let body = div()
            .px(px(20.))
            .py(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .line_height(px(18.))
            .when(matches!(s.what, Clear::Before(_)), |d| d.child(div().text_color(t.tx2).child("Calls logged before the day you pick go; that day stays.")).child(self.calendar(t, s, cx)))
            .child(div().children(ui::here(&"agents-clear-what".into())).text_color(t.tx).child(what))
            .when(ready, |d| d.child(div().text_color(t.tx3).child("This cannot be undone.")))
            .children(s.error.clone().map(|e| div().text_color(t.err).child(e)));
        let label = match s.count {
            Some(n) if n > 0 => format!("Delete {}", calls(n)),
            _ => "Delete".to_string(),
        };
        let delete = ui::button(t, "agents-clear-delete", ui::Btn::Danger).h(px(28.)).font_weight(FontWeight::SEMIBOLD).child(if s.busy { "Deleting…".to_string() } else { label }).child(ui::key_hint("⌘⌫"));
        let delete = if ready { delete.on_click(cx.listener(|v, _, _, cx| v.confirm_clear(cx))).into_any_element() } else { delete.opacity(0.5).into_any_element() };
        Some(
            div()
                .id("agents-clear-sheet")
                .absolute()
                .inset_0()
                .bg(t.scrim)
                .occlude()
                .track_focus(&self.clear_focus)
                .key_context(format!("{SHEET} Modal").as_str())
                .on_action(cx.listener(|v, _: &KeepCalls, window, cx| v.close_clear(window, cx)))
                .on_action(cx.listener(|v, _: &DeleteCalls, _, cx| v.confirm_clear(cx)))
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(|v, _, window, cx| v.close_clear(window, cx)))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    ui::modal(t, "agents-clear-card")
                        .w(px(460.))
                        .child(ui::modal_header(t, None, title, ui::modal_close(t, "agents-clear-close").on_click(cx.listener(|v, _, window, cx| v.close_clear(window, cx)))))
                        .child(body)
                        .child(
                            ui::modal_footer(t)
                                .child(div().flex_1())
                                .child(
                                    ui::button(t, "agents-clear-cancel", ui::Btn::Secondary)
                                        .h(px(28.))
                                        .child("Keep them")
                                        .child(ui::key_hint("esc"))
                                        .on_click(cx.listener(|v, _, window, cx| v.close_clear(window, cx))),
                                )
                                .child(delete),
                        ),
                )
                .into_any_element(),
        )
    }
}
