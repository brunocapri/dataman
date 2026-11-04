//! Agents › Protection: one list of connections and, for the selected one, one page: how agents
//! reach it (or its agent login, last), Access (what agents may do there, and how much) and
//! Privacy (what they may see). Everything writes the connection's `mcp_rules`; no MCP tool can.

use dataman_core::privacy::{self, AgentView, ItemKind, Mask, Names, ReadFrom, SensitiveCol, Visible, VisibleItem, VisibleMode};
use dataman_core::settings::{MCP_MAX_ROWS, MCP_WRITE_ROW_LIMIT};
use dataman_core::nosql::ItemKind as TreeKind;
use dataman_core::{ConnectionProfile, Engine, McpAccess};
use std::collections::HashSet;
use std::time::{Duration, Instant};

use gpui::{Animation, AnimationExt, AnyElement, ClickEvent, Context, FontWeight, SharedString, Window, div, prelude::*};

use crate::agents::{Act, AgentsPage, Tip, field, qmark};
use crate::agents_suggest::Which;
use crate::assets::{Icon, icon};
use crate::theme::{Theme, px};
use crate::ui;

/// The connection list's width; Protection's title sits over it, in a column as wide.
const CONNS_W: f32 = 250.;
/// The sections' widest: wider, a line of help or a row of fields reads badly.
pub(crate) const BODY_W: f32 = 720.;
/// The body's top padding: a section brought to the top sits where the first one does.
const BODY_TOP: f32 = 24.;

/// A section of the page brought to its top once laid out (a link to the agent login, a stage).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Jump {
    Privacy,
    /// The agent login section, or the card that offers one at the top.
    Login,
}

pub(crate) fn tier_tip() -> Tip {
    Tip::new("MCP tier")
        .lead("How far agents can go on this connection.")
        .item("Off", "Agents can't use it.")
        .item("Read", "Reads only.")
        .item("Write", "Also inserts, and updates or deletes with a real WHERE, up to Rows per write.")
        .item("Full", "Also destructive statements (DROP, TRUNCATE, a DELETE without WHERE…). Each one waits for your Allow.")
        .note("On production, every write waits for your Allow.")
}

fn limits_tip() -> Tip {
    Tip::new("Limits")
        .lead("Per call, for agents only. Until you pick another, each starts on its default.")
        .item("Time per call · 5 min", "Longest a statement runs before it is stopped. An agent can ask for less. ∞ lets it ask for up to a day.")
        .item("Rows per read · 1,000", "Most rows an agent gets back from one read. ∞: as many as it asks for.")
        .item("Rows per write · 100", "A write that changes more rows waits for your Allow. ∞: no write waits for its row count. From the Write tier up.")
        .note("A read's size isn't limited: the rows are.")
}

/// Masking happens in Masked data only, in two layers: the Masks inside every
/// value, and this connection's sensitive columns whole.
pub(crate) fn sees_tip() -> Tip {
    Tip::new("What the agent sees")
        .lead("How much of this connection's data reaches an agent. Your own tabs always show everything.")
        .item("Data", "Rows as stored. Nothing is masked.")
        .item("Masked data", "Values a Mask recognizes (cards, e-mails, keys…) are masked in everything the agent receives. The Sensitive columns below are masked whole, on top of that. Host, port and user aren't shown.")
        .item("Structure only", "Tables, columns, types and query plans. No rows are read or written; only the structure changes the MCP tier allows. Host, port and user aren't shown.")
        .note("In every mode, what Visibility hides stays hidden and errors don't quote values.")
}

pub(crate) fn sens_tip() -> Tip {
    Tip::new("Sensitive columns")
        .lead("Columns masked in Masked data, in addition to the values Masks already catch.")
        .item("Redacted", "Agents get ‹hidden›.")
        .item("Partial", "Only the first or last 1–4 characters show. A value no longer than that is hidden whole.")
}

fn read_from_tip() -> Tip {
    Tip::new("Read from")
        .lead("Which members of the replica set answer agents' reads.")
        .item("Primary", "Always the primary.")
        .item("Prefer secondary", "A secondary when one is up, otherwise the primary. Keeps agents' load off the primary.")
        .item("Secondary", "Secondaries only. Reads fail while none is up.")
}

fn vis_tip(e: Engine) -> Tip {
    let (one, many) = nouns(e);
    Tip::new(format!("{} visibility", capitalized(one)))
        .lead("What exists for agents on this connection, in every mode.")
        .item("Everything except", format!("The list holds what is hidden. {} created later are visible.", capitalized(many)))
        .item("Only these", format!("The list holds the {many} and schemas agents can see. Everything else is hidden, {many} created later too."))
        .lead(format!("Hidden {many} and schemas aren't listed, described or queryable. Hidden columns are removed: Dataman narrows each query so, for agents, they don't exist."))
        .example("public.payments (table) · audit.* (schema) · users.password_hash (column)")
        .note("Functions, copies of the data in other tables and statements Dataman can't read are refused or not covered. Only database permissions block everything.")
}

/// What the engine calls a table, one and many.
pub(crate) fn nouns(e: Engine) -> (&'static str, &'static str) {
    let n = e.nouns();
    (n.one, n.many)
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn tier_label(a: McpAccess) -> &'static str {
    match a {
        McpAccess::Off => "Off",
        McpAccess::Read => "Read",
        McpAccess::Write => "Write",
        McpAccess::Full => "Full",
    }
}

fn placeholder(e: Engine) -> String {
    let n = e.nouns().one;
    match e {
        // Keys are named by patterns, not listed: no `*` for all of them.
        Engine::Redis => format!("{n} pattern or {n}.field"),
        _ => format!("{n}, {n}.{} or *", part(e)),
    }
}

/// What a table's parts are called: columns, or a document's fields.
fn part(e: Engine) -> &'static str {
    if e.is_sql() { "column" } else { "field" }
}

fn time_label(s: u64) -> String {
    match s {
        privacy::UNLIMITED => "∞".into(),
        s if s < 60 => format!("{s} s"),
        s => format!("{} min", s / 60),
    }
}

