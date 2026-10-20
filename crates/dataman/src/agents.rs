//! The Agents area, opened from the robot at the foot of the rail: who used Dataman through MCP
//! and what they ran (Home, Audit), and per connection what agents may do (Permissions) and see
//! (Privacy), with the sensitive data types masked in everything an agent receives.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use dataman_core::privacy::{HashKey, Mask, Validator};
use dataman_core::{ConnectionProfile, Core, Engine};
use dataman_mcp::audit::{AllowedBy, Decision, Entry, Outcome};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, MouseButton, Pixels, Point,
    SharedString, StyledText, Window, div, point, prelude::*,
};

use crate::assets::{Icon, icon};
use crate::conns::ConnStore;
use crate::input::{InputEvent, TextInput};
use crate::theme::{ActiveTheme, Theme, mono, px};
use crate::ui;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Home,
    Audit,
    Permissions,
    Privacy,
    Types,
}

impl Page {
    pub const ALL: [Page; 5] = [Page::Home, Page::Audit, Page::Permissions, Page::Privacy, Page::Types];

    pub fn label(self) -> &'static str {
        match self {
            Page::Home => "Home",
            Page::Audit => "Audit",
            Page::Permissions => "Permissions",
            Page::Privacy => "Privacy",
            Page::Types => "Sensitive data",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Page::Home => "home",
            Page::Audit => "audit",
            Page::Permissions => "permissions",
            Page::Privacy => "privacy",
            Page::Types => "types",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Page::Home => Icon::Home,
            Page::Audit => Icon::Pulse,
            Page::Permissions => Icon::Shield,
            Page::Privacy => Icon::Padlock,
            Page::Types => Icon::Types,
        }
    }
}

