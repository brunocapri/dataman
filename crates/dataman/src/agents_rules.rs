//! Agents › Protection: one list of connections and, for the selected one, one page: how agents
//! reach it (or its agent login, last), Access (what agents may do there, and how much) and
//! Privacy (what they may see). Everything writes the connection's `mcp_rules`; no MCP tool can.

use dataman_core::privacy::{self, AgentView, ItemKind, Mask, Names, ReadFrom, SensitiveCol, Visible, VisibleItem, VisibleMode};
use dataman_core::settings::{MCP_MAX_ROWS, MCP_WRITE_ROW_LIMIT};
use dataman_core::{ConnectionProfile, Engine, McpAccess};
use gpui::{AnyElement, ClickEvent, Context, FontWeight, SharedString, Window, div, prelude::*};

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
        .item("Masked data", "Values a Mask recognizes (cards, e-mails, keys…) are masked in everything the agent receives, and the sensitive columns below are masked whole. Host, port and user aren't shown.")
        .item("Structure only", "Tables, columns, types and query plans. No rows are read or written; only the structure changes the MCP tier allows. Host, port and user aren't shown.")
        .note("In every mode, what Visibility hides stays hidden and errors don't quote values.")
}

pub(crate) fn sens_tip() -> Tip {
    Tip::new("Sensitive columns")
        .lead("Masked data only: this connection's columns masked whole. The Masks also mask matching values in every column.")
        .item("Hidden", "The value becomes ‹hidden›.")
        .item("Partial", "Keeps the end: a•••@acme.com, •••• 4242.")
        .item("Hash", "A stable code, ‹h:7Q2KX9RM4A›. The same value always gives the same code, so an agent can still match rows.")
        .item("Off", "Kept in the list, not masked.")
        .lead("Agents can select these columns as they are and filter on them in WHERE; they can't rename, group, sort or compute with them.")
        .example("users.email · *ssn* (any column with ssn in its name)")
        .note("Scan again suggests columns by their names. Check what it adds.")
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
    match e {
        Engine::MongoDb => ("collection", "collections"),
        Engine::Elasticsearch | Engine::OpenSearch => ("index", "indices"),
        Engine::Redis => ("key", "keys"),
        _ => ("table", "tables"),
    }
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

fn placeholder(e: Engine) -> &'static str {
    match e {
        Engine::MongoDb => "collection, collection.field or *",
        Engine::Elasticsearch | Engine::OpenSearch => "index, index.field or *",
        Engine::Redis => "key pattern or key.field",
        _ => "table, table.column or *",
    }
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
        .max_w(px(BODY_W))
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(div().text_size(px(15.)).font_weight(FontWeight::BOLD).text_color(t.tx).child(title.to_string()))
        .child(div().border_1().border_color(t.line).rounded(px(3.)).flex().flex_col().children(subs.into_iter().enumerate().map(move |(k, s)| s.px(px(20.)).py(px(18.)).when(k > 0, |d| d.border_t_1().border_color(faint)))))
}

/// A subsection: its title and `?`, the control below.
pub(crate) fn sub(head: impl IntoElement) -> gpui::Div {
    div().flex().flex_col().gap(px(10.)).child(head)
}

pub(crate) fn sub_title(t: &Theme, tip_id: &str, title: &str, tip: Tip) -> gpui::Div {
    div().flex().flex_row().items_center().gap(px(7.)).text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).text_color(t.tx).child(title.to_string()).child(qmark(t, tip_id, tip).font_weight(FontWeight::NORMAL))
}

/// A bordered table: rows divided by faint lines.
fn table(t: &Theme, rows: Vec<gpui::Stateful<gpui::Div>>) -> gpui::Div {
    let faint = t.grid_line;
    div().border_1().border_color(t.line).rounded(px(2.)).flex().flex_col().children(rows.into_iter().enumerate().map(move |(k, r)| r.when(k > 0, |d| d.border_t_1().border_color(faint))))
}

/// A row's × (remove), shown while the row is hovered.
fn remove_x(t: &Theme, id: SharedString, row: SharedString) -> gpui::Stateful<gpui::Div> {
    ui::icon_button(t, id, Icon::Close, 20., 9.).rounded(px(2.)).bg(t.hov).invisible().group_hover(row, |s| s.visible())
}

/// A muted line under a control: a state worth knowing (production, agents refused…).
pub(crate) fn state_note(t: &Theme, text: impl Into<SharedString>) -> gpui::Div {
    div().text_size(px(11.)).text_color(t.tx3).child(text.into())
}