fn count_label(n: u64) -> String {
    if n == privacy::UNLIMITED {
        return "∞".into();
    }
    let s = n.to_string();
    let mut out = String::new();
    for (k, c) in s.chars().enumerate() {
        if k > 0 && (s.len() - k).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Where the connection is, as the pane's header says it.
fn place(p: &ConnectionProfile) -> String {
    match () {
        _ if p.engine.is_file() => p.summary(),
        _ if p.is_socket() || p.port == 0 => p.host.clone(),
        _ => format!("{}:{}", p.host, p.port),
    }
}

/// A parent section: its title over one bordered group, its subsections divided by faint lines.
pub(crate) fn section(t: &Theme, id: &str, title: &str, subs: Vec<gpui::Div>) -> gpui::Div {
    let faint = t.grid_line;
    div()
        .children(ui::here(&SharedString::from(id.to_string()).into()))
        // A width of its own (not just a cap): laid out from content alone, the page's text is
        // measured over and over, every frame of a scroll.
        .w_full()
        .max_w(px(BODY_W))
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(div().text_size(px(15.)).font_weight(FontWeight::BOLD).text_color(t.tx).child(title.to_string()))
        .child(div().border_1().border_color(t.line).rounded(px(3.)).flex().flex_col().children(subs.into_iter().enumerate().map(move |(k, s)| s.px(px(20.)).py(px(18.)).when(k > 0, |d| d.border_t_1().border_color(faint)))))
}

/// Protection's sections: the title, then the subsections with no box around them, a line between.
pub(crate) fn flat_section(t: &Theme, id: &str, title: &str, subs: Vec<gpui::Div>) -> gpui::Div {
    div()
        .children(ui::here(&SharedString::from(id.to_string()).into()))
        .w_full()
        .max_w(px(BODY_W))
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(div().text_size(px(15.)).font_weight(FontWeight::BOLD).text_color(t.tx).child(title.to_string()))
        .child(div().flex().flex_col().children(subs.into_iter().enumerate().map(|(k, s)| s.py(px(18.)).when(k == 0, |d| d.pt(px(6.))).when(k > 0, |d| d.border_t_1().border_color(t.line)))))
}

/// A subsection: its title and `?`, the control below.
pub(crate) fn sub(head: impl IntoElement) -> gpui::Div {
    div().flex().flex_col().gap(px(10.)).child(head)
}

pub(crate) fn sub_title(t: &Theme, tip_id: &str, title: &str, tip: Tip) -> gpui::Div {
    div().flex().flex_row().items_center().gap(px(7.)).text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).text_color(t.tx).child(title.to_string()).child(qmark(t, tip_id, tip).font_weight(FontWeight::NORMAL))
}

/// A bordered table: rows divided by faint lines.
fn table(t: &Theme, rows: Vec<AnyElement>) -> gpui::Div {
    let faint = t.grid_line;
    div().border_1().border_color(t.line).rounded(px(2.)).flex().flex_col().children(rows.into_iter().enumerate().map(move |(k, r)| div().when(k > 0, |d| d.border_t_1().border_color(faint)).child(r)))
}

const GROW: Duration = Duration::from_millis(220);
const FADE: Duration = Duration::from_millis(200);
/// Collections (or indices) Scan again samples, at most.
const SCAN_MAX: usize = 200;

/// A row that just appeared grows to its height and fades in; any other is drawn as it is.
fn grow(row: gpui::Stateful<gpui::Div>, h: f32, key: &str, new: bool) -> AnyElement {
    if !new {
        return row.into_any_element();
    }
    row.overflow_hidden()
        .with_animation(SharedString::from(format!("agents-in-{key}")), Animation::new(GROW).with_easing(gpui::ease_out_quint()), move |d, k| d.h(px(h * k)).opacity(k))
        .into_any_element()
}

/// A row on its way out: it shrinks to nothing and fades.
fn shrink(row: gpui::Stateful<gpui::Div>, h: f32, key: &str) -> AnyElement {
    row.overflow_hidden()
        .with_animation(SharedString::from(format!("agents-out-{key}")), Animation::new(GROW).with_easing(gpui::ease_out_quint()), move |d, k| d.h(px(h * (1. - k))).opacity(1. - k))
        .into_any_element()
}

/// A row that just appeared grows in, one that is being removed shrinks away.
fn motion(row: gpui::Stateful<gpui::Div>, h: f32, key: &str, new: &HashSet<String>, leaving: &HashSet<String>) -> AnyElement {
    if leaving.contains(key) { shrink(row, h, key) } else { grow(row, h, key, new.contains(key)) }
}

/// Between the dimmed look (what agents see is not Masked data) and the full one: at once, or
/// fading when what agents see has just changed.
fn dim(el: gpui::Div, id: &'static str, masked: bool, fading: bool) -> AnyElement {
    if !fading {
        return el.when(!masked, |d| d.opacity(0.55)).into_any_element();
    }
    el.with_animation((id, masked as usize), Animation::new(FADE).with_easing(gpui::ease_out_quint()), move |d, k| d.opacity(if masked { 0.55 + 0.45 * k } else { 1. - 0.45 * k }))
        .into_any_element()
}

/// A row's × (remove), shown while the row is hovered.
/// Add or Hide with nothing typed: drawn, not clickable.
fn off_button(t: &Theme, id: &'static str, label: &'static str) -> AnyElement {
    div().children(ui::here(&id.into())).flex_none().h(px(30.)).px(px(14.)).flex().items_center().border_1().border_color(t.line2).text_size(px(crate::theme::text::BODY)).text_color(t.tx4).child(label).into_any_element()
}

/// A row's ×, as a tab's: there only while the row is hovered (the row lights up), brighter
/// under the pointer.
fn remove_x(t: &Theme, id: SharedString, row: SharedString) -> gpui::Stateful<gpui::Div> {
    let (tx3, tx) = (t.tx3, t.tx);
    div()
        .children(ui::here(&id.clone().into()))
        .id(id)
        .flex_none()
        .size(px(16.))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(crate::theme::text::TITLE))
        .cursor_pointer()
        .hover(move |s| s.text_color(tx))
        .child("×")
        .text_color(gpui::transparent_black())
        .group_hover(row, move |s| s.text_color(tx3))
}