pub enum AgentsEvent {
    Open { sql: String, connection: Option<String>, database: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcomes {
    Any,
    Ran,
    Refused,
    Failed,
}

impl Outcomes {
    const ALL: [Outcomes; 4] = [Outcomes::Any, Outcomes::Ran, Outcomes::Refused, Outcomes::Failed];

    fn label(self) -> &'static str {
        match self {
            Outcomes::Any => "Any outcome",
            Outcomes::Ran => "Ran",
            Outcomes::Refused => "Refused or denied",
            Outcomes::Failed => "Errors",
        }
    }

    fn admits(self, e: &Entry) -> bool {
        match self {
            Outcomes::Any => true,
            Outcomes::Ran => e.outcome == Outcome::Ok,
            Outcomes::Refused => e.outcome == Outcome::Refused || denied(e),
            Outcomes::Failed => matches!(e.outcome, Outcome::Error | Outcome::Timeout | Outcome::Invalid) && !denied(e),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    Hour,
    Today,
    Week,
    All,
}

impl When {
    const ALL: [When; 4] = [When::Hour, When::Today, When::Week, When::All];

    fn label(self) -> &'static str {
        match self {
            When::Hour => "Last hour",
            When::Today => "Today",
            When::Week => "Last 7 days",
            When::All => "All time",
        }
    }

    fn admits(self, e: &Entry) -> bool {
        let now = chrono::Utc::now();
        match self {
            When::Hour => now - e.ts < chrono::Duration::hours(1),
            When::Today => e.ts.with_timezone(&Local).date_naive() == Local::now().date_naive(),
            When::Week => now - e.ts < chrono::Duration::days(7),
            When::All => true,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Act {
    Agent(Option<String>),
    Conn(Option<String>),
    Outcome(Outcomes),
    When(When),
    SensMask(usize, Mask),
    TypeMask(String, Mask),
    Scope(Option<String>),
    Validator(Validator),
    CopyQuery(usize),
    OpenTab(usize),
}

pub(crate) struct Menu {
    pub(crate) id: SharedString,
    at: Point<Pixels>,
    width: f32,
    items: Vec<(SharedString, bool, Act)>,
}

pub(crate) struct TypeEditor {
    pub(crate) editing: Option<String>,
    pub(crate) name: Entity<TextInput>,
    pub(crate) regex: Entity<TextInput>,
    pub(crate) mask: Mask,
    pub(crate) more: bool,
    pub(crate) validator: Validator,
    pub(crate) ignore_case: bool,
    pub(crate) keep: u8,
    pub(crate) samples: Vec<String>,
    pub(crate) sample: Entity<TextInput>,
    pub(crate) confirm_delete: bool,
}

pub struct AgentsPage {
    pub(crate) core: Arc<Core>,
    pub(crate) store: Entity<ConnStore>,
    focus: FocusHandle,
    pub(crate) page: Page,
    visible: bool,
    pub(crate) entries: Vec<Entry>,
    loaded: bool,
    pub(crate) search: Entity<TextInput>,
    pub(crate) f_agent: Option<String>,
    pub(crate) f_conn: Option<String>,
    pub(crate) f_outcome: Outcomes,
    pub(crate) f_when: When,
    pub(crate) selected: Option<usize>,
    pub(crate) menu: Option<Menu>,
    pub(crate) anchors: Rc<RefCell<HashMap<SharedString, Bounds<Pixels>>>>,
    row_bounds: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    pub(crate) conn: Option<String>,
    pub(crate) allow_input: Entity<TextInput>,
    pub(crate) allow_error: Option<String>,
    pub(crate) yolo_confirm: bool,
    pub(crate) sens_input: Entity<TextInput>,
    pub(crate) vis_input: Entity<TextInput>,
    pub(crate) scan_note: Option<String>,
    pub(crate) scope: Option<String>,
    pub(crate) editor: TypeEditor,
    pub(crate) name_hint: Option<String>,
    pub(crate) key: HashKey,
}

impl EventEmitter<AgentsEvent> for AgentsPage {}

impl Focusable for AgentsPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Seen in the last five minutes: still at work.
const CONNECTED: chrono::TimeDelta = chrono::TimeDelta::minutes(5);

fn denied(e: &Entry) -> bool {
    e.approval.as_ref().is_some_and(|a| matches!(a.decision, Decision::Denied | Decision::Timeout))
}

fn hidden_refusal(e: &Entry) -> bool {
    e.error.as_deref().is_some_and(|m| m.contains("not visible to agents") || m.contains(" is hidden"))
}

/// The refusal's own words, without the `refused: ` every refusal starts with.
fn refusal_text(e: &Entry) -> String {
    let m = e.error.clone().unwrap_or_default();
    m.strip_prefix("refused: ").map(str::to_string).unwrap_or(m)
}

fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{} {many}", crate::grid::ui_count(n as usize)) }
}

fn is_write_tool(e: &Entry) -> bool {
    matches!(e.tool.as_str(), "execute" | "nosql_execute" | "terminate_backend" | "cancel_backend") || e.approval.is_some() || e.allowed_by.is_some()
}

fn masked_count(e: &Entry) -> u64 {
    e.seen.as_ref().map_or(0, |s| s.masked.len() as u64)
}

/// The one-word outcome of Home's table: nothing for a plain success.
pub(crate) fn outcome_word(e: &Entry) -> (String, bool) {
    if denied(e) {
        return ("denied".into(), true);
    }
    match e.outcome {
        Outcome::Ok => {
            let w = match (&e.approval, &e.allowed_by) {
                (Some(a), _) if a.decision == Decision::Approved => "allowed",
                (_, Some(AllowedBy::Yolo)) => "yolo",
                (_, Some(AllowedBy::Allowlist { .. })) => "allowed query",
                _ if e.draft => "draft",
                _ => "",
            };
            (w.into(), false)
        }
        Outcome::Refused if hidden_refusal(e) => ("refused · hidden column".into(), true),
        Outcome::Refused => ("refused".into(), true),
        Outcome::Cancelled => ("cancelled".into(), false),
        Outcome::Timeout => ("timed out".into(), true),
        Outcome::Invalid => ("invalid".into(), true),
        Outcome::Error => ("error".into(), true),
    }
}

/// The note under an Audit row; true when it reads in the error color.
pub(crate) fn audit_note(e: &Entry) -> (String, bool) {
    if let Some(a) = e.approval.as_ref().filter(|a| matches!(a.decision, Decision::Denied | Decision::Timeout)) {
        let mut parts: Vec<String> = a.reasons.clone();
        parts.push(if a.decision == Decision::Denied { "denied by you".into() } else { "no answer in time".into() });
        return (parts.join(" · "), true);
    }
    match e.outcome {
        Outcome::Ok => {
            let mut parts = Vec::new();
            let doc = e.tool.starts_with("nosql") || e.sql.as_deref().is_some_and(|s| s.starts_with("db.") || s.starts_with("GET ") || s.starts_with("POST "));
            match e.rows {
                _ if e.draft => parts.push("draft, placed in your tab".to_string()),
                Some(n) if is_write_tool(e) => parts.push(plural(n, "row", "rows")),
                Some(n) if doc => parts.push(plural(n, "document", "documents")),
                Some(n) => parts.push(plural(n, "row", "rows")),
                None => {}
            }
            if let Some(c) = e.seen.as_ref().and_then(|s| s.columns) {
                parts.push(plural(c, "column", "columns"));
            }
            let m = masked_count(e);
            if m > 0 {
                parts.push(format!("{m} masked"));
            }
            if let Some(a) = e.approval.as_ref().filter(|a| a.decision == Decision::Approved) {
                if a.reasons.iter().any(|r| r == "production") {
                    parts.push("production write".into());
                }
                parts.push("allowed by you".into());
            }
            match &e.allowed_by {
                Some(AllowedBy::Yolo) => parts.push("ran without asking · YOLO".into()),
                Some(AllowedBy::Allowlist { .. }) => parts.push("an allowed query".into()),
                None => {}
            }
            if parts.is_empty() {
                parts.push("ok".into());
            }
            (parts.join(" · "), false)
        }
        Outcome::Refused => (format!("refused · {}", refusal_text(e)), true),
        Outcome::Cancelled => ("cancelled".into(), false),
        _ => (e.error.clone().unwrap_or_else(|| e.outcome.label().into()), true),
    }
}

pub(crate) fn decision_text(e: &Entry) -> String {
    match e.approval.as_ref().map(|a| a.decision) {
        Some(Decision::Approved) => return "Allowed by you".into(),
        Some(Decision::Denied) => return "Denied by you".into(),
        Some(Decision::Timeout) => return "No answer in time".into(),
        Some(Decision::Unavailable) => return "Refused · the app was closed".into(),
        None => {}
    }
    match e.outcome {
        Outcome::Refused if hidden_refusal(e) => "Refused · hidden".into(),
        Outcome::Refused => {
            let m = e.error.as_deref().unwrap_or("");
            if ["tier", "reads only", "destructive", "read-only", "MCP access"].iter().any(|k| m.contains(k)) { "Refused · tier".into() } else { "Refused".into() }
        }
        Outcome::Ok if e.draft => "Drafted · placed in your tab".into(),
        Outcome::Ok => match &e.allowed_by {
            Some(AllowedBy::Yolo) => "Ran · YOLO, no question".into(),
            Some(AllowedBy::Allowlist { .. }) => "Ran · an allowed query, no question".into(),
            None if is_write_tool(e) => "Ran · write, no question".into(),
            None => "Ran · read, no question".into(),
        },
        Outcome::Cancelled => "Cancelled".into(),
        Outcome::Timeout => "Timed out".into(),
        Outcome::Invalid => "Invalid arguments".into(),
        Outcome::Error => "Failed".into(),
    }
}

pub(crate) fn ago(ts: chrono::DateTime<chrono::Utc>) -> String {
    let d = chrono::Utc::now() - ts;
    if d >= CONNECTED {
        return "idle".into();
    }
    match d.num_seconds().max(0) {
        s if s < 60 => format!("{s} s ago"),
        s => format!("{} min ago", s / 60),
    }
}

pub(crate) struct AgentCard {
    pub(crate) name: String,
    pub(crate) last: chrono::DateTime<chrono::Utc>,
    pub(crate) conns: Vec<String>,
}

pub(crate) fn agents_of(entries: &[Entry]) -> Vec<AgentCard> {
    let mut out: Vec<AgentCard> = Vec::new();
    for e in entries {
        let card = match out.iter_mut().find(|c| c.name == e.client) {
            Some(c) => c,
            None => {
                out.push(AgentCard { name: e.client.clone(), last: e.ts, conns: Vec::new() });
                out.last_mut().unwrap()
            }
        };
        card.last = card.last.max(e.ts);
        if let Some(c) = &e.connection
            && !card.conns.contains(c)
        {
            card.conns.push(c.clone());
        }
    }
    out.sort_by(|a, b| b.last.cmp(&a.last));
    out
}

fn fmt_ms(ms: f64) -> String {
    if ms < 1000. { format!("{ms:.0} ms") } else { format!("{:.1} s", ms / 1000.) }
}

impl AgentsPage {
    pub fn new(core: Arc<Core>, store: Entity<ConnStore>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::new(cx, "Search").bare());
        cx.subscribe(&search, |v: &mut Self, _, ev: &InputEvent, cx| {
            if let InputEvent::Changed = ev {
                v.selected = None;
                cx.notify();
            }
        })
        .detach();
        let allow_input = cx.new(|cx| TextInput::new(cx, "DELETE FROM sessions WHERE expires_at < ?").bare());
        cx.subscribe(&allow_input, |v: &mut Self, _, ev: &InputEvent, cx| match ev {
            InputEvent::Submit => v.add_allowed(cx),
            InputEvent::Changed => {
                v.allow_error = None;
                cx.notify();
            }
        })
        .detach();
        let sens_input = cx.new(|cx| TextInput::new(cx, "table.column or *pattern*").bare());
        cx.subscribe(&sens_input, |v: &mut Self, _, ev: &InputEvent, cx| {
            if let InputEvent::Submit = ev {
                v.add_sensitive(cx);
            }
        })
        .detach();
        let vis_input = cx.new(|cx| TextInput::new(cx, "table, table.column or *").bare());
        cx.subscribe(&vis_input, |v: &mut Self, _, ev: &InputEvent, cx| match ev {
            InputEvent::Submit => v.add_visible(cx),
            InputEvent::Changed => cx.notify(),
        })
        .detach();
        let name = cx.new(|cx| TextInput::new(cx, "Customer code").bare());
        let regex = cx.new(|cx| TextInput::new(cx, "\\bCUST-[0-9]{6}\\b").bare());
        let sample = cx.new(|cx| TextInput::new(cx, "A line to test, then ↩").bare());
        for i in [&name, &regex] {
            cx.subscribe(i, |_: &mut Self, _, _: &InputEvent, cx| cx.notify()).detach();
        }
        cx.subscribe(&sample, |v: &mut Self, s, ev: &InputEvent, cx| {
            if let InputEvent::Submit = ev {
                let text = s.read(cx).text().to_string();
                if !text.trim().is_empty() {
                    v.editor.samples.push(text);
                    s.update(cx, |s, cx| s.set_text("", cx));
                }
                cx.notify();
            }
        })
        .detach();
        cx.observe(&store, |_, _, cx| cx.notify()).detach();
        let key = if cfg!(feature = "checks") && std::env::var_os("DATAMAN_SHOT_STAGE").is_some() {
            HashKey::fixed(b"dataman-checks")
        } else {
            HashKey::stored(core.secrets.clone())
        };
        let conn = store.read(cx).profiles.first().map(|p| p.id.clone());
        let mut v = AgentsPage {
            core,
            store,
            focus: cx.focus_handle(),
            page: Page::Home,
            visible: false,
            entries: Vec::new(),
            loaded: false,
            search,
            f_agent: None,
            f_conn: None,
            f_outcome: Outcomes::Any,
            f_when: When::Today,
            selected: None,
            menu: None,
            anchors: Default::default(),
            row_bounds: Default::default(),
            conn,
            allow_input,
            allow_error: None,
            yolo_confirm: false,
            sens_input,
            vis_input,
            scan_note: None,
            scope: None,
            editor: TypeEditor { editing: None, name, regex, mask: Mask::Hidden, more: false, validator: Validator::None, ignore_case: false, keep: 4, samples: Vec::new(), sample, confirm_delete: false },
            name_hint: None,
            key,
        };
        v.poll(cx);
        v
    }

    /// While the page shows, the log is read again every few seconds (agents keep working).
    fn poll(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let Ok(visible) = this.update(cx, |v, _| v.visible) else { break };
                if visible {
                    let _ = this.update(cx, |v, cx| v.reload(cx));
                }
            }
        })
        .detach();
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let path = self.core.dirs.mcp_audit_file();
        let task = cx.background_spawn(async move { dataman_mcp::audit::tail(&path, 500) });
        cx.spawn(async move |this, cx| {
            let mut entries = task.await;
            entries.reverse();
            let _ = this.update(cx, |v, cx| {
                // The selection follows its entry as new ones arrive on top.
                if !v.entries.is_empty() {
                    let sel = v.selected.and_then(|i| v.entries.get(i)).map(|e| (e.ts, e.tool.clone()));
                    v.selected = sel.and_then(|(ts, tool)| entries.iter().position(|e| e.ts == ts && e.tool == tool));
                }
                v.entries = entries;
                v.loaded = true;
                cx.notify();
            });
        })
        .detach();
    }

