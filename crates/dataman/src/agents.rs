//! The Agents area, opened from the robot at the foot of the rail: who used Dataman through MCP
//! and what they ran (MCP, Audit), and per connection what agents may do (Permissions) and see
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
use crate::ui::Tipped;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Audit,
    Permissions,
    Privacy,
    Types,
    Mcp,
}

impl Page {
    pub const ALL: [Page; 5] = [Page::Mcp, Page::Audit, Page::Permissions, Page::Privacy, Page::Types];

    pub fn label(self) -> &'static str {
        match self {
            Page::Audit => "Audit",
            Page::Permissions => "Permissions",
            Page::Privacy => "Privacy",
            Page::Types => "Sensitive data",
            Page::Mcp => "MCP",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Page::Audit => "audit",
            Page::Permissions => "permissions",
            Page::Privacy => "privacy",
            Page::Types => "types",
            Page::Mcp => "mcp",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Page::Audit => Icon::Pulse,
            Page::Permissions => Icon::Shield,
            Page::Privacy => Icon::Padlock,
            Page::Types => Icon::Types,
            Page::Mcp => Icon::Plug,
        }
    }
}

pub enum AgentsEvent {
    Open { sql: String, connection: Option<String>, database: Option<String> },
    /// A cell of what the agent received, for the value viewer (read-only).
    ViewValue(crate::value_view::ValueCell),
}

gpui::actions!(agents_list, [ListUp, ListDown, ListPageUp, ListPageDown, ListFirst, ListLast, ListOpen]);

/// The page's list keys, as the explorer and history have them; not in a field or the reply's grid.
const LIST: &str = "AgentsPage && !TextInput && !ResultGrid";
const PAGE_STEP: isize = 10;

pub fn bind_keys(cx: &mut App) {
    let c = Some(LIST);
    cx.bind_keys([
        crate::keymap::kb("up", ListUp, c),
        crate::keymap::kb("down", ListDown, c),
        crate::keymap::kb("pageup", ListPageUp, c),
        crate::keymap::kb("pagedown", ListPageDown, c),
        crate::keymap::kb("home", ListFirst, c),
        crate::keymap::kb("end", ListLast, c),
        crate::keymap::kb("cmd-up", ListFirst, c),
        crate::keymap::kb("cmd-down", ListLast, c),
        crate::keymap::kb("enter", ListOpen, c),
    ]);
}

/// A call's reply, as the log has it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Kept {
    Reading,
    Reply { text: Arc<str>, cut: bool },
    RotatedOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    By(isize),
    First,
    Last,
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
    /// Privacy's row in the nav shows Sensitive data under it.
    pub(crate) privacy_open: bool,
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
    pub(crate) editor: TypeEditor,
    pub(crate) key: HashKey,
    pub(crate) suggest: crate::agents_suggest::Suggest,
    /// What the selected call's agent received, in the results grid; zoomed over the whole
    /// Audit page, growing from its place in the detail as a result grows over the editor.
    pub(crate) received: Entity<crate::grid::ResultGrid>,
    received_for: Option<Option<String>>,
    pub(crate) received_rows: Option<(usize, u64)>,
    pub(crate) received_zoom: bool,
    pub(crate) received_tween: Option<crate::motion::ZoomTween>,
    /// Where the reply sits in the detail, and the page below the header it zooms over.
    received_slot: Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    received_area: Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    /// Audit's detail width as the seam left it; shown within `detail_shown`'s bounds.
    pub(crate) detail_w: f32,
    pub(crate) detail_drag: bool,
    audit_body: Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    pub(crate) audit_scroll: gpui::ScrollHandle,
    pub(crate) conns_scroll: gpui::ScrollHandle,
    pub(crate) types_scroll: gpui::ScrollHandle,
    /// Where each call's row sits among the Audit list's children (day headers come between).
    audit_child: HashMap<usize, usize>,
    pub(crate) types_offset: usize,
    pub(crate) type_sel: Option<usize>,
    /// Replies read back from the log (`mcp-replies.jsonl`), by call id.
    pub(crate) replies: HashMap<String, Kept>,
    /// Sampled fields of collections and indices, for the suggestions (connection id and name).
    pub(crate) fields: HashMap<String, Vec<(String, String)>>,
    pub(crate) loading: BTreeSet<String>,
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

// Refusals quote names the agent wrote (`"x is hidden from agents" is not a table…`): a refusal
// is told by its own fixed ending, after any name.

fn hidden_column_refusal(e: &Entry) -> bool {
    let conn = e.connection.as_deref().unwrap_or_default();
    e.outcome == Outcome::Refused && e.error.as_deref().is_some_and(|m| m.ends_with(&format!(" is hidden from agents on {conn}")))
}

fn hidden_refusal(e: &Entry) -> bool {
    let conn = e.connection.as_deref().unwrap_or_default();
    hidden_column_refusal(e) || (e.outcome == Outcome::Refused && e.error.as_deref().is_some_and(|m| m.ends_with(&format!(" is not visible to agents on {conn}"))))
}

const MASKED_ENDINGS: &[&str] = &[
    "select it as it is, or compare it as it is in a filter; it cannot be renamed, copied, grouped, sorted or computed",
    "select it as it is or filter on it; it cannot be renamed, grouped, sorted or computed",
    "find and filter on it; it cannot be grouped, sorted or computed",
    "search and filter on it; it cannot be aggregated or sorted",
];

fn masked_refusal(e: &Entry) -> bool {
    let conn = e.connection.as_deref().unwrap_or_default();
    let marker = format!(" is masked for agents on {conn}: ");
    e.outcome == Outcome::Refused && e.error.as_deref().and_then(|m| m.rfind(&marker).map(|at| &m[at + marker.len()..])).is_some_and(|rest| MASKED_ENDINGS.contains(&rest))
}