/// The page's segmented control: one choice, told by its wash.
fn choices<T: Copy + PartialEq + 'static>(t: &Theme, id: &str, opts: &[(T, &str)], on: T, cx: &mut Context<AgentsPage>, pick: impl Fn(&mut AgentsPage, T, &mut Context<AgentsPage>) + Clone + 'static) -> gpui::Div {
    let segs = opts.iter().enumerate().map(|(k, (v, label))| {
        let (v, pick) = (*v, pick.clone());
        let sel = v == on;
        let (hov, tx) = (t.hov, t.tx);
        let sid = SharedString::from(format!("{id}-{k}"));
        div()
            .children(ui::here(&sid.clone().into()))
            .id(sid)
            .h(px(24.))
            .px(px(14.))
            .flex()
            .items_center()
            .rounded(px(1.))
            .when(sel, |d| d.bg(t.prs).text_color(t.tx).font_weight(FontWeight::MEDIUM))
            .when(!sel, move |d| d.text_color(t.tx2).cursor_pointer().hover(move |s| s.bg(hov).text_color(tx)))
            .child(label.to_string())
            .on_click(cx.listener(move |p, _, _, cx| {
                pick(p, v, cx);
                cx.notify();
            }))
    });
    div().flex().child(div().flex().flex_row().gap(px(2.)).p(px(2.)).border_1().border_color(t.line2).rounded(px(2.)).children(segs))
}

/// How wide a popup button is: on its own, in a row of a table, or as wide as its column.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fit {
    Alone,
    Row,
    Fill,
}

/// A closed popup button: its value and a chevron.
fn select(t: &Theme, page: &AgentsPage, id: SharedString, label: &str, enabled: bool, fit: Fit) -> gpui::Div {
    let hov = t.hov;
    let small = fit == Fit::Row;
    let min = match fit {
        Fit::Alone => 180.,
        Fit::Row => 110.,
        Fit::Fill => 0.,
    };
    div().relative().min_w(px(min)).when(fit == Fit::Fill, |d| d.flex_1()).when(fit != Fit::Fill, |d| d.flex_none()).child(page.anchor(id.clone())).child(
        div()
            .children(ui::here(&id.into()))
            .h(px(if small { 26. } else { 28. }))
            .px(px(if small { 8. } else { 10. }))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .border_1()
            .rounded(px(2.))
            .when(enabled, |d| d.bg(t.field).border_color(t.line2).text_color(t.tx).hover(move |s| s.bg(hov)))
            .when(!enabled, |d| d.bg(t.hov).border_color(t.line).text_color(t.tx3))
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(label.to_string()))
            .child(icon(Icon::ChevronDown).size(px(10.)).text_color(if enabled { t.tx2 } else { t.tx4 })),
    )
}