    pub fn set_visible(&mut self, on: bool, cx: &mut Context<Self>) {
        if on && !self.visible {
            self.reload(cx);
        }
        self.visible = on;
        if !on {
            self.menu = None;
        }
    }

    pub fn show(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.page != page {
            self.menu = None;
        }
        self.page = page;
        if page == Page::Audit || page == Page::Home {
            self.reload(cx);
        }
        cx.notify();
    }

    pub(crate) fn profiles(&self, cx: &App) -> Vec<ConnectionProfile> {
        self.store.read(cx).profiles.clone()
    }

    pub(crate) fn profile_named(&self, name: &str, cx: &App) -> Option<ConnectionProfile> {
        let s = self.store.read(cx);
        s.profiles.iter().find(|p| p.name == name).or_else(|| s.profile(name)).cloned()
    }

    pub(crate) fn engine_of(&self, name: &str, cx: &App) -> Option<Engine> {
        self.profile_named(name, cx).map(|p| p.engine)
    }

    pub(crate) fn connected_count(&self) -> usize {
        agents_of(&self.entries).iter().filter(|a| chrono::Utc::now() - a.last < CONNECTED).count()
    }

    pub(crate) fn filtered(&self, cx: &App) -> Vec<usize> {
        let q = self.search.read(cx).text().trim().to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.f_agent.as_ref().is_none_or(|a| &e.client == a))
            .filter(|(_, e)| self.f_conn.as_ref().is_none_or(|c| e.connection.as_ref() == Some(c)))
            .filter(|(_, e)| self.f_outcome.admits(e) && self.f_when.admits(e))
            .filter(|(_, e)| {
                q.is_empty()
                    || [e.statement(), e.error.as_deref(), e.connection.as_deref(), Some(e.client.as_str()), Some(e.tool.as_str())]
                        .into_iter()
                        .flatten()
                        .any(|s| s.to_lowercase().contains(&q))
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Edits a connection's saved profile; the MCP server reads the file again on its next call.
    pub(crate) fn edit_conn(&mut self, id: &str, f: impl FnOnce(&mut ConnectionProfile), cx: &mut Context<Self>) {
        if let Some(mut p) = self.core.config.get(id) {
            f(&mut p);
            if let Err(e) = self.core.config.upsert(p) {
                log::warn!("could not save the connection: {e:#}");
            }
            self.store.update(cx, |s, cx| s.reload(cx));
            cx.notify();
        }
    }

    pub(crate) fn edit_settings(&mut self, f: impl FnOnce(&mut dataman_core::settings::Settings), cx: &mut Context<Self>) {
        if let Err(e) = self.core.settings.update(f) {
            log::warn!("could not save the settings: {e:#}");
        }
        cx.notify();
    }

    // ---- Menus ----

    /// An invisible canvas recording where a dropdown's button sits, so its menu opens under it.
    pub(crate) fn anchor(&self, id: impl Into<SharedString>) -> AnyElement {
        let (id, anchors) = (id.into(), self.anchors.clone());
        gpui::canvas(move |b, _, _| anchors.borrow_mut().insert(id, b), |_, _, _, _| {}).absolute().top_0().left_0().size_full().into_any_element()
    }

    pub(crate) fn open_menu(&mut self, id: impl Into<SharedString>, width: f32, items: Vec<(SharedString, bool, Act)>, cx: &mut Context<Self>) {
        let id = id.into();
        if self.menu.as_ref().is_some_and(|m| m.id == id) || ui::menu_just_shut(&id) {
            self.menu = None;
            cx.notify();
            return;
        }
        let at = self.anchors.borrow().get(&id).map(|b| point(b.left(), b.bottom() + gpui::px(4.))).unwrap_or_default();
        self.menu = Some(Menu { id, at, width, items });
        cx.notify();
    }

    fn open_menu_at(&mut self, id: impl Into<SharedString>, at: Point<Pixels>, width: f32, items: Vec<(SharedString, bool, Act)>, cx: &mut Context<Self>) {
        self.menu = Some(Menu { id: id.into(), at, width, items });
        cx.notify();
    }

    fn run(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        match act {
            Act::Agent(a) => {
                self.f_agent = a;
                self.selected = None;
            }
            Act::Conn(c) => {
                self.f_conn = c;
                self.selected = None;
            }
            Act::Outcome(o) => {
                self.f_outcome = o;
                self.selected = None;
            }
            Act::When(w) => {
                self.f_when = w;
                self.selected = None;
            }
            Act::SensMask(i, m) => {
                if let Some(id) = self.conn.clone() {
                    self.edit_conn(&id, |p| {
                        if let Some(s) = p.mcp_rules.sensitive.get_mut(i) {
                            s.how = m;
                        }
                    }, cx);
                }
            }
            Act::TypeMask(id, m) => self.set_type_mask(&id, m, cx),
            Act::Scope(s) => {
                self.scope = s;
                self.name_hint = None;
            }
            Act::Validator(v) => self.editor.validator = v,
            Act::CopyQuery(i) => {
                if let Some(s) = self.entries.get(i).and_then(|e| e.statement()) {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(s.to_string()));
                }
            }
            Act::OpenTab(i) => self.open_entry(i, cx),
        }
        let _ = window;
        cx.notify();
    }

    fn open_entry(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(e) = self.entries.get(i) else { return };
        let Some(sql) = e.statement() else { return };
        cx.emit(AgentsEvent::Open { sql: sql.to_string(), connection: e.connection.clone(), database: e.database.clone() });
    }

    fn render_menu(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let m = self.menu.as_ref()?;
        let id = m.id.clone();
        let rows = m.items.iter().enumerate().map(|(k, (label, on, act))| {
            let act = act.clone();
            ui::menu_item(t, SharedString::from(format!("{}:{k}", m.id)), label.clone(), on.then_some("✓"), true)
                .on_click(cx.listener(move |v, _: &ClickEvent, window, cx| v.run(act.clone(), window, cx)))
                .into_any_element()
        });
        let pop = ui::popover(t)
            .id("agents-menu")
            .w(px(m.width))
            .text_size(px(12.))
            .font_family(mono())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down_out(cx.listener(move |v, _: &gpui::MouseDownEvent, _, cx| {
                ui::menu_shut(id.to_string());
                v.menu = None;
                cx.notify();
            }))
            .children(rows);
        Some(gpui::deferred(gpui::anchored().position(m.at).snap_to_window_with_margin(px(8.)).child(ui::menu_in("agents-menu-in", pop))).priority(3).into_any_element())
    }

    // ---- Shared pieces ----

    pub(crate) fn header(&self, t: &Theme, title: &str) -> gpui::Div {
        div().h(px(56.)).flex_none().px(px(32.)).flex().flex_row().items_center().gap(px(12.)).child(div().text_size(px(17.)).font_weight(FontWeight::BOLD).text_color(t.tx).child(title.to_string()))
    }

    pub(crate) fn conn_label(&self, t: &Theme, name: &str, cx: &App) -> gpui::Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.))
            .min_w_0()
            .children(self.engine_of(name, cx).and_then(|e| crate::assets::small_logo(e, 14.)))
            .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx2).child(name.to_string()))
    }

    // ---- Nav (in the sidebar) ----

    pub fn render_nav(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let n = self.connected_count();
        let rows = Page::ALL.map(|p| {
            let on = self.page == p && self.visible;
            let id = SharedString::from(format!("agents-nav-{}", p.key()));
            let hov = t.hov;
            div()
                .children(ui::here(&id.clone().into()))
                .id(id)
                .h(px(28.))
                .pl(px(14.))
                .pr(px(12.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .text_color(if on { t.tx } else { t.tx2 })
                .when(on, |d| d.bg(t.prs))
                .when(!on, move |d| d.hover(move |s| s.bg(hov)))
                .child(icon(p.icon()).size(px(14.)).text_color(if on { t.tx } else { t.tx3 }))
                .child(div().flex_1().child(p.label()))
                .on_click(cx.listener(move |v, _, _, cx| v.show(p, cx)))
        });
        div()
            .id("agents-nav")
            .size_full()
            .flex()
            .flex_col()
            .text_size(px(12.))
            .child(
                div()
                    .h(px(34.))
                    .flex_none()
                    .pl(px(14.))
                    .pr(px(12.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().text_size(px(11.)).text_color(t.tx2).child("AGENTS"))
                    .child(div().text_size(px(11.)).text_color(t.tx3).child(format!("{n} connected"))),
            )
            .child(div().pt(px(2.)).pb(px(10.)).flex().flex_col().children(rows))
            .into_any_element()
    }

    // ---- Home ----

    fn render_home(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let head = self.header(t, "Agents");
        if self.entries.is_empty() {
            let cmd = crate::mcp::stdio_command();
            let copy = cmd.clone();
            let body = div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(14.))
                .when(self.loaded, |d| {
                    d.child(div().text_size(px(13.)).text_color(t.tx2).child("No agent has connected yet."))
                        .child(
                            div()
                                .max_w(px(720.))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.))
                                .child(div().min_w_0().px(px(10.)).py(px(6.)).bg(t.field).border_1().border_color(t.line).text_size(px(11.5)).text_color(t.tx).overflow_hidden().whitespace_nowrap().text_ellipsis().child(cmd))
                                .child(ui::button(t, "agents-copy-cmd", ui::Btn::Secondary).h(px(28.)).child("Copy").on_click(cx.listener(move |_, _, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()))))),
                        )
                });
            return div().size_full().flex().flex_col().child(head).child(body).into_any_element();
        }
        let cards: Vec<AnyElement> = agents_of(&self.entries)
            .into_iter()
            .take(6)
            .map(|a| {
                let when = ago(a.last);
                let idle = when == "idle";
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .px(px(16.))
                    .py(px(14.))
                    .border_1()
                    .border_color(t.line)
                    .when(idle, |d| d.opacity(0.55))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(div().text_size(px(13.)).font_weight(FontWeight::BOLD).text_color(t.tx).child(a.name.clone()))
                            .child(div().flex_1())
                            .child(div().text_size(px(11.)).text_color(t.tx3).child(when)),
                    )
                    .child(div().flex().flex_row().flex_wrap().gap_x(px(14.)).gap_y(px(6.)).children(a.conns.iter().map(|c| self.conn_label(t, c, cx))))
                    .into_any_element()
            })
            .collect();
        let mut grid = div().flex().flex_col().gap(px(12.));
        let mut it = cards.into_iter().peekable();
        while it.peek().is_some() {
            let mut row: Vec<AnyElement> = it.by_ref().take(3).collect();
            while row.len() < 3 {
                row.push(div().flex_1().into_any_element());
            }
            grid = grid.child(div().flex().flex_row().gap(px(12.)).children(row));
        }
        let col = |w: f32| div().flex_none().w(px(w));
        let header = div()
            .h(px(28.))
            .flex_none()
            .px(px(10.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.))
            .border_b_1()
            .border_color(t.line2)
            .text_size(px(11.))
            .text_color(t.tx3)
            .child(col(62.).child("TIME"))
            .child(col(96.).child("AGENT"))
            .child(col(150.).child("CONNECTION"))
            .child(div().flex_1().child("STATEMENT"))
            .child(col(56.).flex().justify_end().child("ROWS"))
            .child(col(56.).flex().justify_end().child("TOOK"))
            .child(col(170.).child("OUTCOME"));
        let rows = self.entries.iter().enumerate().filter(|(_, e)| e.statement().is_some()).take(40).map(|(i, e)| {
            let (word, bad) = outcome_word(e);
            let hov = t.hov;
            let id = SharedString::from(format!("agents-home-row-{i}"));
            div()
                .children(ui::here(&id.clone().into()))
                .id(id)
                .h(px(30.))
                .flex_none()
                .px(px(10.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(14.))
                .border_b_1()
                .border_color(t.line)
                .hover(move |s| s.bg(hov))
                .child(col(62.).text_color(t.tx3).child(e.ts.with_timezone(&Local).format("%H:%M:%S").to_string()))
                .child(col(96.).text_color(t.tx).overflow_hidden().whitespace_nowrap().text_ellipsis().child(e.client.clone()))
                .child(col(150.).child(e.connection.as_deref().map(|c| self.conn_label(t, c, cx).into_any_element()).unwrap_or_else(|| div().into_any_element())))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx2).child(one_line(e.statement().unwrap_or(""))))
                .child(col(56.).flex().justify_end().text_color(t.tx2).child(e.rows.map(|n| crate::grid::ui_count(n as usize)).unwrap_or_default()))
                .child(col(56.).flex().justify_end().text_color(t.tx3).child(if e.outcome == Outcome::Ok { fmt_ms(e.ms) } else { String::new() }))
                .child(col(170.).text_size(px(11.5)).text_color(if bad { t.err } else { t.tx3 }).overflow_hidden().whitespace_nowrap().text_ellipsis().child(word))
                .on_click(cx.listener(move |v, _, _, cx| {
                    v.f_agent = None;
                    v.f_conn = None;
                    v.f_outcome = Outcomes::Any;
                    v.f_when = When::All;
                    v.search.update(cx, |s, cx| s.set_text("", cx));
                    v.selected = Some(i);
                    v.show(Page::Audit, cx);
                }))
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(head)
            .child(
                div()
                    .id("agents-home-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(28.))
                    .pt(px(8.))
                    .px(px(32.))
                    .pb(px(24.))
                    .child(grid)
                    .child(div().flex().flex_col().child(header).children(rows)),
            )
            .into_any_element()
    }

    // ---- Audit ----

    fn filter_button(&self, t: &Theme, id: &'static str, label: String, cx: &mut Context<Self>) -> AnyElement {
        let items: Vec<(SharedString, bool, Act)> = match id {
            "agents-f-agent" => {
                let mut v = vec![("All agents".into(), self.f_agent.is_none(), Act::Agent(None))];
                v.extend(agents_of(&self.entries).into_iter().map(|a| (a.name.clone().into(), self.f_agent.as_deref() == Some(a.name.as_str()), Act::Agent(Some(a.name)))));
                v
            }
            "agents-f-conn" => {
                let names: BTreeSet<String> = self.entries.iter().filter_map(|e| e.connection.clone()).collect();
                let mut v = vec![("All connections".into(), self.f_conn.is_none(), Act::Conn(None))];
                v.extend(names.into_iter().map(|n| (n.clone().into(), self.f_conn.as_deref() == Some(n.as_str()), Act::Conn(Some(n)))));
                v
            }
            "agents-f-outcome" => Outcomes::ALL.iter().map(|o| (o.label().into(), *o == self.f_outcome, Act::Outcome(*o))).collect(),
            _ => When::ALL.iter().map(|w| (w.label().into(), *w == self.f_when, Act::When(*w))).collect(),
        };
        let hov = t.hov;
        div()
            .relative()
            .child(self.anchor(id))
            .child(
                div()
                    .children(ui::here(&id.into()))
                    .id(id)
                    .h(px(28.))
                    .px(px(10.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .border_1()
                    .border_color(t.line2)
                    .text_color(t.tx)
                    .hover(move |s| s.bg(hov))
                    .child(label)
                    .child(icon(Icon::ChevronDown).size(px(10.)).text_color(t.tx3))
                    .on_click(cx.listener(move |v, _, _, cx| v.open_menu(id, 200., items.clone(), cx))),
            )
            .into_any_element()
    }

    fn render_audit(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let shown = self.filtered(cx);
        if self.selected.is_none_or(|s| !shown.contains(&s)) {
            self.selected = shown.first().copied();
        }
        let search_focused = self.search.read(cx).focus_handle(cx).is_focused(window);
        let toolbar = div()
            .h(px(44.))
            .flex_none()
            .px(px(32.))
            .pb(px(12.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .children(ui::here(&"agents-field-search".into()))
                    .w(px(260.))
                    .h(px(28.))
                    .px(px(9.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .bg(t.field)
                    .border_1()
                    .border_color(if search_focused { t.acc } else { t.field_b })
                    .child(icon(Icon::Search).size(px(13.)).text_color(t.tx3))
                    .child(div().flex_1().min_w_0().child(self.search.clone())),
            )
            .child(self.filter_button(t, "agents-f-agent", self.f_agent.clone().unwrap_or_else(|| "All agents".into()), cx))
            .child(self.filter_button(t, "agents-f-conn", self.f_conn.clone().unwrap_or_else(|| "All connections".into()), cx))
            .child(self.filter_button(t, "agents-f-outcome", self.f_outcome.label().into(), cx))
            .child(self.filter_button(t, "agents-f-when", self.f_when.label().into(), cx));
        let bounds = self.row_bounds.clone();
        bounds.borrow_mut().clear();
        let rows = shown.iter().map(|&i| {
            let e = &self.entries[i];
            let (note, bad) = audit_note(e);
            let sel = self.selected == Some(i);
            let hov = t.hov;
            let b = bounds.clone();
            let id = SharedString::from(format!("agents-audit-row-{i}"));
            div()
                .relative()
                .child(gpui::canvas(move |r, _, _| { b.borrow_mut().insert(i, r); }, |_, _, _, _| {}).absolute().top_0().left_0().size_full())
                .child(
                    div()
                        .children(ui::here(&id.clone().into()))
                        .id(id)
                        .flex()
                        .flex_row()
                        .gap(px(12.))
                        .pl(px(32.))
                        .pr(px(24.))
                        .py(px(10.))
                        .border_b_1()
                        .border_color(t.line)
                        .when(sel, |d| d.bg(t.prs))
                        .when(!sel, move |d| d.hover(move |s| s.bg(hov)))
                        .child(div().w(px(58.)).flex_none().text_color(t.tx3).child(e.ts.with_timezone(&Local).format("%H:%M:%S").to_string()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(4.))
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(10.))
                                        .child(div().text_color(t.tx).child(e.client.clone()))
                                        .children(e.connection.as_deref().map(|c| self.conn_label(t, c, cx))),
                                )
                                .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx2).child(one_line(e.statement().unwrap_or(e.tool.as_str()))))
                                .child(div().text_size(px(11.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(if bad { t.err } else { t.tx3 }).child(note)),
                        )
                        .on_click(cx.listener(move |v, ev: &ClickEvent, _, cx| {
                            v.selected = Some(i);
                            v.menu = None;
                            if ev.click_count() >= 2 {
                                v.open_entry(i, cx);
                            }
                            cx.notify();
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |v, ev: &gpui::MouseDownEvent, _, cx| {
                                v.selected = Some(i);
                                let items = vec![("Copy Query".into(), false, Act::CopyQuery(i)), ("Open in Tab".into(), false, Act::OpenTab(i))];
                                v.open_menu_at("agents-row-menu", ev.position, 180., items, cx);
                                cx.stop_propagation();
                            }),
                        ),
                )
        });
        let list: AnyElement = if shown.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(t.tx3)
                .child(if self.entries.is_empty() { "No calls yet." } else { "Nothing matches." })
                .into_any_element()
        } else {
            div().id("agents-audit-list").flex_1().min_w_0().overflow_y_scroll().flex().flex_col().children(rows).into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.header(t, "Audit"))
            .child(toolbar)
            .child(div().flex_1().min_h_0().flex().flex_row().border_t_1().border_color(t.line).child(list).children(self.render_detail(t, cx)))
            .into_any_element()
    }

    fn render_detail(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let i = self.selected?;
        let e = self.entries.get(i)?;
        let block = |text: &str| {
            div()
                .px(px(12.))
                .py(px(10.))
                .bg(t.field)
                .border_1()
                .border_color(t.line)
                .flex()
                .flex_col()
                .text_size(px(12.))
                .line_height(px(19.))
                .children(crate::editor::highlighted_lines(t, text).into_iter().take(14).map(|(s, runs)| {
                    if s.is_empty() { div().h(px(19.)).into_any_element() } else { div().whitespace_nowrap().overflow_hidden().child(StyledText::new(s).with_runs(runs)).into_any_element() }
                }))
        };
        let seen = e.seen.clone().unwrap_or_default();
        let sent = {
            let mut parts = Vec::new();
            if let Some(n) = e.rows {
                parts.push(plural(n, "row", "rows"));
            }
            if let Some(c) = seen.columns {
                parts.push(plural(c, "column", "columns"));
            }
            if parts.is_empty() { "—".to_string() } else { parts.join(" · ") }
        };
        let masked = if seen.masked.is_empty() { "—".to_string() } else { seen.masked.iter().map(|(c, m)| format!("{c} ({})", m.key())).collect::<Vec<_>>().join(" · ") };
        let values = if seen.values.is_empty() { "—".to_string() } else { seen.values.iter().map(|(k, n)| format!("{n} {}", k.to_lowercase())).collect::<Vec<_>>().join(" · ") };
        let fact = |k: &str, v: String| [div().text_color(t.tx3).child(k.to_string()).into_any_element(), div().min_w_0().text_color(t.tx).child(v).into_any_element()];
        let facts: Vec<AnyElement> = [
            fact("Agent", e.client.clone()),
            fact("Connection", [e.connection.clone(), e.database.clone().filter(|d| !d.is_empty())].into_iter().flatten().collect::<Vec<_>>().join(" · ")),
            fact("Decision", decision_text(e)),
            fact("Sent", sent),
            fact("Masked", masked),
            fact("In values", values),
        ]
        .into_iter()
        .flatten()
        .collect();
        let mut grid = div().flex().flex_col().gap(px(8.));
        let mut it = facts.into_iter();
        while let (Some(k), Some(v)) = (it.next(), it.next()) {
            grid = grid.child(div().flex().flex_row().gap(px(12.)).child(div().w(px(96.)).flex_none().child(k)).child(div().flex_1().min_w_0().child(v)));
        }
        let error = (e.outcome != Outcome::Ok).then(|| e.error.clone()).flatten();
        Some(
            div()
                .w(px(420.))
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(18.))
                .px(px(24.))
                .py(px(20.))
                .border_l_1()
                .border_color(t.line)
                .overflow_hidden()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.))
                        .text_color(t.tx3)
                        .child(div().text_color(t.tx).font_weight(FontWeight::SEMIBOLD).child(e.tool.clone()))
                        .child(div().flex_1())
                        .child(format!("{} · {}", e.ts.with_timezone(&Local).format("%H:%M:%S"), fmt_ms(e.ms))),
                )
                .children(e.statement().map(&block))
                .children(e.rewritten.as_deref().map(|r| div().flex().flex_col().gap(px(6.)).child(ui::caps(t, "REWRITTEN")).child(block(r))))
                .child(grid)
                .children(error.map(|m| div().text_size(px(11.5)).line_height(px(17.)).text_color(t.err).child(m)))
                .child(div().flex_1())
                .when(e.statement().is_some(), |d| {
                    d.child(
                        div().flex().child(
                            ui::button(t, "agents-open-tab", ui::Btn::Secondary)
                                .h(px(28.))
                                .px(px(10.))
                                .child("Open in a tab")
                                .on_click(cx.listener(move |v, _, _, cx| v.open_entry(i, cx))),
                        ),
                    )
                })
                .into_any_element(),
        )
    }

    // ---- Check hooks: the same ones the old log panel had ----

    #[cfg(feature = "checks")]
    pub(crate) fn demo_entries(&self) -> &[Entry] {
        &self.entries
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_row_point(&self, i: usize) -> Option<Point<Pixels>> {
        self.row_bounds.borrow().get(&i).map(|b| point(b.left() + gpui::px(120.), b.top() + gpui::px(12.)))
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_menu_items(&self) -> Vec<(&'static str, bool)> {
        match &self.menu {
            Some(m) if m.id.as_ref() == "agents-row-menu" => vec![("Copy Query", true), ("Open in Tab", true)],
            _ => Vec::new(),
        }
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_menu_open(&self) -> bool {
        self.menu.is_some()
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_menu_labels(&self) -> Vec<String> {
        self.menu.as_ref().map(|m| m.items.iter().map(|(l, _, _)| l.to_string()).collect()).unwrap_or_default()
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_open_menu(&mut self, i: usize, cx: &mut Context<Self>) {
        let at = self.demo_row_point(i).unwrap_or_default();
        let items = vec![("Copy Query".into(), false, Act::CopyQuery(i)), ("Open in Tab".into(), false, Act::OpenTab(i))];
        self.open_menu_at("agents-row-menu", at, 180., items, cx);
    }
}

pub(crate) fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A label with a `?` whose hover shows a short explanation.
pub(crate) fn label_tip(t: &Theme, id: &str, label: &str, rule: &str, example: Option<&str>) -> gpui::Div {
    div().flex().flex_row().items_center().gap(px(6.)).child(ui::caps(t, label.to_string())).child(qmark(t, id, rule, example, false))
}

pub(crate) fn qmark(t: &Theme, id: &str, rule: &str, example: Option<&str>, code: bool) -> gpui::Stateful<gpui::Div> {
    let (rule, example) = (SharedString::from(rule.to_string()), example.map(|e| SharedString::from(e.to_string())));
    div()
        .id(SharedString::from(id.to_string()))
        .flex_none()
        .size(px(15.))
        .rounded(px(7.5))
        .flex()
        .items_center()
        .justify_center()
        .border_1()
        .border_color(t.line2)
        .text_size(px(10.))
        .text_color(t.tx3)
        .child("?")
        .tooltip(move |_, cx| {
            let (rule, example) = (rule.clone(), example.clone());
            cx.new(|_| RuleTip { rule, example, code }).into()
        })
}

pub(crate) struct RuleTip {
    rule: SharedString,
    example: Option<SharedString>,
    code: bool,
}

impl Render for RuleTip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .w(px(340.))
            .px(px(12.))
            .py(px(10.))
            .bg(t.pop)
            .shadow(t.ring())
            .font_family(mono())
            .text_size(px(11.5))
            .line_height(px(17.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(div().text_color(t.tx).when(self.code, |d| d.font_family(crate::editor::mono().family)).child(self.rule.clone()))
            .children(self.example.clone().map(|e| div().text_color(t.tx3).child(e)))
    }
}

/// A 28 pt field around a bare input: the accent outline while it has the focus (no underline).
pub(crate) fn field(t: &Theme, id: &'static str, input: &Entity<TextInput>, window: &Window, cx: &App) -> gpui::Div {
    let focused = input.read(cx).focus_handle(cx).is_focused(window);
    let error = input.read(cx).error;
    div()
        .children(ui::here(&id.into()))
        .h(px(28.))
        .px(px(9.))
        .flex()
        .flex_row()
        .items_center()
        .bg(t.field)
        .border_1()
        .border_color(if focused {
            t.acc
        } else if error {
            t.err
        } else {
            t.field_b
        })
        .child(div().flex_1().min_w_0().child(input.clone()))
}

/// The page's own segmented control: compact segments as wide as their words.
pub(crate) fn seg<T: Copy + PartialEq + 'static>(
    t: &Theme,
    id: &str,
    opts: &[(T, &str)],
    on: T,
    small: bool,
    cx: &mut Context<AgentsPage>,
    pick: impl Fn(&mut AgentsPage, T, &mut Context<AgentsPage>) + Clone + 'static,
) -> gpui::Div {
    let segs = opts.iter().enumerate().map(|(k, (v, label))| {
        let (v, pick) = (*v, pick.clone());
        let sel = v == on;
        let hov = t.hov;
        let sid = SharedString::from(format!("{id}-{k}"));
        div()
            .children(ui::here(&sid.clone().into()))
            .id(sid)
            .h(px(if small { 20. } else { 22. }))
            .px(px(if small { 7. } else { 10. }))
            .flex()
            .items_center()
            .text_size(px(if small { 11.5 } else { 12. }))
            .when(sel, |d| d.bg(t.prs).text_color(t.tx).border_1().border_color(t.line2))
            .when(!sel, move |d| d.text_color(t.tx2).hover(move |s| s.bg(hov)))
            .child(label.to_string())
            .on_click(cx.listener(move |p, _, _, cx| {
                pick(p, v, cx);
                cx.notify();
            }))
    });
    div().flex().child(div().flex().flex_row().p(px(2.)).border_1().border_color(t.line2).bg(t.field).children(segs))
}

/// The mask dropdown (96 pt): a closed popup button.
pub(crate) fn dropdown(t: &Theme, page: &AgentsPage, id: SharedString, label: &str, enabled: bool) -> gpui::Div {
    div().relative().w(px(96.)).flex_none().child(page.anchor(id.clone())).child(
        div()
            .children(ui::here(&id.clone().into()))
            .h(px(24.))
            .px(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .border_1()
            .border_color(t.line2)
            .text_color(if enabled { t.tx } else { t.tx3 })
            .child(div().flex_1().child(label.to_string()))
            .child(icon(Icon::ChevronDown).size(px(9.)).text_color(t.tx3)),
    )
}

pub(crate) fn switch(t: &Theme, on: bool) -> gpui::Div {
    div()
        .flex_none()
        .w(px(28.))
        .h(px(16.))
        .p(px(2.))
        .flex()
        .flex_row()
        .when(on, |d| d.bg(t.tx2).justify_end())
        .when(!on, |d| d.border_1().border_color(t.field_b))
        .child(div().size(px(10.)).bg(if on { t.pop } else { t.tx3 }))
}

pub(crate) fn checkbox(t: &Theme, on: bool, disabled: bool) -> gpui::Div {
    if disabled {
        return div().flex_none().size(px(14.)).border_1().border_color(t.line2).opacity(0.5);
    }
    ui::checkbox(t, on)
}

impl Render for AgentsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let body = match self.page {
            Page::Home => self.render_home(&t, cx),
            Page::Audit => self.render_audit(&t, window, cx),
            Page::Permissions => self.render_permissions(&t, window, cx),
            Page::Privacy => self.render_privacy(&t, window, cx),
            Page::Types => self.render_types(&t, window, cx),
        };
        div()
            .id("agents-page")
            .key_context("AgentsPage")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .font_family(mono())
            .text_size(px(12.))
            .text_color(t.tx)
            .child(body)
            .children(self.render_menu(&t, cx))
    }
}