/// A muted line under a control: a state worth knowing (production, agents refused…).
pub(crate) fn state_note(t: &Theme, text: impl Into<SharedString>) -> gpui::Div {
    div().text_size(px(crate::theme::text::SMALL)).text_color(t.tx3).child(text.into())
}

/// The page's segmented control: one choice, told by its wash.
pub(crate) fn choices<T: Copy + PartialEq + 'static>(t: &Theme, id: &str, opts: &[(T, &str)], on: T, cx: &mut Context<AgentsPage>, pick: impl Fn(&mut AgentsPage, T, &mut Context<AgentsPage>) + Clone + 'static) -> gpui::Div {
    let segs = opts.iter().enumerate().map(|(k, (v, label))| {
        let (v, pick) = (*v, pick.clone());
        let sel = v == on;
        let hov = t.hov;
        let sid = SharedString::from(format!("{id}-{k}"));
        // As ui::segment draws it.
        div()
            .children(ui::here(&sid.clone().into()))
            .id(sid)
            .relative()
            .h_full()
            .px(px(12.))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(crate::theme::text::UI))
            .border_1()
            .border_color(gpui::transparent_black())
            .when(sel, |d| d.bg(t.pop).border_color(t.line2).text_color(t.tx))
            .when(!sel, move |d| d.text_color(t.tx3).cursor_pointer().hover(move |s| s.bg(hov)))
            .child(label.to_string())
            .on_click(cx.listener(move |p, _, _, cx| {
                pick(p, v, cx);
                cx.notify();
            }))
    });
    div().flex().child(
        div()
            .relative()
            .flex()
            .flex_row()
            .h(px(28.))
            .p(px(2.))
            .bg(t.field)
            .border_1()
            .border_color(t.line2)
            .children(segs),
    )
}

/// How wide a popup button is: on its own, or as wide as its column.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fit {
    Alone,
    Fill,
}

/// A closed popup button: its value and a chevron.
fn select(t: &Theme, page: &AgentsPage, id: SharedString, label: &str, enabled: bool, fit: Fit) -> gpui::Div {
    let min = match fit {
        Fit::Alone => 180.,
        Fit::Fill => 0.,
    };
    let open = page.menu.as_ref().is_some_and(|m| m.id == id);
    div().relative().min_w(px(min)).when(fit == Fit::Fill, |d| d.flex_1()).when(fit != Fit::Fill, |d| d.flex_none()).child(page.anchor(id.clone())).child(crate::agents::select_box(t, id, label, enabled, open))
}

/// How Redacted reads in the page's examples.
pub(crate) const REDACTED: &str = "‹redacted›";

/// What an agent gets for a value of `field`: a made-up one of its kind, masked as `how` says.
pub(crate) fn masked_example(field: &str, how: Mask) -> String {
    let f = field.to_ascii_lowercase();
    let sample = match () {
        _ if f.contains("mail") => "ana@acme.com",
        _ if f.contains("phone") || f.contains("mobile") => "0415 555 4242",
        _ if f.contains("card") => "4111 1111 1111 4242",
        _ if f.contains("ssn") => "123-45-4242",
        _ if f.contains("iban") => "DE89 3704 0044 0532 0130 42",
        _ => "x7Kq2Lm9Rt4242",
    };
    match how {
        // Named as the mask is: the agent gets ‹hidden› (the tip says so).
        Mask::Hidden => REDACTED.into(),
        _ => privacy::mask_value(how, sample),
    }
}

impl AgentsPage {
    pub(crate) fn selected_profile(&mut self, cx: &mut Context<Self>) -> Option<ConnectionProfile> {
        let all = self.profiles(cx);
        if self.conn.as_ref().is_none_or(|id| !all.iter().any(|p| &p.id == id)) {
            self.conn = all.first().map(|p| p.id.clone());
        }
        let id = self.conn.clone()?;
        all.into_iter().find(|p| p.id == id)
    }