/// Refused by what agents may do on the connection (Permissions), not by its tier.
fn permission_refusal(e: &Entry) -> bool {
    let conn = e.connection.as_deref().unwrap_or_default();
    e.outcome == Outcome::Refused
        && e.error.as_deref().is_some_and(|m| {
            m.ends_with("(Dataman › Agents › Permissions)")
                || m.ends_with(" never runs for agents")
                || m.ends_with(&format!("is off for agents on {conn} while some data is kept from agents"))
                || m.contains(&format!("refused: {conn} shows agents its structure only"))
        })
}

/// What the agent wrote or named, as it may be shown: invisible and bidi characters made
/// visible, runs of spaces and lines folded, and at most `max` characters.
pub(crate) fn shown_agent_text(s: &str, max: usize) -> String {
    let s = match s.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s.to_string(),
    };
    dataman_mcp::shown_text(&s)
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
        Outcome::Refused => (format!("refused · {}", one_line(&shown_agent_text(&refusal_text(e), 400))), true),
        Outcome::Cancelled => ("cancelled".into(), false),
        _ => (e.error.as_deref().map_or_else(|| e.outcome.label().into(), |m| one_line(&shown_agent_text(m, 400))), true),
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
        Outcome::Refused if masked_refusal(e) => "Refused · masked".into(),
        Outcome::Refused if permission_refusal(e) => "Refused · permissions".into(),
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

pub(crate) struct AgentCard {
    pub(crate) name: String,
    pub(crate) last: chrono::DateTime<chrono::Utc>,
}

pub(crate) fn agents_of(entries: &[Entry]) -> Vec<AgentCard> {
    let mut out: Vec<AgentCard> = Vec::new();
    for e in entries {
        let card = match out.iter_mut().find(|c| c.name == e.client) {
            Some(c) => c,
            None => {
                out.push(AgentCard { name: e.client.clone(), last: e.ts });
                out.last_mut().unwrap()
            }
        };
        card.last = card.last.max(e.ts);
    }
    out.sort_by(|a, b| b.last.cmp(&a.last));
    out
}