/// What an agent gets for a value of `field`: a made-up one of its kind, masked as `how` says.
fn masked_example(field: &str, how: Mask, key: &privacy::HashKey) -> String {
    let f = field.to_ascii_lowercase();
    let sample = match () {
        _ if f.contains("mail") => "ana@acme.com",
        _ if f.contains("phone") || f.contains("mobile") => "0415 555 4242",
        _ if f.contains("card") => "4111 1111 1111 4242",
        _ if f.contains("ssn") => "123-45-4242",
        _ if f.contains("iban") => "DE89 3704 0044 0532 0130 42",
        _ => "x7Kq2Lm9Rt4242",
    };
    privacy::mask_value(how, sample, key)
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

    pub(crate) fn render_conn_list(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
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
                .h(px(34.))
                .flex_none()
                .pl(px(32.))
                .pr(px(16.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .cursor_pointer()
                .when(sel, |d| d.bg(t.prs))
                .when(!sel, move |d| d.hover(move |s| s.bg(hov)))
                .children(crate::assets::small_logo(p.engine, 14.))
                .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(p.name.clone()))
                // Agents connect here as a login of their own.
                .when(login, |d| d.child(div().children(ui::here(&SharedString::from(format!("agents-conn-key-{}", p.name)).into())).flex_none().child(icon(Icon::Key).size(px(12.)).text_color(t.ok))))
                .child(div().flex_1())
                .child(div().flex_none().text_size(px(11.)).text_color(if a == McpAccess::Full { t.warn } else { t.tx3 }).child(a.label()))
                .on_click(cx.listener(move |v, _, window, cx| {
                    v.focus_list(window, cx);
                    v.conn = Some(id.clone());
                    v.allow_error = None;
                    v.yolo_confirm = false;
                    v.scan_note = None;
                    cx.notify();
                }))
        });
        div().id("agents-conns").w(px(CONNS_W)).flex_none().py(px(4.)).flex().flex_col().overflow_y_scroll().track_scroll(&self.conns_scroll).border_r_1().border_color(t.line).children(rows).into_any_element()
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
            .child(div().w(px(CONNS_W)).flex_none().pl(px(32.)).pr(px(16.)).flex().items_center().border_r_1().border_color(t.line).text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(t.tx).child("Protection"))
            .children(p.map(|p| {
                div()
                    .children(ui::here(&"agents-protection-conn".into()))
                    .flex_1()
                    .min_w_0()
                    .px(px(32.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .children(crate::assets::small_logo(p.engine, 15.))
                    .child(div().flex_none().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(t.tx).child(p.name.clone()))
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
            .child(div().flex_1().min_h_0().flex().flex_row().child(self.render_conn_list(t, cx)).child(body))
            .children(self.login_sheet(t, &p, cx))
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
        section(t, "agents-access", "Access", subs)
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
                .text_size(px(11.5))
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
            .children(self.allow_error.clone().map(|e| div().text_size(px(11.)).text_color(t.err).child(e)))
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
                    .child(div().flex_1().flex().flex_col().gap(px(3.)).child(div().text_color(if on { t.err } else { t.tx }).child("YOLO")).child(div().text_size(px(11.)).text_color(t.tx3).child("Writes run without asking.")))
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
                        .text_size(px(11.))
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

    fn privacy_section(&mut self, t: &Theme, p: &ConnectionProfile, window: &mut Window, cx: &mut Context<Self>) -> gpui::Div {
        let r = p.mcp_rules.clone();
        let names = Names::of(p.engine);
        let (one, _) = nouns(p.engine);
        let place = placeholder(p.engine);
        if self.vis_input.read(cx).text().is_empty() {
            self.vis_input.update(cx, |i, cx| i.set_placeholder(place, cx));
        }
        let sens_place = if p.engine.is_sql() { "table.column or *pattern*" } else { "collection.field or *pattern*" };
        self.sens_input.update(cx, |i, cx| i.set_placeholder(sens_place, cx));
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
        let mut rows: Vec<gpui::Stateful<gpui::Div>> = Vec::new();
        for (g, ks) in groups {
            let label = if g.is_empty() { format!("Any {one}") } else { g.clone() };
            rows.push(
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
            );
            for k in ks {
                let s = &r.sensitive[k];
                let name = s.col.rsplit_once('.').map_or(s.col.as_str(), |(_, f)| f);
                let id = SharedString::from(format!("agents-sens-mask-{k}"));
                let mut items: Vec<(SharedString, bool, Act)> = Mask::ALL.iter().map(|m| (m.label().into(), s.on && *m == s.how, Act::SensMask(k, *m))).collect();
                items.push(("Off".into(), !s.on, Act::SensOff(k)));
                let example = if s.on { masked_example(name, s.how, &self.key) } else { "as stored".into() };
                let rid = SharedString::from(format!("agents-sens-{k}"));
                rows.push(
                    div()
                        .id(rid.clone())
                        .group(rid.clone())
                        .h(px(36.))
                        .pl(px(34.))
                        .pr(px(10.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(9.))
                        .child(icon(Icon::Column).size(px(13.)).text_color(t.tx3))
                        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(if s.on { t.tx } else { t.tx3 }).child(name.to_string()))
                        .child(div().w(px(130.)).flex_none().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.)).text_color(t.tx3).child(example))
                        .child(self.pick(t, &id, if s.on { s.how.label() } else { "Off" }, masked, Fit::Row, 120., items, cx))
                        .when(masked, |d| {
                            d.child(remove_x(t, SharedString::from(format!("agents-sens-rm-{k}")), rid.clone()).on_click(cx.listener(move |v, _, _, cx| {
                                v.set_rule(|r| {
                                    if k < r.sensitive.len() {
                                        r.sensitive.remove(k);
                                    }
                                }, cx)
                            })))
                        })
                        .when(!masked, |d| d.child(div().w(px(20.)).flex_none())),
                );
            }
        }
        let scan = div()
            .children(ui::here(&"agents-scan".into()))
            .id("agents-scan")
            .ml_auto()
            .font_weight(FontWeight::NORMAL)
            .text_size(px(12.))
            .text_color(if masked { t.tx2 } else { t.tx4 })
            .when(masked, |d| d.cursor_pointer().hover(|s| s.opacity(0.75)).on_click(cx.listener(|v, _, _, cx| v.scan(cx))))
            .child("Scan again");
        let title = if p.engine.is_sql() { "Sensitive columns" } else { "Sensitive fields" };
        let add = div()
            .flex()
            .flex_row()
            .gap(px(10.))
            .when(!masked, |d| d.opacity(0.55))
            .when(masked, |d| {
                d.child(self.suggest_field(t, Which::Sensitive, field(t, "agents-field-sensitive", &self.sens_input, window, cx).h(px(30.)), window, cx))
                    .child(ui::button(t, "agents-sens-add", ui::Btn::Secondary).h(px(30.)).px(px(14.)).child("Add").on_click(cx.listener(|v, _, _, cx| v.add_sensitive(cx))))
            })
            // No input at all while disabled: nothing to focus, click, type into or Tab to.
            .when(!masked, |d| {
                d.child(div().children(ui::here(&"agents-field-sensitive".into())).flex_1().h(px(30.)).px(px(9.)).flex().items_center().bg(t.field).border_1().border_color(t.field_b).text_color(t.tx4).child(sens_place))
                    .child(div().children(ui::here(&"agents-sens-add".into())).flex_none().h(px(30.)).px(px(14.)).flex().items_center().border_1().border_color(t.line2).text_color(t.tx4).child("Add"))
            });
        let sensitive = sub(sub_title(t, "agents-sens-tip", title, sens_tip()).child(scan))
            // Kept and editable in every mode, but they only mask in Masked data: say so.
            .when(!masked, |d| {
                d.child(
                    div()
                        .children(ui::here(&"agents-sens-unused".into()))
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(6.))
                        .text_size(px(11.5))
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
                        ),
                )
            })
            .children(self.scan_note.clone().map(|n| state_note(t, n)))
            .when(!rows.is_empty(), |d| d.child(table(t, rows).when(!masked, |d| d.opacity(0.55))))
            .child(add);

        // Visibility: the mode, then its entries, struck through while they are what's hidden.
        let except = r.visible.mode == VisibleMode::Except;
        let typed = self.vis_input.read(cx).text().trim().to_string();
        let kind = Visible::kind_of(&typed, &self.schemas_of(p, cx), names);
        let verb = if r.visible.mode == VisibleMode::Only && kind != ItemKind::Column { "Show" } else { "Hide" };
        let modes = [VisibleMode::Except, VisibleMode::Only].iter().map(|m| (m.label().into(), *m == r.visible.mode, Act::VisMode(*m))).collect();
        let items: Vec<gpui::Stateful<gpui::Div>> = r
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
                div()
                    .id(rid.clone())
                    .group(rid.clone())
                    .h(px(36.))
                    .pl(px(12.))
                    .pr(px(10.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(9.))
                    .child(icon(glyph).size(px(if it.kind == ItemKind::Column { 13. } else { 14. })).text_color(t.tx3))
                    .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(it.name.clone()))
                    .child(remove_x(t, SharedString::from(format!("agents-vis-rm-{k}")), rid).on_click(cx.listener(move |v, _, _, cx| {
                        v.set_rule(|r| {
                            if k < r.visible.items.len() {
                                r.visible.items.remove(k);
                            }
                        }, cx)
                    })))
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
                    .child(ui::button(t, "agents-vis-add", ui::Btn::Secondary).h(px(30.)).px(px(14.)).child(verb).on_click(cx.listener(|v, _, _, cx| v.add_visible(cx)))),
            );
        section(t, "agents-privacy", "Privacy", vec![sees, sensitive, visible])
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
        match p.engine {
            Engine::Postgres => out.push("public".into()),
            Engine::SqlServer => out.push("dbo".into()),
            Engine::Sqlite | Engine::DuckDb => out.push("main".into()),
            _ => {}
        }
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
        if !p.engine.is_sql() {
            self.scan_note = Some("Scan reads SQL schemas; add fields by name here.".into());
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
        let id = p.id.clone();
        cx.spawn(async move |this, cx| {
            let db = conn.database().to_string();
            let _ = conn.preload(&db).await;
            let snap = conn.schema.snapshot(&db);
            let mut found: Vec<(String, Mask)> = Vec::new();
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
                    (0, _) => "No column looks sensitive by its name.".into(),
                    (_, 0) => "Nothing new found.".into(),
                    (_, 1) => "1 column added.".into(),
                    (_, n) => format!("{n} columns added."),
                });
                cx.notify();
            });
        })
        .detach();
    }
}