    pub(crate) fn render_conn_list(&self, t: &Theme, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.profiles(cx).into_iter().map(|p| {
            let sel = self.conn.as_deref() == Some(p.id.as_str());
            let a = p.mcp_access();
            let hov = t.hov;
            let id = p.id.clone();
            let sid = SharedString::from(format!("agents-conn-{}", p.name));
            let login = p.agent_login.is_some() && dataman_core::agent_login::supported(p.engine);
            div()
                .children(ui::here(&sid.clone().into()))
                .id(sid)
                .h(px(28.))
                .flex_none()
                .pl(px(32.))
                .pr(px(16.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .cursor_pointer()
                .map(|d| ui::selectable_flat(d, t, sel, focused))
                .when(!sel, move |d| d.hover(move |s| s.bg(hov)))
                .children(crate::assets::small_logo(p.engine, 14.))
                .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(p.name.clone()))
                // Agents connect here as a login of their own.
                .when(login, |d| d.child(div().children(ui::here(&SharedString::from(format!("agents-conn-key-{}", p.name)).into())).flex_none().child(icon(Icon::Key).size(px(12.)).text_color(t.ok))))
                .child(div().flex_1())
                .child(div().flex_none().text_size(px(crate::theme::text::SMALL)).text_color(if a == McpAccess::Full { t.warn } else { t.tx3 }).child(a.label()))
                .on_click(cx.listener(move |v, _, window, cx| {
                    v.focus_list(window, cx);
                    v.conn = Some(id.clone());
                    v.allow_error = None;
                    v.yolo_confirm = false;
                    v.scan_note = None;
                    cx.notify();
                }))
        });
        div().id("agents-conns").w(px(CONNS_W)).flex_none().py(px(4.)).flex().flex_col().gap(px(6.)).overflow_y_scroll().track_scroll(&self.conns_scroll).border_r_1().border_color(t.line).children(rows).into_any_element()
    }

    /// One bar over both columns, a single line under it: Protection over the connections, the
    /// selected one's logo, name and place over the page.
    fn protection_bar(&self, t: &Theme, p: Option<&ConnectionProfile>) -> gpui::Div {
        div()
            .h(px(52.))
            .flex_none()
            .flex()
            .flex_row()
            .border_b_1()
            .border_color(t.line)
            .child(div().w(px(CONNS_W)).flex_none().pl(px(32.)).pr(px(16.)).flex().items_center().border_r_1().border_color(t.line).child(crate::agents::page_title(t, "Protection")))
            .children(p.map(|p| {
                div()
                    .children(ui::here(&"agents-protection-conn".into()))
                    .flex_1()
                    .min_w_0()
                    .pl(px(32.))
                    .pr(px(crate::agents::CLOSE_ROOM))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .children(crate::assets::small_logo(p.engine, 15.))
                    .child(div().flex_none().text_size(px(crate::theme::text::TITLE)).font_weight(FontWeight::SEMIBOLD).text_color(t.tx).child(p.name.clone()))
                    .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx3).child(place(p)))
            }))
    }

    fn no_conns(&self, t: &Theme) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.protection_bar(t, None))
            .child(div().flex_1().flex().items_center().justify_center().text_color(t.tx3).child("No saved connections."))
            .into_any_element()
    }

    pub(crate) fn set_rule(&mut self, f: impl FnOnce(&mut privacy::AgentRules), cx: &mut Context<Self>) {
        if let Some(id) = self.conn.clone() {
            self.edit_conn(&id, |p| f(&mut p.mcp_rules), cx);
        }
    }

    /// A popup button whose menu sets one value (an `Act` each).
    #[allow(clippy::too_many_arguments)]
    fn pick(&self, t: &Theme, id: &str, label: &str, enabled: bool, fit: Fit, width: f32, items: Vec<(SharedString, bool, Act)>, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let sid = SharedString::from(id.to_string());
        let el = div().id(SharedString::from(format!("{id}-btn"))).flex().when(fit == Fit::Fill, |d| d.w_full()).child(select(t, self, sid.clone(), label, enabled, fit));
        if enabled { el.cursor_pointer().on_click(cx.listener(move |v, _: &ClickEvent, _, cx| v.open_menu(sid.clone(), width, items.clone(), cx))) } else { el }
    }

    /// Protection: how agents reach the connection, Access, Privacy, and its agent login last.
    pub(crate) fn render_protection(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(p) = self.selected_profile(cx) else { return self.no_conns(t) };
        self.login_sync(&p, cx);
        let card = self.login_card(t, &p, cx);
        let access = self.access_section(t, &p, window, cx);
        let privacy = self.privacy_section(t, &p, window, cx);
        let login = self.login_section(t, &p, cx);
        self.do_jump(card.is_some(), login.is_some(), window);
        let body = div()
            .id("agents-protection-body")
            .flex_1()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&self.protection_scroll)
            .px(px(32.))
            .pt(px(BODY_TOP))
            .pb(px(48.))
            .flex()
            .flex_col()
            .gap(px(44.))
            .children(card)
            .child(access)
            .child(privacy)
            .children(login);
        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .child(self.protection_bar(t, Some(&p)))
            .child(div().flex_1().min_h_0().flex().flex_row().child(self.render_conn_list(t, self.focus.is_focused(window), cx)).child(body))
            .children(self.login_sheet(t, &p, cx))
            .children(self.sens_sheet(t, cx))
            .into_any_element()
    }

    /// Brings the asked-for section to the top once the page is laid out (child bounds are laid
    /// out unscrolled).
    fn do_jump(&mut self, card: bool, login: bool, window: &mut Window) {
        let Some(j) = self.jump else { return };
        let ix = match j {
            Jump::Privacy => 1 + card as usize,
            Jump::Login if login => 2 + card as usize,
            Jump::Login => 0,
        };
        let s = &self.protection_scroll;
        match s.bounds_for_item(ix) {
            Some(b) if s.bounds().size.height > gpui::px(0.) => {
                let y = (s.bounds().top() + px(BODY_TOP) - b.top()).max(-s.max_offset().y).min(gpui::px(0.));
                s.set_offset(gpui::point(gpui::px(0.), y));
                self.jump = None;
            }
            _ => window.on_next_frame(|window, _| window.refresh()),
        }
    }

    // ---- Access ----

    fn access_section(&mut self, t: &Theme, p: &ConnectionProfile, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let r = &p.mcp_rules;
        let tier = p.mcp_access();
        let tiers: Vec<(McpAccess, &str)> = McpAccess::ALL.iter().map(|a| (*a, tier_label(*a))).collect();
        let mut subs = vec![
            sub(sub_title(t, "agents-tier-tip", "MCP", tier_tip()))
                .child(choices(t, "agents-tier", &tiers, tier, cx, |v, a, cx| {
                    if let Some(id) = v.conn.clone() {
                        v.edit_conn(&id, |p| p.mcp = Some(a), cx);
                    }
                }))
                .when(p.env.is_production(), |d| d.child(state_note(t, "Production: every write asks.")))
                .children(self.login_refused_note(t, p)),
        ];
        if p.engine == Engine::MongoDb {
            let cur = r.read_from.unwrap_or(ReadFrom::Primary);
            let items = ReadFrom::ALL.iter().map(|f| (f.label().into(), *f == cur, Act::ReadFrom(*f))).collect();
            subs.push(sub(sub_title(t, "agents-read-from-tip", "Read from", read_from_tip())).child(self.pick(t, "agents-read-from", cur.label(), true, Fit::Alone, 180., items, cx)));
        }
        // A value that came from the old global setting (or a file) shows as a choice of its own,
        // in order before ∞. The value that applies is the one selected; choosing the default
        // stores nothing, so the connection keeps following it.
        let with = |list: &[u64], cur: Option<u64>| -> Vec<u64> {
            let mut v = list.to_vec();
            if let Some(c) = cur.filter(|c| !v.contains(c)) {
                v.push(c);
                v.sort_by_key(|n| privacy::row_cap(*n));
            }
            v
        };
        let time = r.time_limit_s.unwrap_or(privacy::DEFAULT_TIME_LIMIT_S);
        let reads = r.read_rows.unwrap_or(MCP_MAX_ROWS);
        let writes = p.mcp_row_limit.unwrap_or(MCP_WRITE_ROW_LIMIT);
        let time_items = with(&privacy::TIME_LIMITS, r.time_limit_s).into_iter().map(|s| (time_label(s).into(), s == time, Act::TimeLimit(s))).collect();
        let read_items = with(&privacy::READ_ROWS, r.read_rows).into_iter().map(|n| (count_label(n).into(), n == reads, Act::ReadRows(n))).collect();
        let write_items = with(&privacy::WRITE_ROWS, p.mcp_row_limit).into_iter().map(|n| (count_label(n).into(), n == writes, Act::WriteRows(n))).collect();
        // Rows per write means nothing below the Write tier.
        let writing = tier >= McpAccess::Write;
        let limit = |label: &str, on: bool, el: gpui::Stateful<gpui::Div>| div().flex_1().min_w_0().flex().flex_col().gap(px(6.)).child(div().text_color(if on { t.tx2 } else { t.tx3 }).child(label.to_string())).child(el);
        let row = div()
            .flex()
            .flex_row()
            .gap(px(16.))
            .child(limit("Time per call", true, self.pick(t, "agents-time", &time_label(time), true, Fit::Fill, 150., time_items, cx)))
            .child(limit("Rows per read", true, self.pick(t, "agents-rrows", &count_label(reads), true, Fit::Fill, 150., read_items, cx)))
            .child(limit("Rows per write", writing, self.pick(t, "agents-wrows", &count_label(writes), writing, Fit::Fill, 150., write_items, cx)));
        subs.push(sub(sub_title(t, "agents-limits-tip", "Limits", limits_tip())).child(row));
        if dataman_core::MCP_ADVANCED {
            if p.engine.is_sql() {
                subs.push(div().child(self.render_allowed(t, p, window, cx)));
            }
            subs.push(div().child(self.render_yolo(t, p, cx)));
        }
        flat_section(t, "agents-access", "Access", subs)
    }

    fn render_allowed(&self, t: &Theme, p: &ConnectionProfile, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let rows = p.mcp_allowed.iter().enumerate().map(|(k, s)| {
            div()
                .h(px(26.))
                .px(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .border_1()
                .border_color(t.line)
                .text_size(px(crate::theme::text::UI))
                .text_color(t.tx2)
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(s.clone()))
                .child(ui::icon_button(t, ("agents-allowed-rm", k), Icon::Close, 20., 10.).on_click(cx.listener(move |v, _, _, cx| {
                    if let Some(id) = v.conn.clone() {
                        v.edit_conn(&id, |p| {
                            if k < p.mcp_allowed.len() {
                                p.mcp_allowed.remove(k);
                            }
                        }, cx);
                    }
                })))
        });
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(ui::caps(t, "ALLOWED WITHOUT ASKING"))
            .children(rows)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.))
                    .child(field(t, "agents-field-allowed", &self.allow_input, window, cx).flex_1())
                    .child(ui::button(t, "agents-allowed-add", ui::Btn::Secondary).h(px(28.)).child("+ Add").on_click(cx.listener(|v, _, _, cx| v.add_allowed(cx)))),
            )
            .children(self.allow_error.clone().map(|e| div().text_size(px(crate::theme::text::SMALL)).text_color(t.err).child(e)))
            .into_any_element()
    }

    pub(crate) fn add_allowed(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.selected_profile(cx) else { return };
        let text = self.allow_input.read(cx).text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let d = p.engine.dialect();
        let entry = dataman_core::guard::allowlist_entry(&text, d)
            .and_then(|e| if p.mcp_allowed.iter().any(|a| dataman_core::guard::allowlist_match(a, &e, d)) { Err("already allowed".to_string()) } else { Ok(e) });
        match entry {
            Ok(e) => {
                self.edit_conn(&p.id, |p| p.mcp_allowed.push(e), cx);
                self.allow_input.update(cx, |i, cx| i.set_text("", cx));
                self.allow_error = None;
            }
            Err(e) => self.allow_error = Some(e),
        }
        cx.notify();
    }

    fn render_yolo(&self, t: &Theme, p: &ConnectionProfile, cx: &mut Context<Self>) -> AnyElement {
        let on = p.mcp_yolo;
        let confirming = self.yolo_confirm;
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .id("agents-yolo")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.))
                    .child(div().flex_1().flex().flex_col().gap(px(3.)).child(div().text_color(if on { t.err } else { t.tx }).child("YOLO")).child(div().text_size(px(crate::theme::text::SMALL)).text_color(t.tx3).child("Writes run without asking.")))
                    .child(crate::agents::switch(t, "agents-yolo-switch", on))
                    .cursor_pointer()
                    .on_click(cx.listener(|v, _, _, cx| {
                        let Some(p) = v.selected_profile(cx) else { return };
                        // On production YOLO takes a second, explicit yes.
                        if !p.mcp_yolo && p.env.is_production() && !v.yolo_confirm {
                            v.yolo_confirm = true;
                            cx.notify();
                            return;
                        }
                        v.yolo_confirm = false;
                        v.edit_conn(&p.id, |p| p.mcp_yolo = !p.mcp_yolo, cx);
                    })),
            )
            .when(confirming, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(crate::theme::text::SMALL))
                        .text_color(t.err)
                        .child(div().flex_1().child("Production: agents would write here without anyone asking."))
                        .child(ui::button(t, "agents-yolo-yes", ui::Btn::Danger).h(px(24.)).child("Turn on").on_click(cx.listener(|v, _, _, cx| {
                            v.yolo_confirm = false;
                            if let Some(id) = v.conn.clone() {
                                v.edit_conn(&id, |p| p.mcp_yolo = true, cx);
                            }
                        }))),
                )
            })
            .into_any_element()
    }

    // ---- Privacy ----

    /// The rows (by key) that appeared since this connection's page was last drawn: added,
    /// scanned or taken from a suggestion. None on its first showing.
    fn arrivals(&mut self, conn: &str, keys: Vec<String>, reduced: bool) -> HashSet<String> {
        let now = Instant::now();
        if self.rows_seen.0 != conn {
            self.rows_new.clear();
        } else if !reduced {
            for k in keys.iter().filter(|k| !self.rows_seen.1.contains(*k)) {
                self.rows_new.insert(k.clone(), now);
            }
        }
        self.rows_seen = (conn.to_string(), keys.into_iter().collect());
        self.rows_new.retain(|_, at| now.duration_since(*at) < GROW * 3);
        self.rows_new.keys().cloned().collect()
    }

    /// Removes a row once it has shrunk away (at once under reduced motion). Rows go by name,
    /// so others added or removed meanwhile don't shift which one goes.
    fn leave(&mut self, key: String, cx: &mut Context<Self>, remove: impl FnOnce(&mut privacy::AgentRules) + 'static) {
        if cx.reduce_motion() {
            return self.set_rule(remove, cx);
        }
        let Some(id) = self.conn.clone() else { return };
        if !self.rows_leaving.insert(key.clone()) {
            return;
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(GROW).await;
            let _ = this.update(cx, |v, cx| {
                v.rows_leaving.remove(&key);
                v.edit_conn(&id, |p| remove(&mut p.mcp_rules), cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// When what agents see last changed on this connection (not on coming to it).
    fn view_changed(&mut self, conn: &str, view: AgentView) -> Option<Instant> {
        match &mut self.view_seen {
            Some((c, v, at)) if c == conn => {
                if *v != view {
                    (*v, *at) = (view, Some(Instant::now()));
                }
                *at
            }
            _ => {
                self.view_seen = Some((conn.to_string(), view, None));
                None
            }
        }
    }

    fn privacy_section(&mut self, t: &Theme, p: &ConnectionProfile, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let r = p.mcp_rules.clone();
        let names = Names::of(p.engine);
        let (one, _) = nouns(p.engine);
        let place = placeholder(p.engine);
        if self.vis_input.read(cx).text().is_empty() {
            self.vis_input.update(cx, |i, cx| i.set_placeholder(place, cx));
        }
        let sens_place = format!("{one}.{} or *pattern*", part(p.engine));
        self.sens_input.update(cx, |i, cx| i.set_placeholder(sens_place.clone(), cx));
        let views: Vec<(AgentView, &str)> = AgentView::ALL.iter().map(|v| (*v, v.label())).collect();
        let sees = sub(sub_title(t, "agents-sees-tip", "Agents see", sees_tip())).child(choices(t, "agents-view", &views, r.view, cx, |v, view, cx| v.set_rule(|r| r.view = view, cx)));

        // Sensitive columns mask in Masked data only: elsewhere the table is shown, dimmed and
        // inert (no focus, click or key), and Use Masked data is the way back to editing.
        let masked = r.view == AgentView::Masked;
        // By table (or collection), in the order each first appears; a pattern without a table
        // goes under "Any table".
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for (k, s) in r.sensitive.iter().enumerate() {
            let g = s.col.rsplit_once('.').map(|(g, _)| g.to_string()).unwrap_or_default();
            match groups.iter_mut().find(|(n, _)| *n == g) {
                Some((_, ks)) => ks.push(k),
                None => groups.push((g, vec![k])),
            }
        }
        let reduced = cx.reduce_motion();
        let fading = !reduced && self.view_changed(&p.id, r.view).is_some_and(|at| at.elapsed() < FADE * 2);
        let mut keys: Vec<String> = groups.iter().map(|(g, _)| format!("g:{g}")).collect();
        keys.extend(r.sensitive.iter().map(|s| format!("s:{}", s.col)));
        keys.extend(r.visible.items.iter().map(|i| format!("v:{}", i.name)));
        let new = self.arrivals(&p.id, keys, reduced);
        let mut rows: Vec<AnyElement> = Vec::new();
        for (g, ks) in groups {
            let label = if g.is_empty() { format!("Any {one}") } else { g.clone() };
            let key = format!("g:{g}");
            // The header goes with its last column.
            let mut going = HashSet::new();
            if ks.iter().all(|k| self.rows_leaving.contains(&format!("s:{}", r.sensitive[*k].col))) {
                going.insert(key.clone());
            }
            rows.push(motion(
                div()
                    .id(SharedString::from(format!("agents-sens-group-{g}")))
                    .h(px(34.))
                    .px(px(12.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(9.))
                    .bg(t.alt)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(icon(Icon::Table).size(px(14.)).text_color(t.tx3))
                    .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(label)),
                34.,
                &key,
                &new,
                &going,
            ));
            for k in ks {
                let s = &r.sensitive[k];
                let name = s.col.rsplit_once('.').map_or(s.col.as_str(), |(_, f)| f);
                let id = SharedString::from(format!("agents-sens-edit-{k}"));
                // An older version's Off still reads Off, but isn't offered.
                let how = if s.on { crate::agents_sens::summary(s.how) } else { "Off".into() };
                let rid = SharedString::from(format!("agents-sens-{k}"));
                let hov = t.hov;
                let key = format!("s:{}", s.col);
                rows.push(motion(
                    div()
                        .id(rid.clone())
                        .group(rid.clone())
                        .h(px(36.))
                        .hover(move |s| s.bg(hov))
                        .pl(px(34.))
                        .pr(px(10.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(9.))
                        .child(icon(Icon::Column).size(px(13.)).text_color(t.tx3))
                        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(if s.on { t.tx } else { t.tx3 }).child(name.to_string()))
                        .child(div().w(px(180.)).flex_none().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(crate::theme::text::BODY)).text_color(if s.on { t.tx2 } else { t.tx3 }).child(how))
                        .child({
                            let edit = ui::button(t, id.clone(), ui::Btn::Secondary).h(px(26.)).px(px(10.)).text_size(px(crate::theme::text::BODY)).child("Edit");
                            if masked {
                                let (col, how) = (s.col.clone(), s.how);
                                edit.on_click(cx.listener(move |v, _, _, cx| v.open_sens_edit(col.clone(), how, cx))).into_any_element()
                            } else {
                                edit.opacity(0.4).cursor_default().into_any_element()
                            }
                        })
                        .when(masked, |d| {
                            let (key, col) = (key.clone(), s.col.clone());
                            d.child(remove_x(t, SharedString::from(format!("agents-sens-rm-{k}")), rid.clone()).on_click(cx.listener(move |v, _, _, cx| {
                                let col = col.clone();
                                v.leave(key.clone(), cx, move |r| r.sensitive.retain(|s| s.col != col))
                            })))
                        })
                        .when(!masked, |d| d.child(div().w(px(16.)).flex_none())),
                    36.,
                    &key,
                    &new,
                    &self.rows_leaving,
                ));
            }
        }
        let scan = div()
            .children(ui::here(&"agents-scan".into()))
            .id("agents-scan")
            .ml_auto()
            .font_weight(FontWeight::NORMAL)
            .text_size(px(crate::theme::text::BODY))
            .text_color(if masked { t.tx2 } else { t.tx4 })
            .when(masked, |d| d.cursor_pointer().hover(|s| s.opacity(0.75)).on_click(cx.listener(|v, _, _, cx| v.scan(cx))))
            .child("Scan again");
        let title = if p.engine.is_sql() { "Sensitive columns" } else { "Sensitive fields" };
        let sens_empty = self.sens_input.read(cx).text().trim().is_empty();
        let add = div()
            .flex()
            .flex_row()
            .gap(px(10.))
            .when(masked, |d| {
                d.child(self.suggest_field(t, Which::Sensitive, field(t, "agents-field-sensitive", &self.sens_input, window, cx).h(px(30.)), window, cx))
                    .child(if sens_empty { off_button(t, "agents-sens-add", "Add") } else { ui::button(t, "agents-sens-add", ui::Btn::Secondary).h(px(30.)).px(px(14.)).child("Add").on_click(cx.listener(|v, _, _, cx| v.add_sensitive(cx))).into_any_element() })
            })
            // No input at all while disabled: nothing to focus, click, type into or Tab to.
            .when(!masked, |d| {
                d.child(div().children(ui::here(&"agents-field-sensitive".into())).flex_1().h(px(30.)).px(px(9.)).flex().items_center().bg(t.field).border_1().border_color(t.field_b).text_color(t.tx4).child(sens_place))
                    .child(off_button(t, "agents-sens-add", "Add"))
            });
        let add = dim(add, "agents-sens-add-dim", masked, fading);
        let sensitive = sub(sub_title(t, "agents-sens-tip", title, sens_tip()).child(scan))
            // Kept and editable in every mode, but they only mask in Masked data: say so.
            .when(!masked, |d| {
                let note = div()
                        .children(ui::here(&"agents-sens-unused".into()))
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(6.))
                        .text_size(px(crate::theme::text::UI))
                        .text_color(t.tx2)
                        .child("Used only when Agents see is Masked data.")
                        .child(
                            div()
                                .children(ui::here(&"agents-sens-use-masked".into()))
                                .id("agents-sens-use-masked")
                                .text_color(t.acc_t)
                                .cursor_pointer()
                                .hover(|s| s.opacity(0.75))
                                .child("Use Masked data")
                                .on_click(cx.listener(|v, _, _, cx| v.set_rule(|r| r.view = AgentView::Masked, cx))),
                        );
                d.child(if fading { note.with_animation("agents-sens-unused-in", Animation::new(FADE).with_easing(gpui::ease_out_quint()), |d, k| d.opacity(k)).into_any_element() } else { note.into_any_element() })
            })
            .children(self.scan_note.clone().map(|n| state_note(t, n)))
            .when(!rows.is_empty(), |d| d.child(dim(table(t, rows), "agents-sens-dim", masked, fading)))
            .child(add);

        // Visibility: the mode, then its entries, struck through while they are what's hidden.
        let except = r.visible.mode == VisibleMode::Except;
        let typed = self.vis_input.read(cx).text().trim().to_string();
        let kind = Visible::kind_of(&typed, &self.schemas_of(p, cx), names);
        let verb = if r.visible.mode == VisibleMode::Only && kind != ItemKind::Column { "Show" } else { "Hide" };
        let modes = [VisibleMode::Except, VisibleMode::Only].iter().map(|m| (m.label().into(), *m == r.visible.mode, Act::VisMode(*m))).collect();
        let items: Vec<AnyElement> = r
            .visible
            .items
            .iter()
            .enumerate()
            .map(|(k, it)| {
                let glyph = match (it.kind == ItemKind::Column, except) {
                    (true, true) => Icon::ColumnOff,
                    (true, false) => Icon::Column,
                    (false, true) => Icon::TableOff,
                    (false, false) => Icon::Table,
                };
                let rid = SharedString::from(format!("agents-vis-{k}"));
                let key = format!("v:{}", it.name);
                let hov = t.hov;
                let row = div()
                    .id(rid.clone())
                    .group(rid.clone())
                    .h(px(36.))
                    .hover(move |s| s.bg(hov))
                    .pl(px(12.))
                    .pr(px(10.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(9.))
                    .child(icon(glyph).size(px(if it.kind == ItemKind::Column { 13. } else { 14. })).text_color(t.tx3))
                    .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(it.name.clone()))
                    .child(remove_x(t, SharedString::from(format!("agents-vis-rm-{k}")), rid).on_click({
                        let (key, name) = (key.clone(), it.name.clone());
                        cx.listener(move |v, _, _, cx| {
                            let name = name.clone();
                            v.leave(key.clone(), cx, move |r| r.visible.items.retain(|i| i.name != name))
                        })
                    }));
                motion(row, 36., &key, &new, &self.rows_leaving)
            })
            .collect();
        let visible = sub(sub_title(t, "agents-vis-tip", &format!("{} visibility", capitalized(one)), vis_tip(p.engine)))
            .child(self.pick(t, "agents-vis-mode", r.visible.mode.label(), true, Fit::Alone, 180., modes, cx))
            .when(!items.is_empty(), |d| d.child(table(t, items)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(10.))
                    .child(self.suggest_field(t, Which::Visible, field(t, "agents-field-visible", &self.vis_input, window, cx).h(px(30.)), window, cx))
                    .child(if typed.is_empty() { off_button(t, "agents-vis-add", verb) } else { ui::button(t, "agents-vis-add", ui::Btn::Secondary).h(px(30.)).px(px(14.)).child(verb).on_click(cx.listener(|v, _, _, cx| v.add_visible(cx))).into_any_element() }),
            );
        flat_section(t, "agents-privacy", "Privacy", vec![sees, visible, sensitive])
    }

    /// The schemas (or MongoDB databases) known for a connection: from the schema cache when it
    /// is connected, else its own database.
    fn schemas_of(&self, p: &ConnectionProfile, cx: &mut Context<Self>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if let Some(c) = self.store.read(cx).connection(&p.id) {
            let db = c.database().to_string();
            out.extend(c.schema.cached_schemas(&db).unwrap_or_default());
            out.extend(c.schema.cached_databases().unwrap_or_default());
        }
        if !p.database.is_empty() {
            out.push(p.database.clone());
        }
        // The default schemas count even before the explorer has read them.
        out.extend(p.engine.default_schema().map(str::to_string));
        out
    }

    pub(crate) fn add_sensitive(&mut self, cx: &mut Context<Self>) {
        if self.selected_profile(cx).is_none_or(|p| p.mcp_rules.view != AgentView::Masked) {
            return;
        }
        let text = self.sens_input.read(cx).text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let how = privacy::sensitive_name(text.rsplit('.').next().unwrap_or(&text).trim_matches('*')).unwrap_or(Mask::Hidden);
        self.set_rule(|r| {
            if !r.sensitive.iter().any(|s| s.col.eq_ignore_ascii_case(&text)) {
                r.sensitive.push(SensitiveCol { col: text, how, on: true });
            }
        }, cx);
        self.sens_input.update(cx, |i, cx| i.set_text("", cx));
    }

    pub(crate) fn add_visible(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.selected_profile(cx) else { return };
        let text = self.vis_input.read(cx).text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let kind = Visible::kind_of(&text, &self.schemas_of(&p, cx), Names::of(p.engine));
        self.set_rule(|r| {
            if !r.visible.items.iter().any(|i| i.name.eq_ignore_ascii_case(&text)) {
                r.visible.items.push(VisibleItem { name: text, kind });
            }
        }, cx);
        self.vis_input.update(cx, |i, cx| i.set_text("", cx));
    }

    /// Suggests sensitive columns by name from the schema of the connected database (on your
    /// connection, never an agent's).
    pub(crate) fn scan(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.selected_profile(cx).filter(|p| p.mcp_rules.view == AgentView::Masked) else { return };
        if p.engine == Engine::Redis {
            self.scan_note = Some("Redis keys have no fields to scan; add them by name here.".into());
            cx.notify();
            return;
        }
        let Some(conn) = self.store.read(cx).connection(&p.id) else {
            self.scan_note = Some(format!("Connect to {} to scan it.", p.name));
            cx.notify();
            return;
        };
        self.scan_note = Some("Scanning…".into());
        cx.notify();
        let (id, unit) = (p.id.clone(), if p.engine.is_sql() { "column" } else { "field" });
        cx.spawn(async move |this, cx| {
            let db = conn.database().to_string();
            let mut found: Vec<(String, Mask)> = Vec::new();
            if conn.engine().is_sql() {
                let _ = conn.preload(&db).await;
                let snap = conn.schema.snapshot(&db);
                for (rel, cols) in &snap.columns {
                    let Some(r) = snap.relations.get(*rel) else { continue };
                    if r.kind.is_view() {
                        continue;
                    }
                    for c in cols {
                        if let Some(m) = privacy::sensitive_name(&c.name) {
                            found.push((format!("{}.{}", r.name, c.name), m));
                        }
                    }
                }
            } else {
                // Documents have no schema: the fields of a sample of each collection (or index),
                // a nested one by the last part of its path.
                let parent = (conn.engine() == Engine::MongoDb).then_some(db.as_str());
                let colls = conn.nosql_tree(parent).await.unwrap_or_default();
                for c in colls.iter().filter(|c| c.kind == TreeKind::Collection && !c.name.starts_with('.') && !c.name.starts_with("system.")).take(SCAN_MAX) {
                    for (path, _) in conn.sample_fields(&db, &c.name).await.unwrap_or_default() {
                        if let Some(m) = privacy::sensitive_name(path.rsplit('.').next().unwrap_or(&path)) {
                            found.push((format!("{}.{path}", c.name), m));
                        }
                    }
                }
            }
            found.sort();
            let _ = this.update(cx, |v, cx| {
                let mut added = 0;
                v.edit_conn(&id, |p| {
                    for (col, how) in found.iter() {
                        if !p.mcp_rules.sensitive.iter().any(|s| s.col.eq_ignore_ascii_case(col)) {
                            p.mcp_rules.sensitive.push(SensitiveCol { col: col.clone(), how: *how, on: true });
                            added += 1;
                        }
                    }
                }, cx);
                v.scan_note = Some(match (found.len(), added) {
                    (0, _) => format!("No {unit} looks sensitive by its name."),
                    (_, 0) => "Nothing new found.".into(),
                    (_, 1) => format!("1 {unit} added."),
                    (_, n) => format!("{n} {unit}s added."),
                });
                cx.notify();
            });
        })
        .detach();
    }
}