/// The day a call belongs to, as Audit groups them.
pub(crate) fn day_label(ts: chrono::DateTime<chrono::Utc>) -> String {
    let d = ts.with_timezone(&Local).date_naive();
    match (Local::now().date_naive() - d).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => d.format("%A, %b %-d").to_string(),
        _ => d.format("%a, %b %-d, %Y").to_string(),
    }
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
        cx.subscribe(&sens_input, |v: &mut Self, _, ev: &InputEvent, cx| match ev {
            InputEvent::Submit => {
                if !v.accept_suggest(cx) {
                    v.add_sensitive(cx);
                }
            }
            InputEvent::Changed => v.update_suggest(crate::agents_suggest::Which::Sensitive, cx),
        })
        .detach();
        let vis_input = cx.new(|cx| TextInput::new(cx, "table, table.column or *").bare());
        cx.subscribe(&vis_input, |v: &mut Self, _, ev: &InputEvent, cx| match ev {
            InputEvent::Submit => {
                if !v.accept_suggest(cx) {
                    v.add_visible(cx);
                }
            }
            InputEvent::Changed => v.update_suggest(crate::agents_suggest::Which::Visible, cx),
        })
        .detach();
        let received = cx.new(|cx| {
            let mut g = crate::grid::ResultGrid::new(cx);
            g.tight_gutter();
            g
        });
        cx.subscribe(&received, |_: &mut Self, g, ev: &crate::grid::GridEvent, cx| {
            if let crate::grid::GridEvent::ViewValue { row, col } = ev
                && let Some(cell) = g.read(cx).value_at(*row, *col)
            {
                cx.emit(AgentsEvent::ViewValue(cell));
            }
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
            page: Page::Audit,
            privacy_open: true,
            visible: false,
            entries: Vec::new(),
            loaded: false,
            search,
            f_agent: None,
            f_conn: None,
            f_outcome: Outcomes::Any,
            f_when: When::All,
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
            editor: TypeEditor { editing: None, name, regex, mask: Mask::Hidden, more: false, validator: Validator::None, ignore_case: false, keep: 4, samples: Vec::new(), sample, confirm_delete: false },
            key,
            suggest: Default::default(),
            received,
            received_for: None,
            received_rows: None,
            received_zoom: false,
            received_tween: None,
            received_slot: Rc::default(),
            received_area: Rc::default(),
            detail_w: DETAIL_W,
            detail_drag: false,
            audit_body: Rc::default(),
            audit_scroll: gpui::ScrollHandle::new(),
            conns_scroll: gpui::ScrollHandle::new(),
            types_scroll: gpui::ScrollHandle::new(),
            audit_child: HashMap::new(),
            types_offset: 0,
            type_sel: None,
            replies: HashMap::new(),
            fields: HashMap::new(),
            loading: BTreeSet::new(),
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
        if page == Page::Types {
            self.privacy_open = true;
        }
        self.page = page;
        if page == Page::Audit {
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

    pub(crate) fn clear_filters(&mut self, cx: &mut Context<Self>) {
        self.f_agent = None;
        self.f_conn = None;
        self.f_outcome = Outcomes::Any;
        self.f_when = When::All;
        self.search.update(cx, |i, cx| i.set_text("", cx));
        self.selected = None;
        cx.notify();
    }

    /// Keyboard focus on the page's lists (a click on a row puts it there).
    pub(crate) fn focus_list(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }

    fn list_step(&mut self, step: Step, cx: &mut Context<Self>) -> bool {
        let next = |len: usize, cur: Option<usize>| -> Option<usize> {
            if len == 0 {
                return None;
            }
            Some(match (step, cur) {
                (Step::First, _) => 0,
                (Step::Last, _) => len - 1,
                (Step::By(_), None) => 0,
                (Step::By(d), Some(c)) => (c as isize + d).clamp(0, len as isize - 1) as usize,
            })
        };
        match self.page {
            Page::Audit => {
                let shown = self.filtered(cx);
                let cur = self.selected.and_then(|s| shown.iter().position(|&i| i == s));
                let Some(n) = next(shown.len(), cur) else { return true };
                self.selected = Some(shown[n]);
                self.menu = None;
                if let Some(&child) = self.audit_child.get(&shown[n]) {
                    self.audit_scroll.scroll_to_item(child);
                }
            }
            Page::Permissions | Page::Privacy => {
                let all = self.profiles(cx);
                let cur = self.conn.as_ref().and_then(|id| all.iter().position(|p| &p.id == id));
                let Some(n) = next(all.len(), cur) else { return true };
                self.conn = Some(all[n].id.clone());
                self.allow_error = None;
                self.yolo_confirm = false;
                self.scan_note = None;
                self.close_suggest(cx);
                self.conns_scroll.scroll_to_item(n);
            }
            Page::Mcp => return false,
            Page::Types => {
                let len = self.type_rows().len();
                let Some(n) = next(len, self.type_sel) else { return true };
                self.type_sel = Some(n);
                self.types_scroll.scroll_to_item(n + self.types_offset);
            }
        }
        cx.notify();
        true
    }

    fn list_open(&mut self, cx: &mut Context<Self>) -> bool {
        match self.page {
            Page::Audit => match self.selected {
                Some(i) => self.open_entry(i, cx),
                None => return false,
            },
            Page::Types => match self.type_sel.and_then(|k| self.type_rows().get(k).map(|r| r.id.clone())) {
                Some(id) => self.flip_type(&id, cx),
                None => return false,
            },
            _ => return false,
        }
        true
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
        // A small tree, as the explorer draws one: Sensitive data sits inside Privacy.
        let open = self.privacy_open;
        let rows = Page::ALL.into_iter().filter(|p| *p != Page::Types || open).map(|p| {
            let child = p == Page::Types;
            // A folded Privacy stands for the page inside it.
            let on = self.visible && (self.page == p || (p == Page::Privacy && !open && self.page == Page::Types));
            let id = SharedString::from(format!("agents-nav-{}", p.key()));
            let hov = t.hov;
            let chevron = (p == Page::Privacy).then(|| {
                div()
                    .children(ui::here(&"agents-nav-fold".into()))
                    .id("agents-nav-fold")
                    .size(px(16.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(ui::disclosure("agents-nav-chev", open, 10., t.tx4))
                    .on_click(cx.listener(|v, _, _, cx| {
                        cx.stop_propagation();
                        v.privacy_open = !v.privacy_open;
                        cx.notify();
                    }))
            });
            div()
                .children(ui::here(&id.clone().into()))
                .id(id)
                .h(px(28.))
                .pl(px(6. + if child { 12. } else { 0. }))
                .pr(px(12.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .text_color(if on { t.tx } else { t.tx2 })
                .when(on, |d| d.bg(t.prs))
                .when(!on, move |d| d.hover(move |s| s.bg(hov)))
                .child(div().w(px(16.)).flex_none().flex().justify_center().children(chevron))
                .child(icon(p.icon()).size(px(14.)).text_color(if on { t.tx } else { t.tx3 }))
                .child(div().flex_1().pl(px(4.)).child(p.label()))
                .on_click(cx.listener(move |v, _, window, cx| {
                    v.show(p, cx);
                    v.focus_list(window, cx);
                }))
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

    // ---- Audit ----

    fn filter_button(&self, t: &Theme, id: &'static str, label: String, cx: &mut Context<Self>) -> AnyElement {
        let items: Vec<(SharedString, bool, Act)> = match id {
            "agents-f-agent" => {
                let mut v = vec![("All agents".into(), self.f_agent.is_none(), Act::Agent(None))];
                v.extend(agents_of(&self.entries).into_iter().map(|a| (dataman_mcp::shown_client(&a.name).into(), self.f_agent.as_deref() == Some(a.name.as_str()), Act::Agent(Some(a.name)))));
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

    /// Before any agent called: how to connect one.
    fn render_no_calls(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let cmd = crate::mcp::stdio_command();
        let copy = cmd.clone();
        div()
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
            })
            .into_any_element()
    }

    fn render_audit(&mut self, t: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let shown = self.filtered(cx);
        if self.loaded && self.selected.is_none_or(|s| !shown.contains(&s)) {
            self.selected = shown.first().copied();
        }
        self.sync_received(cx);
        let zoom = self.received_frame(window);
        let area = self.received_area.clone();
        let measure_area = gpui::canvas(move |b, _, _| area.set(Some(b)), |_, _, _, _| {}).absolute().top_0().left_0().size_full();
        if zoom >= 1. && self.received_rows.is_some() {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(self.header(t, "Audit"))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .relative()
                        .flex()
                        .flex_col()
                        .border_t_1()
                        .border_color(t.line)
                        .px(px(32.))
                        .py(px(16.))
                        .child(measure_area)
                        .child(self.received_panel(t, true, true, cx)),
                )
                .into_any_element();
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
        let mut day: Option<String> = None;
        let mut rows: Vec<AnyElement> = Vec::new();
        self.audit_child.clear();
        for &i in &shown {
            let e = &self.entries[i];
            let d = day_label(e.ts);
            if day.as_deref() != Some(d.as_str()) {
                rows.push(
                    div()
                        .h(px(30.))
                        .flex_none()
                        .pl(px(32.))
                        .flex()
                        .items_end()
                        .pb(px(6.))
                        .border_b_1()
                        .border_color(t.line)
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.tx2)
                        .child(d.clone())
                        .into_any_element(),
                );
                day = Some(d);
            }
            let (note, bad) = audit_note(e);
            let sel = self.selected == Some(i);
            let hov = t.hov;
            let b = bounds.clone();
            let id = SharedString::from(format!("agents-audit-row-{i}"));
            let row = div()
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
                                        .child(div().text_color(t.tx).child(dataman_mcp::shown_client(&e.client)))
                                        .children(e.connection.as_deref().map(|c| self.conn_label(t, c, cx))),
                                )
                                .child(div().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx2).child(one_line(&shown_agent_text(e.statement().unwrap_or(e.tool.as_str()), 400))))
                                .child(div().text_size(px(11.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(if bad { t.err } else { t.tx3 }).child(note)),
                        )
                        .on_click(cx.listener(move |v, ev: &ClickEvent, window, cx| {
                            v.focus_list(window, cx);
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
                );
            self.audit_child.insert(i, rows.len());
            rows.push(row.into_any_element());
        }
        let list: AnyElement = if self.entries.is_empty() {
            self.render_no_calls(t, cx)
        } else if shown.is_empty() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(10.))
                .child(div().text_color(t.tx3).child("Nothing matches these filters."))
                .child(
                    ui::button(t, "agents-clear-filters", ui::Btn::Secondary)
                        .h(px(28.))
                        .child("Clear filters")
                        .on_click(cx.listener(|v, _, _, cx| v.clear_filters(cx))),
                )
                .into_any_element()
        } else {
            div().id("agents-audit-list").flex_1().min_w_0().overflow_y_scroll().track_scroll(&self.audit_scroll).flex().flex_col().children(rows).into_any_element()
        };
        // On its way, the reply leaves an empty slot in the detail and grows from it over the
        // page as the result grid grows over the editor: same time and curve.
        let moving = zoom > 0. || self.received_tween.is_some();
        let flight = moving.then(|| self.received_flight(t, zoom, cx)).flatten();
        let fade = |d: gpui::Div| d.when(zoom > 0., |d| d.opacity(1. - zoom));
        let body = self.audit_body.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(self.header(t, "Audit"))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(measure_area)
                    .child(fade(toolbar))
                    .child(
                        fade(div().flex_1().min_h_0().relative().flex().flex_row().border_t_1().border_color(t.line))
                            .child(gpui::canvas(move |b, _, _| body.set(Some(b)), |_, _, _, _| {}).absolute().top_0().left_0().size_full())
                            .child(list)
                            .children(self.render_detail(t, flight.is_some(), cx))
                            .children(self.selected.is_some().then(|| self.render_detail_seam(cx))),
                    )
                    .children(flight),
            )
            .into_any_element()
    }

    /// The detail as wide as the seam left it, within its bounds for the page's width now.
    pub(crate) fn detail_shown(&self) -> f32 {
        let max = self.audit_body.get().map_or(f32::MAX, |b| {
            let w = b.size.width.as_f32();
            (w / 2.).min(w - LIST_MIN).max(DETAIL_MIN)
        });
        self.detail_w.clamp(DETAIL_MIN, max)
    }

    /// The seam between the calls and the detail, as the editor groups' and the docks' seams:
    /// a drag resizes, a double click restores the default.
    fn render_detail_seam(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let body = self.audit_body.clone();
        let drag = self.detail_drag.then(|| {
            gpui::canvas(
                |_, _, _| {},
                move |_, _, window, _| {
                    let (moved, body) = (this.clone(), body.clone());
                    window.on_mouse_event(move |ev: &gpui::MouseMoveEvent, phase, _, cx| {
                        if phase != gpui::DispatchPhase::Bubble {
                            return;
                        }
                        let held = ev.pressed_button == Some(MouseButton::Left);
                        let right = body.get().map(|b| b.right());
                        let _ = moved.update(cx, |v, cx| {
                            match right {
                                Some(r) if held => {
                                    v.detail_w = (r - ev.position.x).as_f32();
                                    v.detail_w = v.detail_shown();
                                }
                                _ => v.detail_drag = false,
                            }
                            cx.notify();
                        });
                    });
                    let this = this.clone();
                    window.on_mouse_event(move |ev: &gpui::MouseUpEvent, phase, _, cx| {
                        if phase == gpui::DispatchPhase::Bubble && ev.button == MouseButton::Left {
                            let _ = this.update(cx, |v, cx| {
                                v.detail_drag = false;
                                cx.notify();
                            });
                        }
                    });
                },
            )
            .absolute()
            .size_0()
        });
        div()
            .children(ui::here(&"agents-detail-seam".into()))
            .id("agents-detail-seam")
            .absolute()
            .top_0()
            .bottom_0()
            .right(px(self.detail_shown() - SEAM_HIT / 2.))
            .w(px(SEAM_HIT))
            .cursor_col_resize()
            .on_mouse_down(MouseButton::Left, cx.listener(|v, ev: &gpui::MouseDownEvent, _, cx| {
                cx.stop_propagation();
                if ev.click_count == 2 {
                    v.detail_w = DETAIL_W;
                    v.detail_drag = false;
                } else {
                    v.detail_drag = true;
                }
                cx.notify();
            }))
            .children(drag)
            .into_any_element()
    }

    /// The zoom's progress this frame, asking for the next while it moves.
    fn received_frame(&mut self, window: &mut Window) -> f32 {
        let target = if self.received_zoom { 1. } else { 0. };
        match self.received_tween.map(|z| z.at(target, std::time::Instant::now())) {
            Some(Some(p)) => {
                window.request_animation_frame();
                p
            }
            Some(None) => {
                self.received_tween = None;
                target
            }
            None => target,
        }
    }

    /// The reply between its slot in the detail and the page below the header, `p` of the way.
    fn received_flight(&self, t: &Theme, p: f32, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (slot, area) = (self.received_slot.get()?, self.received_area.get()?);
        let lerp = |a: f32, b: f32| a + (b - a) * p;
        // The slot sits 1px lower than the panel's own top: the zoomed page's border line.
        let (x, y) = ((slot.left() - area.left()).as_f32(), (slot.top() - area.top()).as_f32() - 1.);
        Some(
            div()
                .absolute()
                .left(px(lerp(x, 0.)))
                .top(px(lerp(y, 0.)))
                .w(px(lerp(slot.size.width.as_f32(), area.size.width.as_f32())))
                .h(px(lerp(slot.size.height.as_f32() + 1., area.size.height.as_f32())))
                .px(px(lerp(0., 32.)))
                .py(px(lerp(0., 16.)))
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(t.bg)
                .border_t_1()
                .border_color(t.line.opacity(p))
                .child(self.received_panel(t, self.received_zoom, true, cx))
                .into_any_element(),
        )
    }

    fn render_detail(&self, t: &Theme, away: bool, cx: &mut Context<Self>) -> Option<AnyElement> {
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
            fact("Agent", dataman_mcp::shown_client(&e.client)),
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
        let received: Option<AnyElement> = (e.outcome == Outcome::Ok).then(|| {
            if self.received_rows.is_some() {
                let slot = self.received_slot.clone();
                let measure = gpui::canvas(move |b, _, _| slot.set(Some(b)), |_, _, _, _| {}).absolute().top_0().left_0().size_full();
                let held = self.received_slot.get().map(|b| b.size.height).filter(|_| away);
                return match held {
                    Some(h) => div().relative().flex_none().h(h).child(measure).into_any_element(),
                    None => div().relative().child(measure).child(self.received_panel(t, false, false, cx)).into_any_element(),
                };
            }
            let note = |s: &str| div().text_size(px(11.)).line_height(px(16.)).text_color(t.tx3).child(s.to_string()).into_any_element();
            let body = match e.id.as_deref().map(|id| self.replies.get(id).cloned().unwrap_or(Kept::Reading)) {
                Some(Kept::Reply { text, cut }) => div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(render_reply(t, &text))
                    .when(cut, |d| d.child(note(&format!("Longer than {} KB: the log keeps the first {} KB.", dataman_mcp::audit::REPLY_MAX / 1024, dataman_mcp::audit::REPLY_MAX / 1024))))
                    .into_any_element(),
                Some(Kept::Reading) => note("Reading the log…"),
                Some(Kept::RotatedOut) => note("This reply was rotated out of the log."),
                None => note("Logged before Dataman kept replies."),
            };
            div().flex().flex_col().gap(px(8.)).child(ui::caps(t, "WHAT THE AGENT RECEIVED")).child(body).into_any_element()
        });
        Some(
            div()
                .id("agents-detail")
                .w(px(self.detail_shown()))
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(18.))
                .px(px(24.))
                .py(px(20.))
                .border_l_1()
                .border_color(t.line)
                .overflow_y_scroll()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.))
                        .text_color(t.tx3)
                        .child(div().text_color(t.tx).font_weight(FontWeight::SEMIBOLD).child(e.tool.clone()))
                        .child(div().flex_1())
                        .child(format!("{} · {}", e.ts.with_timezone(&Local).format("%b %-d, %Y · %H:%M:%S"), fmt_ms(e.ms))),
                )
                .children(e.statement().map(|s| block(&shown_agent_text(s, 20_000))))
                .children(e.rewritten.as_deref().map(|r| div().flex().flex_col().gap(px(6.)).child(ui::caps(t, "REWRITTEN")).child(block(&shown_agent_text(r, 20_000)))))
                .child(grid)
                .children(error.map(|m| div().text_size(px(11.5)).line_height(px(17.)).text_color(t.err).child(shown_agent_text(&m, 2_000))))
                .children(received)
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
    pub(crate) fn demo_audit_w(&self) -> f32 {
        self.audit_body.get().map_or(0., |b| b.size.width.as_f32())
    }

    #[cfg(feature = "checks")]
    pub(crate) fn demo_received_area_w(&self) -> f32 {
        self.received_area.get().map_or(0., |b| b.size.width.as_f32())
    }

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

const REPLY_ROWS: usize = 8;

/// Audit's detail: its default width and narrowest, and the narrowest the calls may get; it
/// never takes more than half the page.
pub(crate) const DETAIL_W: f32 = 420.;
pub(crate) const DETAIL_MIN: f32 = 340.;
const LIST_MIN: f32 = 380.;
const SEAM_HIT: f32 = 8.;

impl AgentsPage {
    /// Puts the selected call's reply in the grid when it holds rows or documents.
    fn sync_received(&mut self, cx: &mut Context<Self>) {
        let entry = self.selected.and_then(|i| self.entries.get(i));
        let id = entry.map(|e| e.id.clone());
        if self.received_for == Some(id.clone().flatten()) && self.received_for.is_some() {
            return;
        }
        let kept = id.clone().flatten().map(|id| self.kept(&id, cx));
        if kept == Some(Kept::Reading) {
            return;
        }
        self.received_for = Some(id.clone().flatten());
        let table = match &kept {
            Some(Kept::Reply { text, cut: false }) => reply_table(text),
            _ => None,
        };
        self.received_rows = table.as_ref().map(|(rs, total)| (rs.rows(), *total));
        if table.is_none() {
            self.received_zoom = false;
            self.received_tween = None;
        }
        let rt = self.core.handle();
        self.received.update(cx, |g, cx| g.set_result(table.map(|(rs, _)| rs), rt, cx));
    }

    /// A call's reply from the log, read in the background the first time it is asked for.
    pub(crate) fn kept(&mut self, id: &str, cx: &mut Context<Self>) -> Kept {
        if let Some(k) = self.replies.get(id) {
            return k.clone();
        }
        self.replies.insert(id.to_string(), Kept::Reading);
        let (log, key) = (self.core.dirs.mcp_audit_file(), id.to_string());
        let task = cx.background_spawn(async move { dataman_mcp::audit::reply(&log, &key) });
        let key = id.to_string();
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |v, cx| {
                let k = match r {
                    Some(r) => Kept::Reply { text: r.reply.into(), cut: r.cut },
                    None => Kept::RotatedOut,
                };
                v.replies.insert(key, k);
                v.received_for = None;
                cx.notify();
            });
        })
        .detach();
        Kept::Reading
    }

    /// What a restart starts from: nothing read yet.
    #[cfg(feature = "checks")]
    pub(crate) fn forget_replies(&mut self, cx: &mut Context<Self>) {
        self.replies.clear();
        self.received_for = None;
        cx.notify();
    }

    pub(crate) fn toggle_received_zoom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page != Page::Audit || self.received_rows.is_none() {
            return;
        }
        let from = self.received_now();
        self.received_zoom = !self.received_zoom;
        self.received_tween = (!cx.reduce_motion()).then(|| crate::motion::ZoomTween::new(from));
        window.focus(&self.received.focus_handle(cx), cx);
        cx.notify();
    }

    /// How far the reply is zoomed, 0 in the detail to 1 over the page.
    pub(crate) fn received_now(&self) -> f32 {
        let target = if self.received_zoom { 1. } else { 0. };
        self.received_tween.and_then(|z| z.at(target, std::time::Instant::now())).unwrap_or(target)
    }

    /// The reply in the results grid, with the results panel's zoom. `fill` grows the grid to
    /// the panel's height, zoomed or on its way.
    fn received_panel(&self, t: &Theme, zoomed: bool, fill: bool, cx: &mut Context<Self>) -> AnyElement {
        let (shown, total) = self.received_rows.unwrap_or((0, 0));
        let count = if total as usize > shown { format!("{shown} of {total}") } else { plural(shown as u64, "row", "rows") };
        let key = if zoomed { Some("esc".to_string()) } else { crate::keymap::key("zoom_result") };
        let bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(ui::caps(t, "WHAT THE AGENT RECEIVED"))
            .child(div().flex_1())
            .child(div().text_size(px(11.)).text_color(t.tx3).child(count))
            .child(
                ui::button(t, "agents-recv-zoom", ui::Btn::Ghost)
                    .h(px(24.))
                    .w(px(26.))
                    .px(px(0.))
                    .justify_center()
                    .when(zoomed, |d| d.bg(t.acc_s).text_color(t.acc_t).border_1().border_color(t.acc))
                    .child(icon(if zoomed { Icon::Collapse } else { Icon::Expand }).size(px(13.)).text_color(if zoomed { t.acc_t } else { t.tx2 }))
                    .tip(ui::tooltip(if zoomed { "Exit zoom" } else { "Zoom result" }, None, key.as_deref()))
                    .on_click(cx.listener(|v, _, window, cx| v.toggle_received_zoom(window, cx))),
            );
        let grid = div().border_1().border_color(t.line).child(self.received.clone());
        let grid = if fill { grid.flex_1().min_h_0() } else { grid.h(px(260.)).flex_none() };
        div()
            .children(ui::here(&"agents-recv".into()))
            .flex()
            .flex_col()
            .gap(px(8.))
            .when(fill, |d| d.size_full())
            .child(bar)
            .child(grid)
            .into_any_element()
    }
}

/// A reply's rows (or documents) as a result set for the grid, with the rows the reply counted.
pub(crate) fn reply_table(text: &str) -> Option<(Arc<dataman_core::result::ResultSet>, u64)> {
    use dataman_core::result::{ChunkBuilder, ColumnInfo, ResultSet};
    use dataman_core::value::Kind;
    use serde_json::Value;
    let v: Value = serde_json::from_str(text).ok()?;
    let result = v.get("results").and_then(|r| r.get(0)).unwrap_or(&v);
    let (names, types, rows): (Vec<String>, Vec<String>, Vec<Vec<Value>>) = if let (Some(cols), Some(rows)) = (result.get("columns").and_then(Value::as_array), result.get("rows").and_then(Value::as_array)) {
        let names = cols.iter().map(|c| c.get(0).and_then(Value::as_str).or(c.as_str()).unwrap_or("").to_string()).collect();
        let types = cols.iter().map(|c| c.get(1).and_then(Value::as_str).unwrap_or("").to_string()).collect();
        (names, types, rows.iter().map(|r| r.as_array().cloned().unwrap_or_default()).collect())
    } else if let Some(docs) = result.get("documents").and_then(Value::as_array) {
        let mut names: Vec<String> = Vec::new();
        for d in docs {
            match d.as_object() {
                Some(m) => names.extend(m.keys().filter(|k| !names.contains(k)).cloned().collect::<Vec<_>>()),
                None if !names.iter().any(|n| n == "value") => names.push("value".into()),
                None => {}
            }
        }
        names.truncate(64);
        let rows = docs.iter().map(|d| names.iter().map(|n| d.as_object().map_or_else(|| d.clone(), |m| m.get(n).cloned().unwrap_or(Value::Null))).collect()).collect();
        (names.clone(), vec![String::new(); names.len()], rows)
    } else {
        return None;
    };
    let kind_of = |k: usize| {
        let vals: Vec<&Value> = rows.iter().filter_map(|r| r.get(k)).filter(|x| !x.is_null()).collect();
        if vals.is_empty() {
            Kind::Text
        } else if vals.iter().all(|x| x.is_i64()) {
            Kind::Int
        } else if vals.iter().all(|x| x.is_number()) {
            Kind::Float
        } else if vals.iter().all(|x| x.is_boolean()) {
            Kind::Bool
        } else if vals.iter().any(|x| x.is_object() || x.is_array()) {
            Kind::Json
        } else {
            Kind::Text
        }
    };
    let cols: Vec<ColumnInfo> = names.iter().enumerate().map(|(k, n)| ColumnInfo { name: n.clone(), type_name: types.get(k).cloned().unwrap_or_default(), type_oid: 0, kind: kind_of(k) }).collect();
    let mut b = ChunkBuilder::new(&cols);
    for r in &rows {
        for (k, c) in cols.iter().enumerate() {
            match (r.get(k).unwrap_or(&Value::Null), c.kind) {
                (Value::Null, _) => b.push_null(k),
                (x, Kind::Int) => b.push_i64(k, x.as_i64().unwrap_or(0)),
                (x, Kind::Float) => b.push_f64(k, x.as_f64().unwrap_or(0.)),
                (x, Kind::Bool) => b.push_bool(k, x.as_bool().unwrap_or(false)),
                (Value::String(s), _) => b.push_text(k, s),
                (x, _) => b.push_text(k, &x.to_string()),
            }
        }
        b.end_row();
    }
    let rs = ResultSet::new("");
    let total = result.get("row_count").or(result.get("total")).and_then(Value::as_u64).unwrap_or(rows.len() as u64);
    rs.finish_with(Some(cols), Some(b.finish()), None, Vec::new(), None, Duration::ZERO);
    Some((rs, total))
}

/// A reply as the agent received it: rows as a small table, documents one per line, anything
/// else as its JSON.
pub(crate) fn render_reply(t: &Theme, text: &str) -> AnyElement {
    use serde_json::Value;
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return div().text_size(px(11.5)).line_height(px(17.)).text_color(t.tx2).child(text.chars().take(2000).collect::<String>()).into_any_element();
    };
    let result = v.get("results").and_then(|r| r.get(0)).unwrap_or(&v);
    let cell = |x: &Value| match x {
        Value::Null => "NULL".to_string(),
        Value::String(s) => one_line(s),
        other => one_line(&other.to_string()),
    };
    let foot = |shown: usize, total: Option<u64>, extra: Option<String>| {
        let mut parts = Vec::new();
        if let Some(n) = total.filter(|n| *n as usize > shown) {
            parts.push(format!("{shown} of {n} shown"));
        }
        parts.extend(extra);
        (!parts.is_empty()).then(|| div().text_size(px(11.)).text_color(t.tx3).child(parts.join(" · ")))
    };
    if let (Some(cols), Some(rows)) = (result.get("columns").and_then(Value::as_array), result.get("rows").and_then(Value::as_array)) {
        let names: Vec<String> = cols.iter().map(|c| c.get(0).and_then(Value::as_str).or(c.as_str()).unwrap_or("").to_string()).collect();
        let shown: Vec<Vec<String>> = rows.iter().take(REPLY_ROWS).map(|r| r.as_array().map(|a| a.iter().map(cell).collect()).unwrap_or_default()).collect();
        let widths: Vec<f32> = (0..names.len())
            .map(|k| {
                let chars = shown.iter().filter_map(|r| r.get(k)).chain(std::iter::once(&names[k])).map(|s| s.chars().count()).max().unwrap_or(4).clamp(3, 18);
                chars as f32 * 7.0 + 14.
            })
            .collect();
        let line = |vals: Vec<String>, head: bool| {
            div().h(px(22.)).flex().flex_row().items_center().border_b_1().border_color(t.line).children(vals.into_iter().enumerate().map(|(k, x)| {
                let null = !head && x == "NULL";
                div()
                    .w(px(widths.get(k).copied().unwrap_or(60.)))
                    .flex_none()
                    .px(px(6.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(if head || null { t.tx3 } else { t.tx })
                    .child(x)
            }))
        };
        let total = result.get("row_count").and_then(Value::as_u64);
        return div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .id("agents-reply-table")
                    .overflow_x_scroll()
                    .border_1()
                    .border_color(t.line)
                    .bg(t.field)
                    .text_size(px(11.5))
                    .child(div().flex().flex_col().min_w(px(widths.iter().sum::<f32>())).child(line(names, true)).children(shown.into_iter().map(|r| line(r, false)))),
            )
            .children(foot(rows.len().min(REPLY_ROWS), total.or(Some(rows.len() as u64)), None))
            .into_any_element();
    }
    if let Some(docs) = result.get("documents").and_then(Value::as_array) {
        let lines = docs.iter().take(REPLY_ROWS).map(|d| div().whitespace_nowrap().overflow_hidden().text_ellipsis().text_color(t.tx).child(one_line(&d.to_string())));
        return div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(div().px(px(10.)).py(px(8.)).flex().flex_col().gap(px(4.)).bg(t.field).border_1().border_color(t.line).text_size(px(11.5)).children(lines))
            .children(foot(docs.len().min(REPLY_ROWS), result.get("total").and_then(Value::as_u64).or(Some(docs.len() as u64)), None))
            .into_any_element();
    }
    // A write: what each statement changed, who allowed it, and the RETURNING rows.
    if v.get("committed").is_some() || v.get("written").is_some() {
        let mut lines: Vec<String> = Vec::new();
        for st in v.get("statements").and_then(Value::as_array).into_iter().flatten() {
            let (verb, n) = (st.get(0).and_then(Value::as_str).unwrap_or("?"), st.get(1).and_then(Value::as_u64).unwrap_or(0));
            lines.push(format!("{verb} · {}", plural(n, "row", "rows")));
        }
        if let Some(w) = v.get("written").and_then(Value::as_str) {
            lines.push(format!("{w} · {}", plural(v.get("affected").and_then(Value::as_u64).unwrap_or(0), "row", "rows")));
        }
        if let Some(a) = v.get("approved") {
            let by = a.get("by").and_then(Value::as_str).unwrap_or("you");
            let why: Vec<&str> = a.get("reasons").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            lines.push(if why.is_empty() { format!("Allowed by {by}") } else { format!("Allowed by {by} · {}", why.join(", ")) });
        }
        let returning = v.get("returning").map(|r| render_reply(t, &r.to_string()));
        return div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(div().px(px(10.)).py(px(8.)).flex().flex_col().gap(px(4.)).bg(t.field).border_1().border_color(t.line).text_size(px(11.5)).text_color(t.tx).children(lines.into_iter().map(|l| div().child(l))))
            .children(returning.map(|r| div().flex().flex_col().gap(px(6.)).child(div().text_size(px(10.5)).text_color(t.tx3).child("RETURNING")).child(r)))
            .into_any_element();
    }
    let pretty = serde_json::to_string_pretty(&v).unwrap_or_default();
    let lines: Vec<&str> = pretty.lines().collect();
    let more = lines.len().saturating_sub(16);
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
            div()
                .px(px(10.))
                .py(px(8.))
                .bg(t.field)
                .border_1()
                .border_color(t.line)
                .text_size(px(11.5))
                .line_height(px(17.))
                .text_color(t.tx2)
                .children(lines.into_iter().take(16).map(|l| div().whitespace_nowrap().overflow_hidden().child(l.to_string()))),
        )
        .when(more > 0, |d| d.child(div().text_size(px(11.)).text_color(t.tx3).child(format!("{more} more lines"))))
        .into_any_element()
}

pub(crate) fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A tooltip that reads as a small page: a bold title, a lead line, terms with what they mean,
/// code, an example and a note set apart at the foot.
#[derive(Clone, Default)]
pub(crate) struct Tip {
    title: Option<SharedString>,
    parts: Vec<TipPart>,
}

#[derive(Clone)]
enum TipPart {
    Lead(SharedString),
    Item(SharedString, SharedString),
    Code(SharedString),
    Example(SharedString),
    Note(SharedString),
}

impl Tip {
    pub(crate) fn new(title: impl Into<SharedString>) -> Tip {
        Tip { title: Some(title.into()), parts: Vec::new() }
    }

    pub(crate) fn lead(mut self, s: impl Into<SharedString>) -> Tip {
        self.parts.push(TipPart::Lead(s.into()));
        self
    }

    pub(crate) fn item(mut self, term: impl Into<SharedString>, what: impl Into<SharedString>) -> Tip {
        self.parts.push(TipPart::Item(term.into(), what.into()));
        self
    }

    pub(crate) fn code(mut self, s: impl Into<SharedString>) -> Tip {
        self.parts.push(TipPart::Code(s.into()));
        self
    }

    pub(crate) fn example(mut self, s: impl Into<SharedString>) -> Tip {
        self.parts.push(TipPart::Example(s.into()));
        self
    }

    pub(crate) fn note(mut self, s: impl Into<SharedString>) -> Tip {
        self.parts.push(TipPart::Note(s.into()));
        self
    }

    /// Everything it says, for checks.
    #[cfg(feature = "checks")]
    pub(crate) fn text(&self) -> String {
        let parts = self.parts.iter().map(|p| match p {
            TipPart::Lead(s) | TipPart::Code(s) | TipPart::Example(s) | TipPart::Note(s) => s.to_string(),
            TipPart::Item(term, what) => format!("{term}: {what}"),
        });
        self.title.iter().map(|t| t.to_string()).chain(parts).collect::<Vec<_>>().join("\n")
    }

    pub(crate) fn view(&self) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static {
        let tip = self.clone();
        move |_, cx| {
            let tip = tip.clone();
            cx.new(|_| TipView { tip }).into()
        }
    }
}

/// A label with a `?` whose hover shows a `Tip`.
pub(crate) fn label_tip(t: &Theme, id: &str, label: &str, tip: Tip) -> gpui::Div {
    div().flex().flex_row().items_center().gap(px(6.)).child(ui::caps(t, label.to_string())).child(qmark(t, id, tip))
}

pub(crate) fn qmark(t: &Theme, id: &str, tip: Tip) -> gpui::Stateful<gpui::Div> {
    let hov = t.tx2;
    div()
        .children(ui::here(&SharedString::from(id.to_string()).into()))
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
        .hover(move |s| s.text_color(hov))
        .child("?")
        .tip(tip.view())
}

pub(crate) struct TipView {
    tip: Tip,
}

impl Render for TipView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let code_font = crate::editor::mono().family;
        let parts = self.tip.parts.iter().map(|p| match p {
            TipPart::Lead(s) => div().text_color(t.tx2).child(s.clone()).into_any_element(),
            TipPart::Item(term, what) => div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(div().text_color(t.tx).font_weight(FontWeight::SEMIBOLD).child(term.clone()))
                .child(div().text_color(t.tx2).child(what.clone()))
                .into_any_element(),
            TipPart::Code(s) => div().px(px(8.)).py(px(5.)).bg(t.field).border_1().border_color(t.line).font_family(code_font.clone()).text_color(t.tx).child(s.clone()).into_any_element(),
            TipPart::Example(s) => div().flex().flex_col().gap(px(2.)).child(div().text_size(px(10.5)).text_color(t.tx3).child("EXAMPLE")).child(div().text_color(t.tx2).child(s.clone())).into_any_element(),
            TipPart::Note(s) => div().mt(px(2.)).pt(px(10.)).border_t_1().border_color(t.line).text_color(t.tx3).child(s.clone()).into_any_element(),
        });
        div()
            .children(ui::here(&"tooltip".into()))
            .w(px(360.))
            .px(px(14.))
            .py(px(12.))
            .bg(t.pop)
            .shadow(t.ring())
            .font_family(mono())
            .text_size(px(11.5))
            .line_height(px(17.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .children(self.tip.title.clone().map(|title| div().text_size(px(12.5)).text_color(t.tx).font_weight(FontWeight::BOLD).child(title)))
            .children(parts)
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
            Page::Audit => self.render_audit(&t, window, cx),
            Page::Permissions => self.render_permissions(&t, window, cx),
            Page::Privacy => self.render_privacy(&t, window, cx),
            Page::Types => self.render_types(&t, window, cx),
            Page::Mcp => self.render_mcp(&t, cx),
        };
        div()
            .id("agents-page")
            .key_context("AgentsPage")
            .on_action(cx.listener(|v, _: &ListUp, _, cx| if !v.list_step(Step::By(-1), cx) { cx.propagate() }))
            .on_action(cx.listener(|v, _: &ListDown, _, cx| if !v.list_step(Step::By(1), cx) { cx.propagate() }))
            .on_action(cx.listener(|v, _: &ListPageUp, _, cx| if !v.list_step(Step::By(-PAGE_STEP), cx) { cx.propagate() }))
            .on_action(cx.listener(|v, _: &ListPageDown, _, cx| if !v.list_step(Step::By(PAGE_STEP), cx) { cx.propagate() }))
            .on_action(cx.listener(|v, _: &ListFirst, _, cx| if !v.list_step(Step::First, cx) { cx.propagate() }))
            .on_action(cx.listener(|v, _: &ListLast, _, cx| if !v.list_step(Step::Last, cx) { cx.propagate() }))
            .on_action(cx.listener(|v, _: &ListOpen, _, cx| if !v.list_open(cx) { cx.propagate() }))
            .on_action(cx.listener(|v, _: &crate::app::ToggleResultZoom, window, cx| {
                if v.page == Page::Audit && v.received_rows.is_some() {
                    v.toggle_received_zoom(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|v, _: &crate::app::FocusEditor, window, cx| {
                // Esc leaves the zoomed reply first, as it leaves a zoomed result.
                if v.received_zoom {
                    v.toggle_received_zoom(window, cx);
                } else {
                    cx.propagate();
                }
            }))
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
