use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, MouseButton, MouseDownEvent, SharedString, Subscription, UniformListScrollHandle, Window, actions,
    anchored, deferred, div, point, prelude::*, uniform_list,
};
use crate::theme::{Pt, px};
use dataman_core::Connection;
use dataman_core::pins::Pin;
use dataman_core::schema::objects::{approx_count, constraint_summary, human_size, partition_bound_summary};
use dataman_core::schema::{ConstraintKind, FuncKind, RelKind, RelStats, TypeKind};

use crate::assets::{Icon, icon};
use crate::conns::{ConnState, ConnStore};
use crate::grid::ui_count;
use crate::input::{InputEvent, TextInput};
use crate::motion::{FindPop, PopFrame, PopStyle};
use crate::theme::{ActiveTheme, Theme};
use crate::tree_menu::{self as menu, Act, ConnPhase, Entry, Obj, Target};
use crate::ui;
use crate::ui::Tipped;

actions!(
    explorer,
    [
        SelectPrev,
        SelectNext,
        Expand,
        Collapse,
        Open,
        ReloadNode,
        EditConnection,
        Disconnect,
        FocusFilter,
        Escape,
        ShowRowActions,
        MenuUp,
        MenuDown,
        MenuLeft,
        MenuRight,
        MenuConfirm,
        MenuConfirmAlt,
        MenuClose,
    ]
);

const MENU_CONTEXT: &str = "ExplorerMenu";
const ROW_GROUP: &str = "tree-row";
const MORE_ROOM: f32 = 14.;

const CONTEXT: &str = "Explorer";

const POP_PAUSE: Duration = Duration::from_millis(150);

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        crate::keymap::kb("up", SelectPrev, c),
        crate::keymap::kb("down", SelectNext, c),
        crate::keymap::kb("right", Expand, c),
        crate::keymap::kb("left", Collapse, c),
        crate::keymap::kb("enter", Open, c),
        crate::keymap::kb("cmd-r", ReloadNode, c),
        crate::keymap::kb("cmd-i", EditConnection, c),
        crate::keymap::kb("cmd-backspace", Disconnect, c),
        crate::keymap::kb("/", FocusFilter, Some("Explorer && !TextInput")),
        crate::keymap::kb("escape", Escape, c),
        // The Mac convention for the keyboard context menu (PC keyboards' menu key does not
        // reach GPUI on macOS).
        crate::keymap::kb("shift-f10", ShowRowActions, c),
    ]);
    let m = Some(MENU_CONTEXT);
    cx.bind_keys([
        crate::keymap::kb("up", MenuUp, m),
        crate::keymap::kb("down", MenuDown, m),
        crate::keymap::kb("left", MenuLeft, m),
        crate::keymap::kb("right", MenuRight, m),
        crate::keymap::kb("enter", MenuConfirm, m),
        crate::keymap::kb("space", MenuConfirm, m),
        crate::keymap::kb("alt-enter", MenuConfirmAlt, m),
        crate::keymap::kb("escape", MenuClose, m),
        crate::keymap::kb("shift-f10", MenuClose, m),
    ]);
}

pub enum TreeEvent {
    OpenRelation { conn: String, db: String, schema: String, name: String },
    OpenCollection { conn: String, db: String, coll: String },
    Selected(String),
    EditConnection(String),
    NewConnection,
    Disconnect(String),
    FocusEditor,
    RowAction { act: Act, target: Target, alt: bool },
    Notice(String),
    Did(crate::undo::Entry),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum G {
    Tables,
    Views,
    MatViews,
    Functions,
    Sequences,
    Types,
    Triggers,
}

impl G {
    const ALL: [G; 7] = [G::Tables, G::Views, G::MatViews, G::Functions, G::Sequences, G::Types, G::Triggers];

    fn key(self) -> &'static str {
        match self {
            G::Tables => "tables",
            G::Views => "views",
            G::MatViews => "matviews",
            G::Functions => "functions",
            G::Sequences => "sequences",
            G::Types => "types",
            G::Triggers => "triggers",
        }
    }

    fn label(self) -> &'static str {
        match self {
            G::MatViews => "materialized views",
            g => g.key(),
        }
    }

    fn holds(self, k: RelKind) -> Option<bool> {
        match self {
            G::Tables => Some(!k.is_view()),
            G::Views => Some(k == RelKind::View),
            G::MatViews => Some(k == RelKind::MaterializedView),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sub {
    Constraints,
    Triggers,
    Partitions,
}

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    /// An explorer folder (its connections are the rows under it).
    Folder { id: String },
    Conn { id: String },
    Db { conn: String, db: String },
    Schema { conn: String, db: String, schema: String },
    Group { conn: String, db: String, schema: String, g: G },
    Roles { conn: String },
    Extensions { conn: String, db: String },
    Pinned { conn: String },
    Rel { conn: String, db: String, schema: String, name: String, kind: RelKind },
    RelRef { conn: String, db: String, schema: String, name: String },
    Columns { conn: String, db: String, schema: String, name: String },
    Indexes { conn: String, db: String, schema: String, name: String },
    TableSub { conn: String, db: String, schema: String, name: String, what: Sub },
    Column { name: String },
    Index { name: String, definition: Option<String> },
    Object { conn: String, db: String, schema: String, name: String, what: Obj },
    DocDb { conn: String, db: String },
    Coll { conn: String, db: String, name: String },
    Error { retry: String },
    Leaf,
}

#[derive(Clone, Copy, PartialEq)]
enum Tone {
    Normal,
    Folder,
    Conn,
    ConnLar,
    ConnOff,
    ConnWait,
    Muted,
    Italic,
    Error,
}

#[derive(Clone, Copy, PartialEq)]
enum Meta {
    Plain,
    Ok,
    Warn,
    Err,
    Off,
    LarState,
    NotReported,
    Link,
}

#[derive(Clone)]
struct Row {
    id: SharedString,
    depth: usize,
    label: SharedString,
    hl: Option<Range<usize>>,
    tone: Tone,
    icon: Option<(Icon, Option<Hsla>)>,
    blank_icon: bool,
    chip: Option<Hsla>,
    chip_dim: bool,
    tip: Option<SharedString>,
    lar: bool,
    tunnel: bool,
    logo: Option<dataman_core::Engine>,
    lock: bool,
    meta: SharedString,
    meta_short: Option<SharedString>,
    meta_kind: Meta,
    expandable: bool,
    expanded: bool,
    kind: Kind,
    /// The explorer folder whose block the row is in (its own row, its connections and theirs).
    in_folder: Option<SharedString>,
}

impl Row {
    fn new(id: impl Into<SharedString>, depth: usize, label: impl Into<SharedString>, kind: Kind) -> Row {
        Row {
            id: id.into(),
            depth,
            label: label.into(),
            hl: None,
            tone: Tone::Normal,
            icon: None,
            blank_icon: false,
            chip: None,
            chip_dim: false,
            tip: None,
            lar: false,
            tunnel: false,
            logo: None,
            lock: false,
            meta: SharedString::default(),
            meta_short: None,
            meta_kind: Meta::Plain,
            expandable: false,
            expanded: false,
            kind,
            in_folder: None,
        }
    }
    fn icon(mut self, i: Icon) -> Row {
        self.icon = Some((i, None));
        self
    }
    fn icon_colored(mut self, i: Icon, c: Hsla) -> Row {
        self.icon = Some((i, Some(c)));
        self
    }
    fn blank_icon(mut self) -> Row {
        self.blank_icon = true;
        self
    }
    fn meta(mut self, m: impl Into<SharedString>, k: Meta) -> Row {
        self.meta = m.into();
        self.meta_kind = k;
        self
    }
    fn tone(mut self, t: Tone) -> Row {
        self.tone = t;
        self
    }
    fn lock(mut self, on: bool) -> Row {
        self.lock = on;
        self
    }
    fn expandable(mut self, open: bool) -> Row {
        self.expandable = true;
        self.expanded = open;
        self
    }
}

struct RowMenu {
    row: Option<SharedString>,
    target: Target,
    entries: Vec<Entry>,
    origin: (f32, f32),
    hi: Option<usize>,
    sub: Option<usize>,
    sub_hi: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
pub enum MenuAnchor {
    Mouse(gpui::Point<gpui::Pixels>),
    Row,
    RowMore,
}

pub struct SchemaTree {
    store: Entity<ConnStore>,
    focus: FocusHandle,
    filter: Entity<TextInput>,
    rows: Vec<Row>,
    expanded: HashSet<String>,
    folder_turn: Option<(String, Instant, bool)>,
    loading: HashSet<String>,
    errors: HashMap<String, String>,
    hovered: Option<SharedString>,
    selected: Option<SharedString>,
    scroll: UniformListScrollHandle,
    filter_stats: Option<(usize, usize, Option<String>)>,
    menu: Option<RowMenu>,
    menu_focus: FocusHandle,
    _menu_blur: Option<Subscription>,
    menu_closed: Option<(Option<SharedString>, Instant)>,
    seen_version: u64,
    /// Open nodes read again after their cache was dropped, with the cache version then.
    reread: HashMap<String, u64>,
    pub last_rebuild: std::time::Duration,
    pop: Option<FindPop<SharedString>>,
    pop_now: Option<(SharedString, PopFrame)>,
    pop_wait: Option<gpui::Task<()>>,
    popped: Option<(SharedString, Range<usize>)>,
    reveal: Option<String>,
    conn_drop: Option<ConnDrop>,
    /// Folders the user shut (they start open).
    shut_folders: HashSet<String>,
    /// The folder the explorer shows and searches, alone.
    scope: Option<String>,
    scope_menu: bool,
    /// A folder being named in place.
    naming: Option<(String, Entity<TextInput>)>,
    focus_back: bool,
    pub conn_bounds: std::rc::Rc<std::cell::RefCell<HashMap<SharedString, gpui::Bounds<gpui::Pixels>>>>,
}

pub struct DraggedConn(pub String);

/// Where a dragged connection lands.
#[derive(Clone, Debug, PartialEq)]
pub enum ConnDrop {
    /// Onto a folder's row: into that folder.
    Into(String),
    /// Among a group's connections (a folder's, or the top level's): before one, or at the end.
    At { folder: Option<String>, before: Option<String> },
}

impl EventEmitter<TreeEvent> for SchemaTree {}

impl Focusable for SchemaTree {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn cid(id: &str) -> String {
    format!("c:{id}")
}
pub fn fid(id: &str) -> String {
    format!("f:{id}")
}
fn did(c: &str, db: &str) -> String {
    format!("c:{c}/d:{db}")
}
fn sid(c: &str, db: &str, s: &str) -> String {
    format!("c:{c}/d:{db}/s:{s}")
}
fn gid(c: &str, db: &str, s: &str, g: G) -> String {
    format!("c:{c}/d:{db}/s:{s}/{}", g.key())
}
fn rid(c: &str, db: &str, s: &str, n: &str) -> String {
    format!("c:{c}/d:{db}/s:{s}/r:{n}")
}
/// Unlike the other nodes it starts open: its id in `expanded` means the user closed it.
fn pinned_id(c: &str) -> String {
    format!("c:{c}/pinned")
}
pub fn pin_id(c: &str, db: &str, s: &str, n: &str) -> String {
    format!("c:{c}/pinned/d:{db}/s:{s}/r:{n}")
}

impl SchemaTree {
    pub fn new(store: Entity<ConnStore>, cx: &mut Context<Self>) -> Self {
        cx.observe(&store, |t, _, cx| {
            t.load_connected(cx);
            t.rebuild(cx);
            cx.notify();
        })
        .detach();
        let filter = cx.new(|cx| TextInput::new(cx, "Search").bare());
        cx.subscribe(&filter, |t: &mut SchemaTree, _, ev: &InputEvent, cx| match ev {
            InputEvent::Changed => {
                t.rebuild(cx);
                if t.filtering(cx)
                    && let Some(r) = t.rows.iter().find(|r| r.hl.is_some())
                {
                    t.selected = Some(r.id.clone());
                }
                t.pop_wait = Some(cx.spawn(async move |t, cx| {
                    cx.background_executor().timer(POP_PAUSE).await;
                    let _ = t.update(cx, |t, cx| t.pop_first_match(cx));
                }));
                cx.notify();
            }
            InputEvent::Submit => {
                if let Some(ix) = t.selected_ix() {
                    t.open(ix, cx);
                }
            }
        })
        .detach();
        let mut t = SchemaTree {
            store,
            focus: cx.focus_handle(),
            filter,
            rows: Vec::new(),
            expanded: HashSet::new(),
            folder_turn: None,
            loading: HashSet::new(),
            errors: Default::default(),
            hovered: None,
            selected: None,
            scroll: UniformListScrollHandle::new(),
            filter_stats: None,
            menu: None,
            menu_focus: cx.focus_handle(),
            _menu_blur: None,
            menu_closed: None,
            seen_version: 0,
            reread: HashMap::new(),
            last_rebuild: Default::default(),
            pop: None,
            pop_now: None,
            pop_wait: None,
            popped: None,
            reveal: None,
            conn_drop: None,
            shut_folders: HashSet::new(),
            scope: None,
            scope_menu: false,
            naming: None,
            focus_back: false,
            conn_bounds: Default::default(),
        };
        t.rebuild(cx);
        t
    }

    pub fn reveal_relation(&mut self, conn: &str, db: &str, schema: &str, name: &str, kind: RelKind, column: Option<&str>, cx: &mut Context<Self>) {
        let g = if kind == RelKind::View {
            G::Views
        } else if kind == RelKind::MaterializedView {
            G::MatViews
        } else {
            G::Tables
        };
        let rel = rid(conn, db, schema, name);
        let mut path = self.open_schema(conn, db, schema, cx);
        path.push((gid(conn, db, schema, g), Kind::Group { conn: conn.into(), db: db.into(), schema: schema.into(), g }));
        let target = match column {
            Some(c) => {
                path.push((rel.clone(), Kind::Rel { conn: conn.into(), db: db.into(), schema: schema.into(), name: name.into(), kind }));
                format!("{rel}/cols/{c}")
            }
            None => rel,
        };
        self.reveal_path(path, target, cx);
    }

    pub fn reveal_function(&mut self, conn: &str, db: &str, schema: &str, oid: u32, cx: &mut Context<Self>) {
        let mut path = self.open_schema(conn, db, schema, cx);
        let g = gid(conn, db, schema, G::Functions);
        path.push((g.clone(), Kind::Group { conn: conn.into(), db: db.into(), schema: schema.into(), g: G::Functions }));
        self.reveal_path(path, format!("{g}/f:{oid}"), cx);
    }

    fn open_schema(&mut self, conn: &str, db: &str, schema: &str, cx: &mut Context<Self>) -> Vec<(String, Kind)> {
        if !self.filter.read(cx).text().is_empty() {
            self.filter.update(cx, |f, cx| f.set_text("", cx));
        }
        vec![
            (cid(conn), Kind::Conn { id: conn.into() }),
            (did(conn, db), Kind::Db { conn: conn.into(), db: db.into() }),
            (sid(conn, db, schema), Kind::Schema { conn: conn.into(), db: db.into(), schema: schema.into() }),
        ]
    }

    fn reveal_path(&mut self, path: Vec<(String, Kind)>, target: String, cx: &mut Context<Self>) {
        for (id, kind) in path {
            if self.expanded.insert(id.clone()) || !self.loading.contains(&id) {
                self.load(&id, &kind, cx);
            }
        }
        self.reveal = Some(target);
        self.rebuild(cx);
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub fn selected_id(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    fn try_reveal(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.reveal.clone() else { return };
        if let Some(ix) = self.rows.iter().position(|r| r.id.as_ref() == id) {
            self.reveal = None;
            self.select(ix, cx);
        }
    }

    #[cfg(feature = "checks")]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    #[cfg(feature = "checks")]
    pub fn scroll_handle(&self) -> &UniformListScrollHandle {
        &self.scroll
    }

    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx) || self.filter.focus_handle(cx).is_focused(window)
    }

    pub fn selected_target(&self, cx: &App) -> Option<Target> {
        let row = self.rows.get(self.selected_ix()?)?;
        self.target_for(row, cx)
    }

    pub fn selected_connection(&self) -> Option<String> {
        let sel = self.selected.as_ref()?;
        let rest = sel.strip_prefix("c:")?;
        Some(rest.split('/').next()?.to_string())
    }

    fn selected_ix(&self) -> Option<usize> {
        let s = self.selected.as_ref()?;
        self.rows.iter().position(|r| &r.id == s)
    }

    fn filtering(&self, cx: &App) -> bool {
        !self.filter.read(cx).text().trim().is_empty()
    }

    fn conn(&self, id: &str, cx: &App) -> Option<Arc<Connection>> {
        self.store.read(cx).connection(id)
    }

    fn version(&self, cx: &App) -> u64 {
        self.store
            .read(cx)
            .states
            .values()
            .map(|s| match s {
                ConnState::Connected(c) => c.schema.version(),
                _ => 0,
            })
            .sum::<u64>()
            + self.store.read(cx).states.len() as u64
    }

    pub fn rebuild(&mut self, cx: &mut Context<Self>) {
        let t0 = std::time::Instant::now();
        // A folder gone (deleted here, by an undo or in the file) takes the scope with it.
        if self.scope.as_ref().is_some_and(|s| !self.store.read(cx).folders.iter().any(|d| d.id == *s)) {
            self.scope = None;
            self.scope_menu = false;
        }
        self.seen_version = self.version(cx);
        let theme = cx.theme();
        let q = self.filter.read(cx).text().trim().to_lowercase();
        let mut rows = Vec::with_capacity(self.rows.len().max(16));
        if q.is_empty() {
            self.filter_stats = None;
            self.build_tree(&mut rows, &theme, cx);
        } else {
            self.build_filtered(&mut rows, &q, &theme, cx);
        }
        self.rows = rows;
        self.last_rebuild = t0.elapsed();
        self.try_reveal(cx);
        self.reread_dropped(cx);
    }

    /// A statement that changes the schema (CREATE / DROP / ALTER in a tab) drops the database's
    /// cache, and only what completion needs is read back: an open database or schema left
    /// unread would stay empty. Read each once more.
    fn reread_dropped(&mut self, cx: &mut Context<Self>) {
        let mut todo = Vec::new();
        for r in &self.rows {
            if !r.expanded || self.loading.contains(r.id.as_ref()) || self.errors.contains_key(r.id.as_ref()) {
                continue;
            }
            let dropped = match &r.kind {
                Kind::Db { conn, db } => self.conn(conn, cx).is_some_and(|c| if c.caps().schemas { c.schema.cached_schemas(db).is_none() } else { c.schema.cached_relations(db, db).is_none() }),
                Kind::Schema { conn, db, schema } => self.conn(conn, cx).is_some_and(|c| c.schema.cached_relations(db, schema).is_none()),
                Kind::Group { conn, db, schema, g } => self.conn(conn, cx).is_some_and(|c| {
                    let k = &c.schema;
                    match g {
                        G::Functions => k.cached_functions(db, schema).is_none(),
                        G::Sequences => k.cached_sequences(db, schema).is_none(),
                        G::Types => k.cached_types(db, schema).is_none(),
                        G::Triggers => k.cached_triggers(db, schema).is_none(),
                        G::Tables | G::Views | G::MatViews => false,
                    }
                }),
                _ => false,
            };
            if dropped && self.reread.get(r.id.as_ref()) != Some(&self.seen_version) {
                todo.push((r.id.to_string(), r.kind.clone()));
            }
        }
        for (id, kind) in todo {
            self.reread.insert(id.clone(), self.seen_version);
            self.load(&id, &kind, cx);
        }
    }

    fn conn_row(&self, p: &dataman_core::ConnectionProfile, state: Option<&ConnState>, open: bool, t: &Theme) -> Row {
        let lar = matches!(state, Some(ConnState::Connected(c)) if c.info.is_lar);
        let (glyph, meta) = match state {
            Some(ConnState::Connecting) => ("◐", Meta::Warn),
            Some(ConnState::Connected(_)) if lar => ("●", Meta::LarState),
            Some(ConnState::Connected(_)) => ("●", Meta::Ok),
            Some(ConnState::Failed(_)) => ("✕", Meta::Err),
            None => ("○", Meta::Off),
        };
        let mut r = Row::new(cid(&p.id), 0, p.name.clone(), Kind::Conn { id: p.id.clone() })
            .tone(match state {
                Some(ConnState::Connected(_)) if lar => Tone::ConnLar,
                Some(ConnState::Connected(_)) => Tone::Conn,
                Some(ConnState::Connecting) => Tone::ConnWait,
                _ => Tone::ConnOff,
            })
            .meta(glyph, meta)
            .expandable(open);
        r.chip = Some(t.profile_color(p));
        r.chip_dim = r.tone == Tone::ConnOff;
        if let Some(ConnState::Failed(e)) = state {
            r.tip = Some(e.clone().into());
        }
        r.lar = lar;
        r.tunnel = p.ssh.is_some();
        r.logo = Some(p.engine);
        r
    }

    /// The folders first (each with its connections, one level in), then the connections in
    /// none; with a scope, that folder alone.
    fn build_tree(&self, rows: &mut Vec<Row>, t: &Theme, cx: &App) {
        let store = self.store.read(cx);
        for d in store.folders.iter().filter(|d| self.scope.as_ref().is_none_or(|s| *s == d.id)) {
            let open = !self.shut_folders.contains(&d.id);
            let n = store.profiles.iter().filter(|p| d.holds(&p.id)).count();
            rows.push(self.folder_row(d, n, open, t));
            if !open {
                continue;
            }
            for p in store.profiles.iter().filter(|p| d.holds(&p.id)) {
                let start = rows.len();
                self.push_conn_block(rows, p, t, cx);
                for r in &mut rows[start..] {
                    r.depth += 1;
                    r.in_folder = Some(d.id.clone().into());
                }
            }
        }
        if self.scope.is_some() {
            return;
        }
        for p in store.profiles.iter().filter(|p| store.folder_of(&p.id).is_none()) {
            self.push_conn_block(rows, p, t, cx);
        }
    }

    fn folder_row(&self, d: &dataman_core::ConnFolder, n: usize, open: bool, t: &Theme) -> Row {
        let color = folder_color(d, t);
        let mut r = Row::new(fid(&d.id), 0, d.name.clone(), Kind::Folder { id: d.id.clone() }).tone(Tone::Folder).meta(n.to_string(), Meta::Plain).expandable(open);
        r.icon = Some((Icon::Schema, color));
        r.in_folder = Some(d.id.clone().into());
        r
    }

    fn push_conn_block(&self, rows: &mut Vec<Row>, p: &dataman_core::ConnectionProfile, t: &Theme, cx: &App) {
        let store = self.store.read(cx);
        let id = cid(&p.id);
        let open = self.expanded.contains(&id);
        let state = store.states.get(&p.id);
        rows.push(self.conn_row(p, state, open, t));
        if !open {
            return;
        }
        match state {
            Some(ConnState::Connected(c)) if !c.engine().is_sql() => self.push_docs(rows, &p.id, c, t),
            Some(ConnState::Connected(c)) => {
                self.push_pinned(rows, &p.id, c, &store.core.pins.of(&p.id), t);
                self.push_conn(rows, &p.id, c, t);
            }
            Some(ConnState::Failed(e)) => rows.push(error_row(&id, 1, e, &id, t)),
            _ => rows.push(loading_row(&id, 1)),
        }
    }

    fn build_filtered(&mut self, rows: &mut Vec<Row>, q: &str, t: &Theme, cx: &App) {
        let store = self.store.read(cx);
        let (mut matches, mut total) = (0usize, 0usize);
        let mut names: Vec<String> = Vec::new();
        let mut find = |name: &str, names: &mut Vec<String>| -> Option<Range<usize>> {
            total += 1;
            let m = match_range(name, q);
            if m.is_none() {
                names.push(name.to_string());
            }
            m
        };
        let scope = self.scope.as_ref().and_then(|s| store.folders.iter().find(|d| d.id == *s));
        for p in store.profiles.iter().filter(|p| scope.is_none_or(|d| d.holds(&p.id))) {
            let Some(ConnState::Connected(conn)) = store.states.get(&p.id) else { continue };
            if !conn.engine().is_sql() {
                let c = p.id.as_str();
                let mut block = Vec::new();
                let top = conn.cached_tree(None).unwrap_or_default();
                let mut hit_rows = |db: &str, items: &[dataman_core::nosql::Item], depth: usize, block: &mut Vec<Row>| -> usize {
                    let mut n = 0;
                    for x in items.iter().filter(|i| i.kind != dataman_core::nosql::ItemKind::Database) {
                        if let Some(m) = find(&x.name, &mut names) {
                            let mut row = coll_row(c, db, x, depth);
                            row.hl = Some(m);
                            block.push(row);
                            n += 1;
                        }
                    }
                    n
                };
                matches += hit_rows(conn.database(), &top, 1, &mut block);
                for db in top.iter().filter(|i| i.kind == dataman_core::nosql::ItemKind::Database) {
                    let Some(items) = conn.cached_tree(Some(&db.name)) else { continue };
                    let mut db_block = Vec::new();
                    let n = hit_rows(&db.name, &items, 2, &mut db_block);
                    if n > 0 {
                        matches += n;
                        let label = if conn.engine() == dataman_core::Engine::Redis { format!("db{}", db.name) } else { db.name.clone() };
                        block.push(Row::new(did(c, &db.name), 1, label, Kind::DocDb { conn: c.into(), db: db.name.clone() }).icon(Icon::Database).expandable(true));
                        block.extend(db_block);
                    }
                }
                if !block.is_empty() {
                    rows.push(self.conn_row(p, store.states.get(&p.id), true, t));
                    rows.extend(block);
                }
                continue;
            }
            let cache = &conn.schema;
            let c = p.id.as_str();
            let flat = !conn.caps().schemas;
            let dd = usize::from(flat);
            let pg = conn.dialect() == dataman_core::sql::Dialect::Postgres;
            let mut block = Vec::new();
            let pinned: Vec<Row> = store
                .core
                .pins
                .of(c)
                .iter()
                .filter_map(|x| {
                    let m = match_range(&x.name, q)?;
                    let mut row = pin_row(c, conn, x, 2, false);
                    row.hl = Some(m);
                    Some(row)
                })
                .collect();
            if !pinned.is_empty() {
                matches += pinned.len();
                block.push(group_row(&pinned_id(c), 1, "pinned", pinned.len(), true, Kind::Pinned { conn: c.into() }).icon(Icon::Pin));
                block.extend(pinned);
            }
            for db in cache.cached_databases().unwrap_or_else(|| vec![conn.database().to_string()]) {
                let mut db_block = Vec::new();
                for s in cache.cached_schemas(&db).unwrap_or_default() {
                    let mut hits = Vec::new();
                    let mut hit = |row: Row, range: Option<Range<usize>>| {
                        if let Some(r) = range {
                            let mut row = row;
                            row.hl = Some(r);
                            hits.push(row);
                        }
                    };
                    if let Some(rels) = cache.cached_relations(&db, &s) {
                        let stats = cache.cached_stats(&db, &s);
                        for r in &rels {
                            let m = find(&r.name, &mut names);
                            if m.is_some() {
                                hit(rel_row(c, &db, &s, r, 3 - dd, false, stats.as_deref()), m);
                            }
                        }
                    }
                    for f in cache.cached_functions(&db, &s).iter().flat_map(|v| v.iter()) {
                        let m = find(&f.name, &mut names);
                        if m.is_some() {
                            hit(function_row(c, &db, &s, f, 3 - dd, pg), m);
                        }
                    }
                    for x in cache.cached_sequences(&db, &s).iter().flat_map(|v| v.iter()) {
                        let m = find(&x.name, &mut names);
                        if m.is_some() {
                            hit(sequence_row(c, &db, &s, x, 3 - dd, pg), m);
                        }
                    }
                    for x in cache.cached_types(&db, &s).iter().flat_map(|v| v.iter()) {
                        let m = find(&x.name, &mut names);
                        if m.is_some() {
                            hit(type_row(c, &db, &s, x, 3 - dd, false, pg), m);
                        }
                    }
                    for x in cache.cached_triggers(&db, &s).iter().flat_map(|v| v.iter()) {
                        let m = find(&x.name, &mut names);
                        if m.is_some() {
                            let id = format!("{}/g:{}/{}", gid(c, &db, &s, G::Triggers), x.table, x.name);
                            hit(trigger_row(id, c, &db, &s, x, 3 - dd, true), m);
                        }
                    }
                    if !hits.is_empty() {
                        matches += hits.len();
                        if !flat {
                            db_block.push(Row::new(sid(c, &db, &s), 2, s.clone(), Kind::Schema { conn: c.into(), db: db.clone(), schema: s.clone() }).icon(Icon::Schema).expandable(true));
                        }
                        db_block.extend(hits);
                    }
                }
                for x in cache.cached_extensions(&db).unwrap_or_default() {
                    let m = find(&x.name, &mut names);
                    if let Some(m) = m {
                        let mut row = extension_row(c, &db, &x, 2);
                        row.hl = Some(m);
                        matches += 1;
                        db_block.push(row);
                    }
                }
                if !db_block.is_empty() {
                    let ro = store.connection(c).is_some_and(|x| x.read_only_for(&db));
                    block.push(Row::new(did(c, &db), 1, db.clone(), Kind::Db { conn: c.into(), db: db.clone() }).icon(Icon::Database).lock(ro).expandable(true));
                    block.extend(db_block);
                }
            }
            for r in cache.cached_roles().unwrap_or_default() {
                let m = find(&r.name, &mut names);
                if let Some(m) = m {
                    let mut row = role_row(c, &r, 1);
                    row.hl = Some(m);
                    matches += 1;
                    block.push(row);
                }
            }
            if !block.is_empty() {
                rows.push(self.conn_row(p, store.states.get(&p.id), true, t));
                rows.extend(block);
            }
        }
        let near = (matches == 0).then(|| nearest(q, &names)).flatten();
        self.filter_stats = Some((matches, total, near));
    }

    fn push_status(&self, rows: &mut Vec<Row>, id: &str, depth: usize, t: &Theme) -> bool {
        if self.loading.contains(id) {
            rows.push(loading_row(id, depth));
            return true;
        }
        if let Some(e) = self.errors.get(id) {
            rows.push(error_row(id, depth, e, id, t));
            return true;
        }
        false
    }

    fn push_pinned(&self, rows: &mut Vec<Row>, c: &str, conn: &Arc<Connection>, pins: &[Pin], t: &Theme) {
        if pins.is_empty() {
            return;
        }
        let g = pinned_id(c);
        let open = !self.expanded.contains(&g);
        rows.push(group_row(&g, 1, "pinned", pins.len(), open, Kind::Pinned { conn: c.into() }).icon(Icon::Pin));
        if !open {
            return;
        }
        for p in pins {
            let id = pin_id(c, &p.database, &p.schema, &p.name);
            let open = self.expanded.contains(&id);
            let row = pin_row(c, conn, p, 2, open);
            let found = row.expandable;
            rows.push(row);
            if open && found {
                self.push_rel(rows, &id, c, &p.database, &p.schema, &p.name, conn, t, 3);
            }
        }
    }

    fn push_docs(&self, rows: &mut Vec<Row>, c: &str, conn: &Arc<Connection>, t: &Theme) {
        let id = cid(c);
        let Some(items) = conn.cached_tree(None) else {
            if !self.push_status(rows, &id, 1, t) {
                rows.push(loading_row(&id, 1));
            }
            return;
        };
        for it in items.iter() {
            match it.kind {
                dataman_core::nosql::ItemKind::Database => {
                    let d = did(c, &it.name);
                    let open = self.expanded.contains(&d);
                    let label = if conn.engine() == dataman_core::Engine::Redis { format!("db{}", it.name) } else { it.name.clone() };
                    let mut row = Row::new(d.clone(), 1, label, Kind::DocDb { conn: c.into(), db: it.name.clone() })
                        .icon(Icon::Database)
                        .lock(conn.read_only_for(&it.name))
                        .expandable(open);
                    if let Some(s) = it.size.filter(|s| *s > 0) {
                        row = row.meta(human_size(s as i64), Meta::Plain);
                    } else if let Some(n) = it.count.filter(|_| conn.engine() == dataman_core::Engine::Redis) {
                        row = row.meta(format!("{} keys", approx_count(n as f64)), Meta::Plain);
                    }
                    rows.push(row);
                    if !open || self.push_status(rows, &d, 2, t) {
                        continue;
                    }
                    match conn.cached_tree(Some(&it.name)) {
                        Some(colls) => rows.extend(colls.iter().map(|x| coll_row(c, &it.name, x, 2))),
                        None => rows.push(loading_row(&d, 2)),
                    }
                }
                _ => rows.push(coll_row(c, conn.database(), it, 1)),
            }
        }
    }

    fn push_conn(&self, rows: &mut Vec<Row>, c: &str, conn: &Arc<Connection>, t: &Theme) {
        let id = cid(c);
        let cache = &conn.schema;
        let Some(dbs) = cache.cached_databases() else {
            if !self.push_status(rows, &id, 1, t) {
                rows.push(loading_row(&id, 1));
            }
            return;
        };
        for db in dbs {
            let d = did(c, &db);
            let open = self.expanded.contains(&d);
            rows.push(
                Row::new(d.clone(), 1, db.clone(), Kind::Db { conn: c.into(), db: db.clone() })
                    .icon(Icon::Database)
                    .lock(conn.read_only_for(&db))
                    .expandable(open),
            );
            if !open || self.push_status(rows, &d, 2, t) {
                continue;
            }
            if !conn.caps().schemas {
                self.push_schema(rows, c, &db, &db, conn, t, 2);
                continue;
            }
            let Some(schemas) = cache.cached_schemas(&db) else { continue };
            for s in schemas {
                let sidv = sid(c, &db, &s);
                let open = self.expanded.contains(&sidv);
                rows.push(
                    Row::new(sidv.clone(), 2, s.clone(), Kind::Schema { conn: c.into(), db: db.clone(), schema: s.clone() })
                        .icon(Icon::Schema)
                        .expandable(open),
                );
                if !open || self.push_status(rows, &sidv, 3, t) {
                    continue;
                }
                self.push_schema(rows, c, &db, &s, conn, t, 3);
            }
            if let Some(exts) = cache.cached_extensions(&db).filter(|v| !v.is_empty()) {
                let g = format!("{d}/extensions");
                let open = self.expanded.contains(&g);
                rows.push(group_row(&g, 2, "extensions", exts.len(), open, Kind::Extensions { conn: c.into(), db: db.clone() }));
                if open {
                    rows.extend(exts.iter().map(|x| extension_row(c, &db, x, 3)));
                }
            }
        }
        if let Some(roles) = cache.cached_roles().filter(|v| !v.is_empty()) {
            let g = format!("{id}/roles");
            let open = self.expanded.contains(&g);
            rows.push(group_row(&g, 1, "roles", roles.len(), open, Kind::Roles { conn: c.into() }));
            if open {
                rows.extend(roles.iter().map(|r| role_row(c, r, 2)));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_schema(&self, rows: &mut Vec<Row>, c: &str, db: &str, s: &str, conn: &Arc<Connection>, t: &Theme, d0: usize) {
        let cache = &conn.schema;
        let Some(rels) = cache.cached_relations(db, s) else {
            rows.push(loading_row(&sid(c, db, s), d0));
            return;
        };
        let counts = cache.cached_counts(db, s).unwrap_or_default();
        let functions = cache.cached_functions(db, s);
        let sequences = cache.cached_sequences(db, s);
        let types = cache.cached_types(db, s);
        let triggers = cache.cached_triggers(db, s);
        let pg = conn.dialect() == dataman_core::sql::Dialect::Postgres;
        let start = rows.len();
        for g in G::ALL {
            let n = match g {
                G::Tables | G::Views | G::MatViews => rels.iter().filter(|r| g.holds(r.kind) == Some(true)).count() as u64,
                G::Functions => functions.as_ref().map(|v| v.len() as u64).or(counts.functions).unwrap_or(0),
                G::Sequences => sequences.as_ref().map(|v| v.len() as u64).or(counts.sequences).unwrap_or(0),
                G::Types => types.as_ref().map(|v| v.len() as u64).or(counts.types).unwrap_or(0),
                G::Triggers => triggers.as_ref().map(|v| v.len() as u64).or(counts.triggers).unwrap_or(0),
            };
            if n == 0 {
                continue;
            }
            let gv = gid(c, db, s, g);
            let open = self.expanded.contains(&gv);
            rows.push(group_row(&gv, d0, g.label(), n as usize, open, Kind::Group { conn: c.into(), db: db.into(), schema: s.into(), g }));
            if !open {
                continue;
            }
            if g.holds(RelKind::Table).is_some() {
                let stats = cache.cached_stats(db, s);
                for r in rels.iter().filter(|r| g.holds(r.kind) == Some(true)) {
                    let rv = rid(c, db, s, &r.name);
                    let open = self.expanded.contains(&rv);
                    rows.push(rel_row(c, db, s, r, d0 + 1, open, stats.as_deref()));
                    if open {
                        self.push_rel(rows, &rv, c, db, s, &r.name, conn, t, d0 + 2);
                    }
                }
                continue;
            }
            if self.push_status(rows, &gv, d0 + 1, t) {
                continue;
            }
            match g {
                G::Functions => rows.extend(functions.iter().flat_map(|v| v.iter()).map(|f| function_row(c, db, s, f, d0 + 1, pg))),
                G::Sequences => rows.extend(sequences.iter().flat_map(|v| v.iter()).map(|x| sequence_row(c, db, s, x, d0 + 1, pg))),
                G::Types => {
                    for x in types.iter().flat_map(|v| v.iter()) {
                        let id = type_id(c, db, s, &x.name);
                        let open = self.expanded.contains(&id);
                        rows.push(type_row(c, db, s, x, d0 + 1, open, pg));
                        if open {
                            rows.extend(x.labels.iter().enumerate().map(|(i, l)| {
                                let what = Obj::EnumLabel;
                                Row::new(format!("{id}/l:{i}"), d0 + 2, l.clone(), Kind::Object { conn: c.into(), db: db.into(), schema: s.into(), name: l.clone(), what })
                                    .icon(Icon::Label)
                            }));
                        }
                    }
                }
                G::Triggers => rows.extend(triggers.iter().flat_map(|v| v.iter()).map(|x| {
                    let id = format!("{gv}/g:{}/{}", x.table, x.name);
                    trigger_row(id, c, db, s, x, d0 + 1, true)
                })),
                _ => {}
            }
        }
        if rows.len() == start {
            rows.push(Row::new(format!("{}/none", sid(c, db, s)), d0, "empty", Kind::Leaf).tone(Tone::Italic));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_rel(&self, rows: &mut Vec<Row>, base: &str, c: &str, db: &str, s: &str, name: &str, conn: &Arc<Connection>, t: &Theme, d: usize) {
        let cols_id = format!("{base}/cols");
        let idx_id = format!("{base}/idx");
        let lar = conn.info.is_lar;
        // Autocomplete's brief columns have no keys: wait for the full list (`columns`).
        let cols = conn.schema.cached_full_columns(db, s, name);
        let open = self.expanded.contains(&cols_id);
        rows.push(
            Row::new(cols_id.clone(), d, "columns", Kind::Columns { conn: c.into(), db: db.into(), schema: s.into(), name: name.into() })
                .icon(Icon::Group)
                .meta(cols.as_ref().map(|c| c.len().to_string()).unwrap_or_default(), Meta::Plain)
                .expandable(open),
        );
        if open && !self.push_status(rows, &cols_id, d + 1, t) {
            let cols = cols.unwrap_or_default();
            for col in &cols {
                let mut meta = short_type(&col.type_name, conn.dialect() == dataman_core::sql::Dialect::Postgres);
                if col.primary_key {
                    meta.push_str(" PK");
                } else if col.nullable == Some(false) {
                    meta.push_str(" !");
                }
                let row = Row::new(format!("{cols_id}/{}", col.name), d + 1, col.name.clone(), Kind::Column { name: col.name.clone() })
                    .meta(meta, Meta::Plain);
                rows.push(if col.primary_key { row.icon_colored(Icon::Key, t.num) } else { row.blank_icon() });
            }
        }
        let idx = conn.schema.cached_indexes(db, s, name);
        let open = self.expanded.contains(&idx_id);
        let gap = lar && idx.as_ref().is_some_and(|i| i.is_empty());
        let mut row = Row::new(idx_id.clone(), d, "indexes", Kind::Indexes { conn: c.into(), db: db.into(), schema: s.into(), name: name.into() })
            .icon(Icon::Group)
            .expandable(open && !gap);
        row = if gap {
            row.meta("not reported", Meta::NotReported)
        } else {
            row.meta(idx.as_ref().map(|c| c.len().to_string()).unwrap_or_default(), Meta::Plain)
        };
        rows.push(row);
        if open && !gap && !self.push_status(rows, &idx_id, d + 1, t) {
            let idx = idx.unwrap_or_default();
            if idx.is_empty() {
                rows.push(Row::new(format!("{idx_id}/none"), d + 1, "no indexes", Kind::Leaf).tone(Tone::Italic));
            }
            for i in idx {
                let meta = if i.primary {
                    "primary".to_string()
                } else if i.unique {
                    format!("unique ({})", i.columns.join(", "))
                } else {
                    format!("({})", i.columns.join(", "))
                };
                let kind = Kind::Index { name: i.name.clone(), definition: i.definition.clone() };
                let row = Row::new(format!("{idx_id}/{}", i.name), d + 1, i.name.clone(), kind).meta(meta, Meta::Plain);
                rows.push(if i.primary { row.icon_colored(Icon::Index, t.num) } else { row.icon(Icon::Index) });
            }
        }
        let Some(x) = conn.schema.cached_extras(db, s, name) else { return };
        let sub = |what: Sub| Kind::TableSub { conn: c.into(), db: db.into(), schema: s.into(), name: name.into(), what };
        if let Some(cons) = x.constraints.filter(|v| !v.is_empty()) {
            let g = format!("{base}/cons");
            let open = self.expanded.contains(&g);
            rows.push(group_row(&g, d, "constraints", cons.len(), open, sub(Sub::Constraints)));
            if open {
                for k in cons {
                    let what = Obj::Constraint { table: name.into(), definition: k.definition.clone() };
                    let row = Row::new(format!("{g}/{}", k.name), d + 1, k.name.clone(), Kind::Object { conn: c.into(), db: db.into(), schema: s.into(), name: k.name.clone(), what })
                        .meta(clip(&constraint_summary(k.kind, k.definition.as_deref()), 36), Meta::Plain);
                    rows.push(match k.kind {
                        ConstraintKind::Primary => row.icon_colored(Icon::Key, t.num),
                        ConstraintKind::Foreign => row.icon(Icon::Link),
                        _ => row.icon(Icon::Constraint),
                    });
                }
            }
        }
        if let Some(trg) = x.triggers.filter(|v| !v.is_empty()) {
            let g = format!("{base}/trg");
            let open = self.expanded.contains(&g);
            rows.push(group_row(&g, d, "triggers", trg.len(), open, sub(Sub::Triggers)));
            if open {
                rows.extend(trg.iter().map(|tr| trigger_row(format!("{g}/{}", tr.name), c, db, s, tr, d + 1, false)));
            }
        }
        if let Some(parts) = x.partitions.filter(|v| !v.is_empty()) {
            let g = format!("{base}/parts");
            let open = self.expanded.contains(&g);
            let partitioned = conn.schema.cached_relations(db, s).is_some_and(|v| v.iter().any(|r| r.name == name && r.kind == RelKind::Partitioned));
            rows.push(group_row(&g, d, if partitioned { "partitions" } else { "children" }, parts.len(), open, sub(Sub::Partitions)));
            if open {
                for p in parts {
                    let label = if p.schema == s || p.schema.is_empty() { p.name.clone() } else { format!("{}.{}", p.schema, p.name) };
                    let kind = Kind::RelRef { conn: c.into(), db: db.into(), schema: if p.schema.is_empty() { s.into() } else { p.schema.clone() }, name: p.name.clone() };
                    rows.push(
                        Row::new(format!("{g}/{}.{}", p.schema, p.name), d + 1, label, kind)
                            .icon(Icon::Table)
                            .meta(p.bound.as_deref().map(|b| clip(&partition_bound_summary(b), 36)).unwrap_or_default(), Meta::Plain),
                    );
                }
            }
        }
        if let Some((ps, pn)) = x.parent {
            let label = if ps == s { pn.clone() } else { format!("{ps}.{pn}") };
            let kind = Kind::RelRef { conn: c.into(), db: db.into(), schema: ps, name: pn };
            rows.push(Row::new(format!("{base}/parent"), d, label, kind).icon(Icon::Partition).meta("parent", Meta::Plain));
        }
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix) else { return };
        self.selected = Some(row.id.clone());
        self.scroll.scroll_to_item(ix, gpui::ScrollStrategy::Nearest);
        if let Some(c) = self.selected_connection() {
            cx.emit(TreeEvent::Selected(c));
        }
        cx.notify();
    }

    pub fn toggle(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix).cloned() else { return };
        self.select(ix, cx);
        if let Kind::Error { retry } = &row.kind {
            self.retry(retry.clone(), cx);
            return;
        }
        if !row.expandable {
            return;
        }
        let id = row.id.to_string();
        if let Kind::Folder { id: f } = &row.kind {
            let opening = !row.expanded;
            if opening {
                self.shut_folders.remove(f);
            } else {
                self.shut_folders.insert(f.clone());
            }
            if !cx.reduce_motion() {
                self.folder_turn = Some((id, Instant::now(), opening));
            }
            self.rebuild(cx);
            cx.notify();
            return;
        }
        let opening = !self.expanded.remove(&id);
        if opening {
            self.expanded.insert(id.clone());
            self.load(&id, &row.kind, cx);
        }
        if matches!(row.icon, Some((Icon::Schema, _))) && !cx.reduce_motion() {
            self.folder_turn = Some((id, Instant::now(), opening));
        }
        self.rebuild(cx);
        cx.notify();
    }

    fn set_expanded(&mut self, ix: usize, open: bool, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix).cloned() else { return };
        if row.expandable && row.expanded != open {
            self.toggle(ix, cx);
        } else if !open {
            if let Some(p) = (0..ix).rev().find(|&i| self.rows[i].depth < row.depth) {
                self.select(p, cx);
            }
        } else if open && row.expanded {
            self.select((ix + 1).min(self.rows.len().saturating_sub(1)), cx);
        }
    }

    fn retry(&mut self, id: String, cx: &mut Context<Self>) {
        self.errors.remove(&id);
        if let Some(row) = self.rows.iter().find(|r| r.id.as_ref() == id).cloned() {
            if let Kind::Conn { id: c } = &row.kind {
                self.store.update(cx, |s, cx| {
                    s.disconnect(c, cx);
                    s.connect(c, None, cx);
                });
            } else {
                self.load(&id, &row.kind, cx);
            }
        }
        self.rebuild(cx);
        cx.notify();
    }

    /// Without this a node left open while its connection came up stayed on "loading…" until it
    /// was collapsed and opened again.
    fn load_connected(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self.store.read(cx).profiles.iter().map(|p| p.id.clone()).collect();
        for c in ids {
            let id = cid(&c);
            if !self.expanded.contains(&id) || self.loading.contains(&id) || self.errors.contains_key(&id) {
                continue;
            }
            let unread = |conn: &Arc<Connection>| if conn.engine().is_sql() { conn.schema.cached_databases().is_none() } else { conn.cached_tree(None).is_none() };
            if self.conn(&c, cx).is_some_and(|conn| unread(&conn)) {
                self.load(&id, &Kind::Conn { id: c.clone() }, cx);
            }
        }
    }

    pub fn expand_id(&mut self, id: &str, cx: &mut Context<Self>) -> bool {
        let Some(ix) = self.rows.iter().position(|r| r.id.as_ref() == id) else {
            if id.starts_with("c:") && !id.contains('/') {
                self.expanded.insert(id.to_string());
                self.load_connected(cx);
                self.rebuild(cx);
                cx.notify();
            }
            return false;
        };
        if !self.rows[ix].expanded {
            self.toggle(ix, cx);
        }
        true
    }

    #[cfg(feature = "checks")]
    pub fn has_row(&self, id: &str) -> bool {
        self.rows.iter().any(|r| r.id.as_ref() == id)
    }

    #[cfg(feature = "checks")]
    pub fn collapse_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(ix) = self.rows.iter().position(|r| r.id.as_ref() == id)
            && self.rows[ix].expanded
        {
            self.toggle(ix, cx);
        }
    }

    #[cfg(feature = "checks")]
    pub fn select_id(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(ix) = self.rows.iter().position(|r| r.id.as_ref() == id) {
            self.select(ix, cx);
        }
    }

    #[cfg(feature = "checks")]
    pub fn refresh_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        self.refresh_conn(id.to_string(), cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_turn_schema(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(ix) = self.rows.iter().position(|r| matches!(r.icon, Some((Icon::Schema, _)))) else { return false };
        self.toggle(ix, cx);
        self.folder_turn.is_some()
    }

    #[cfg(feature = "checks")]
    pub fn demo_folder_turning(&self) -> bool {
        self.folder_turn.as_ref().is_some_and(|(_, at, opening)| crate::motion::folder_fill(*at, *opening).is_some())
    }

    #[cfg(feature = "checks")]
    pub fn demo_focus_filter(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.filter.focus_handle(cx), cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_filter(&self) -> Entity<TextInput> {
        self.filter.clone()
    }

    #[cfg(feature = "checks")]
    pub fn set_filter(&mut self, text: &str, cx: &mut Context<Self>) {
        self.filter.update(cx, |f, cx| f.set_text(text, cx));
        self.rebuild(cx);
        if let Some(r) = self.rows.iter().find(|r| r.hl.is_some()) {
            self.selected = Some(r.id.clone());
        }
        cx.notify();
    }

    pub fn pop_first_match(&mut self, cx: &mut Context<Self>) {
        self.pop_wait = None;
        if !self.filtering(cx) {
            self.popped = None;
            self.pop = None;
            return;
        }
        let Some((ix, r)) = self.rows.iter().enumerate().find(|(_, r)| r.hl.is_some()) else { return };
        let hit = (r.id.clone(), r.hl.clone().unwrap_or_default());
        if self.popped.as_ref() == Some(&hit) {
            return;
        }
        self.pop = FindPop::start(hit.0.clone(), PopStyle::Find, cx);
        self.popped = Some(hit);
        self.scroll.scroll_to_item(ix, gpui::ScrollStrategy::Nearest);
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub fn demo_conn_label(&self, conn_id: &str, cx: &App) -> Option<(Hsla, bool)> {
        let id = cid(conn_id);
        let r = self.rows.iter().find(|r| r.id.as_ref() == id)?;
        Some(label_style(r.tone, false, &cx.theme()))
    }

    #[cfg(feature = "checks")]
    pub fn demo_conn_row(&self, conn_id: &str) -> Option<(String, bool, Option<String>, Option<(String, Option<String>)>)> {
        let id = cid(conn_id);
        let r = self.rows.iter().find(|r| r.id.as_ref() == id)?;
        let err = self.rows.iter().find(|r| r.id.as_ref() == format!("{id}/err")).map(|e| (e.label.to_string(), e.tip.as_ref().map(|t| t.to_string())));
        Some((r.label.to_string(), r.chip_dim, r.tip.as_ref().map(|t| t.to_string()), err))
    }

    #[cfg(feature = "checks")]
    pub fn demo_logo(&self, conn_id: &str) -> Option<dataman_core::Engine> {
        let id = cid(conn_id);
        self.rows.iter().find(|r| r.id.as_ref() == id)?.logo
    }

    #[cfg(feature = "checks")]
    pub fn demo_row_ids(&self, id: &str) -> Vec<String> {
        let prefix = format!("{id}/");
        self.rows.iter().filter(|r| r.id.starts_with(&prefix)).map(|r| r.id.to_string()).collect()
    }

    #[cfg(feature = "checks")]
    pub fn demo_children(&self, id: &str) -> Vec<String> {
        let prefix = format!("{id}/");
        self.rows.iter().filter(|r| r.id.starts_with(&prefix)).map(|r| r.label.to_string()).collect()
    }

    #[cfg(feature = "checks")]
    pub fn demo_conn_bounds(&self, conn_id: &str) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.conn_bounds.borrow().get(cid(conn_id).as_str()).copied()
    }

    #[cfg(feature = "checks")]
    pub fn demo_pop(&self) -> Option<SharedString> {
        self.pop.as_ref().filter(|p| p.frame(Instant::now()).is_some()).map(|p| p.target.clone())
    }

    fn clear_filter(&mut self, cx: &mut Context<Self>) {
        if self.filtering(cx) {
            self.filter.update(cx, |f, cx| f.set_text("", cx));
            self.rebuild(cx);
            self.popped = None;
            self.pop = None;
            cx.notify();
        } else {
            cx.emit(TreeEvent::FocusEditor);
        }
    }

    fn load(&mut self, id: &str, kind: &Kind, cx: &mut Context<Self>) {
        let task: Option<Load> = match kind {
            Kind::Conn { id: c } if self.conn(c, cx).is_some_and(|x| !x.engine().is_sql()) => {
                let conn = self.conn(c, cx).expect("connected");
                let db = conn.database().to_string();
                let d = did(c, &db);
                self.expanded.insert(d.clone());
                self.load(&d, &Kind::DocDb { conn: c.clone(), db: db.clone() }, cx);
                Some(unit(conn.nosql_tree(None)))
            }
            Kind::DocDb { conn, db } => self.conn(conn, cx).map(|c| unit(c.nosql_tree(Some(db)))),
            Kind::Conn { id: c } => {
                let store = self.store.clone();
                match self.conn(c, cx) {
                    Some(conn) => {
                        let db = conn.database().to_string();
                        self.expanded.insert(did(c, &db));
                        let mut loads = vec![unit(conn.databases()), unit(conn.schemas(&db)), unit(conn.extensions(&db)), unit(conn.roles())];
                        if !conn.caps().schemas && !db.is_empty() {
                            loads.extend([unit(conn.relations(&db, &db)), unit(conn.object_counts(&db, &db))]);
                        }
                        Some(join_all(loads))
                    }
                    None => {
                        store.update(cx, |s, cx| s.connect(c, None, cx));
                        None
                    }
                }
            }
            Kind::Db { conn, db } => self.conn(conn, cx).map(|c| {
                if c.caps().schemas {
                    join2(c.schemas(db), c.extensions(db))
                } else {
                    join_all(vec![unit(c.schemas(db)), unit(c.relations(db, db)), unit(c.object_counts(db, db))])
                }
            }),
            Kind::Schema { conn, db, schema } => self.conn(conn, cx).map(|c| join2(c.relations(db, schema), c.object_counts(db, schema))),
            Kind::Group { conn, db, schema, g } => self.conn(conn, cx).and_then(|c| match g {
                G::Tables | G::MatViews => Some(unit(c.rel_stats(db, schema))),
                G::Views => None,
                G::Functions => Some(unit(c.functions(db, schema))),
                G::Sequences => Some(unit(c.sequences(db, schema))),
                G::Types => Some(unit(c.types(db, schema))),
                G::Triggers => Some(unit(c.triggers(db, schema))),
            }),
            Kind::Roles { conn } => self.conn(conn, cx).map(|c| unit(c.roles())),
            Kind::Extensions { conn, db } => self.conn(conn, cx).map(|c| unit(c.extensions(db))),
            Kind::Rel { conn, db, schema, name, .. } => {
                let cols = format!("{id}/cols");
                self.expanded.insert(cols);
                self.conn(conn, cx).map(|c| join_all(vec![unit(c.columns(db, schema, name)), unit(c.indexes(db, schema, name)), unit(c.table_extras(db, schema, name))]))
            }
            Kind::Columns { conn, db, schema, name } => self.conn(conn, cx).map(|c| unit(c.columns(db, schema, name))),
            Kind::Indexes { conn, db, schema, name } => self.conn(conn, cx).map(|c| unit(c.indexes(db, schema, name))),
            Kind::TableSub { conn, db, schema, name, .. } => self.conn(conn, cx).map(|c| unit(c.table_extras(db, schema, name))),
            Kind::Folder { .. } | Kind::Pinned { .. } | Kind::RelRef { .. } | Kind::Object { .. } | Kind::Column { .. } | Kind::Index { .. } | Kind::Coll { .. } | Kind::Error { .. } | Kind::Leaf => None,
        };
        let Some(task) = task else { return };
        let id = id.to_string();
        self.loading.insert(id.clone());
        self.errors.remove(&id);
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |t, cx| {
                t.loading.remove(&id);
                if let Err(e) = r {
                    t.errors.insert(id, format!("{e:#}"));
                }
                t.rebuild(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn refresh_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.selected_connection() {
            self.refresh_conn(id, cx);
        }
    }

    fn refresh_conn(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(c) = self.conn(&id, cx) else { return };
        c.schema.invalidate(None);
        c.forget_tree();
        let root = cid(&id);
        self.collapse_below(&root);
        self.expanded.insert(root.clone());
        self.load(&root, &Kind::Conn { id }, cx);
        self.rebuild(cx);
        cx.notify();
    }

    fn collapse_below(&mut self, id: &str) {
        let prefix = format!("{id}/");
        self.expanded.retain(|e| !e.starts_with(&prefix));
        self.loading.retain(|e| !e.starts_with(&prefix));
        self.errors.retain(|e, _| !e.starts_with(&prefix));
    }

    pub fn reload_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix).cloned() else { return };
        match &row.kind {
            Kind::Conn { id } => self.refresh_conn(id.clone(), cx),
            Kind::Db { conn, db } => {
                let Some(c) = self.conn(conn, cx) else { return };
                c.schema.invalidate(Some(db));
                let id = did(conn, db);
                self.collapse_below(&id);
                self.expanded.insert(id.clone());
                self.load(&id, &Kind::Db { conn: conn.clone(), db: db.clone() }, cx);
            }
            Kind::Schema { conn, db, schema } | Kind::Group { conn, db, schema, .. } => {
                let Some(c) = self.conn(conn, cx) else { return };
                c.schema.invalidate_schema(db, schema);
                let id = sid(conn, db, schema);
                let rels = format!("{id}/r:");
                self.expanded.retain(|e| !e.starts_with(&rels));
                self.loading.retain(|e| !e.starts_with(&rels));
                self.errors.retain(|e, _| !e.starts_with(&rels));
                self.expanded.insert(id.clone());
                self.load(&id, &Kind::Schema { conn: conn.clone(), db: db.clone(), schema: schema.clone() }, cx);
                for g in G::ALL {
                    let gv = gid(conn, db, schema, g);
                    if self.expanded.contains(&gv) {
                        self.load(&gv, &Kind::Group { conn: conn.clone(), db: db.clone(), schema: schema.clone(), g }, cx);
                    }
                }
            }
            Kind::Roles { conn } => {
                let Some(c) = self.conn(conn, cx) else { return };
                c.schema.invalidate_roles();
                self.load(&row.id, &row.kind, cx);
            }
            Kind::Extensions { conn, db } => {
                let Some(c) = self.conn(conn, cx) else { return };
                c.schema.invalidate_extensions(db);
                self.load(&row.id, &row.kind, cx);
            }
            Kind::Rel { conn, db, schema, name, .. }
            | Kind::Columns { conn, db, schema, name }
            | Kind::Indexes { conn, db, schema, name }
            | Kind::TableSub { conn, db, schema, name, .. } => {
                let Some(c) = self.conn(conn, cx) else { return };
                c.schema.invalidate_relation(db, schema, name);
                let id = match &row.kind {
                    Kind::Rel { .. } => row.id.to_string(),
                    _ => row.id.rsplit_once('/').map_or_else(|| rid(conn, db, schema, name), |(base, _)| base.to_string()),
                };
                let kind = Kind::Rel { conn: conn.clone(), db: db.clone(), schema: schema.clone(), name: name.clone(), kind: RelKind::Table };
                self.expanded.insert(id.clone());
                self.load(&id, &kind, cx);
            }
            Kind::Pinned { conn } => {
                let Some(c) = self.conn(conn, cx) else { return };
                for p in self.store.read(cx).core.pins.of(conn) {
                    c.schema.invalidate_relation(&p.database, &p.schema, &p.name);
                    let id = pin_id(conn, &p.database, &p.schema, &p.name);
                    if self.expanded.contains(&id) {
                        let kind = Kind::Rel { conn: conn.clone(), db: p.database.clone(), schema: p.schema.clone(), name: p.name.clone(), kind: p.kind() };
                        self.load(&id, &kind, cx);
                    }
                }
            }
            Kind::DocDb { conn, db } => {
                let id = did(conn, db);
                self.expanded.insert(id.clone());
                self.load(&id, &row.kind, cx);
            }
            Kind::Error { retry } => return self.retry(retry.clone(), cx),
            Kind::Folder { id } => {
                let conns: Vec<String> = self.store.read(cx).folders.iter().find(|d| d.id == *id).map(|d| d.connections.clone()).unwrap_or_default();
                for c in conns {
                    self.refresh_conn(c, cx);
                }
            }
            Kind::Column { .. } | Kind::Index { .. } | Kind::RelRef { .. } | Kind::Object { .. } | Kind::Coll { .. } | Kind::Leaf => {
                if let Some(p) = (0..ix).rev().find(|&i| self.rows[i].depth < row.depth) {
                    self.reload_row(p, cx);
                }
                return;
            }
        }
        self.rebuild(cx);
        cx.notify();
    }

    fn open(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(ix).cloned() else { return };
        match row.kind {
            Kind::Rel { conn, db, schema, name, .. } | Kind::RelRef { conn, db, schema, name } => {
                self.selected = Some(row.id.clone());
                cx.emit(TreeEvent::OpenRelation { conn, db, schema, name });
                cx.notify();
            }
            Kind::Coll { conn, db, name } => {
                self.selected = Some(row.id.clone());
                cx.emit(TreeEvent::OpenCollection { conn, db, coll: name });
                cx.notify();
            }
            Kind::Object { .. } if !row.expandable => {
                self.selected = Some(row.id.clone());
                if let Some(target) = self.target_for(&row, cx)
                    && let Some(act) = menu::default_act(&target)
                {
                    cx.emit(TreeEvent::RowAction { act, target, alt: false });
                }
                cx.notify();
            }
            Kind::Conn { id } if matches!(self.store.read(cx).states.get(&id), Some(ConnState::Failed(_))) => {
                cx.emit(TreeEvent::EditConnection(id));
            }
            _ => self.toggle(ix, cx),
        }
    }

    fn on_click(&mut self, ix: usize, ev: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if crate::keymap::menu_click(&ev.modifiers()) {
            return;
        }
        if let Some((f, _)) = &self.naming {
            if self.rows.get(ix).is_some_and(|r| r.id.as_ref() == fid(f)) {
                return;
            }
            self.commit_naming(cx);
        }
        window.focus(&self.focus, cx);
        if ev.click_count() >= 2 {
            return self.open(ix, cx);
        }
        self.toggle(ix, cx);
    }

    fn move_sel(&mut self, d: isize, cx: &mut Context<Self>) {
        if self.rows.is_empty() || self.naming.is_some() {
            return;
        }
        let ix = match self.selected_ix() {
            Some(i) => (i as isize + d).clamp(0, self.rows.len() as isize - 1) as usize,
            None => 0,
        };
        self.select(ix, cx);
    }

    fn target_for(&self, row: &Row, cx: &App) -> Option<Target> {
        let store = self.store.read(cx);
        let lar = |c: &str| matches!(store.states.get(c), Some(ConnState::Connected(x)) if x.info.is_lar);
        let ro = |c: &str, db: &str| match store.connection(c) {
            Some(x) => x.read_only_for(db),
            None => store.profile(c).is_some_and(|p| p.read_only_for(db)),
        };
        Some(match &row.kind {
            Kind::Folder { id } => {
                let d = store.folders.iter().find(|d| d.id == *id)?;
                Target::Folder {
                    id: id.clone(),
                    name: d.name.clone(),
                    color: d.color.as_deref().and_then(|c| crate::theme::SWATCHES.iter().position(|s| *s == c)),
                    count: store.profiles.iter().filter(|p| d.holds(&p.id)).count(),
                }
            }
            Kind::Conn { id } => Target::Conn {
                id: id.clone(),
                name: store.profile(id).map(|p| p.name.clone()).unwrap_or_default(),
                state: match store.states.get(id) {
                    None => ConnPhase::Off,
                    Some(ConnState::Connecting) => ConnPhase::Connecting,
                    Some(ConnState::Connected(_)) => ConnPhase::Connected,
                    Some(ConnState::Failed(_)) => ConnPhase::Failed,
                },
            },
            Kind::Db { conn, db } => Target::Db {
                conn: conn.clone(),
                db: db.clone(),
                current: store.connection(conn).is_some_and(|c| c.database() == db),
                lar: lar(conn),
                ro: ro(conn, db),
            },
            Kind::Schema { conn, db, schema } => Target::Schema { conn: conn.clone(), db: db.clone(), schema: schema.clone(), lar: lar(conn), ro: ro(conn, db) },
            Kind::Group { .. }
            | Kind::Columns { .. }
            | Kind::Indexes { .. }
            | Kind::TableSub { .. }
            | Kind::Roles { .. }
            | Kind::Extensions { .. }
            | Kind::Pinned { .. } => Target::Group,
            Kind::Rel { conn, db, schema, name, kind } => {
                Target::Rel { conn: conn.clone(), db: db.clone(), schema: schema.clone(), name: name.clone(), kind: *kind, lar: lar(conn), ro: ro(conn, db) }
            }
            Kind::RelRef { conn, db, schema, name } => {
                let kind = store
                    .connection(conn)
                    .and_then(|c| c.schema.cached_relations(db, schema))
                    .and_then(|v| v.into_iter().find(|r| &r.name == name).map(|r| r.kind))
                    .unwrap_or(RelKind::Table);
                Target::Rel { conn: conn.clone(), db: db.clone(), schema: schema.clone(), name: name.clone(), kind, lar: lar(conn), ro: ro(conn, db) }
            }
            Kind::Object { conn, db, schema, name, what } => {
                Target::Object { conn: conn.clone(), db: db.clone(), schema: schema.clone(), name: name.clone(), what: what.clone() }
            }
            Kind::Column { name } => Target::Column { name: name.clone() },
            Kind::Index { name, definition } => Target::Index { name: name.clone(), definition: definition.clone() },
            Kind::DocDb { conn, db } => Target::DocDb { conn: conn.clone(), db: db.clone(), ro: ro(conn, db) },
            Kind::Coll { conn, db, name } => Target::Coll { conn: conn.clone(), db: db.clone(), name: name.clone() },
            Kind::Error { .. } => Target::Error,
            Kind::Leaf => return None,
        })
    }

    fn target_engine(&self, t: &Target, cx: &App) -> dataman_core::config::Engine {
        let id = match t {
            Target::Conn { id, .. } => Some(id),
            Target::Db { conn, .. } | Target::Schema { conn, .. } | Target::Rel { conn, .. } | Target::Object { conn, .. } => Some(conn),
            Target::DocDb { conn, .. } | Target::Coll { conn, .. } => Some(conn),
            _ => None,
        };
        let store = self.store.read(cx);
        id.and_then(|c| store.profile(c)).map(|p| p.engine).unwrap_or_default()
    }

    fn target_caps(&self, t: &Target, cx: &App) -> dataman_core::Caps {
        dataman_core::Caps::of(self.target_engine(t, cx))
    }

    fn target_dialect(&self, t: &Target, cx: &App) -> dataman_core::sql::Dialect {
        self.target_engine(t, cx).dialect()
    }

    fn row_rect(&self, ix: usize) -> Option<(f32, f32, f32)> {
        let st = self.scroll.0.borrow();
        let b = st.base_handle.bounds();
        if b.size.width.pt() <= 0. {
            return None;
        }
        let (top, bottom) = (b.origin.y.pt(), b.origin.y.pt() + b.size.height.pt());
        let y = top + 2. + ix as f32 * 22. + st.base_handle.offset().y.pt();
        let y = y.clamp(top, (bottom - 22.).max(top));
        let left = b.origin.x.pt() + 6.;
        Some((left, y, left + b.size.width.pt() - 12.))
    }

    pub fn open_menu(&mut self, ix: Option<usize>, at: MenuAnchor, window: &mut Window, cx: &mut Context<Self>) {
        let (row, target) = match ix.and_then(|i| self.rows.get(i).cloned()) {
            Some(r) => match self.target_for(&r, cx) {
                Some(t) => (Some(r), t),
                None => return,
            },
            None => (None, Target::Empty),
        };
        if let (Some(i), Some(_)) = (ix, &row) {
            self.select(i, cx);
        }
        let pinned = match &target {
            Target::Rel { conn, db, schema, name, .. } => self.store.read(cx).core.pins.is_pinned(conn, db, schema, name),
            _ => false,
        };
        let entries = menu::with_pin(menu::entries_for(&target, &self.target_caps(&target, cx)), &target, pinned);
        let entries = {
            let theme = cx.theme();
            let store = self.store.read(cx);
            let folders: Vec<(SharedString, Option<Hsla>)> = store.folders.iter().map(|d| (d.name.clone().into(), folder_color(d, &theme))).collect();
            let current = match &target {
                Target::Conn { id, .. } => store.folders.iter().position(|d| d.holds(id)),
                _ => None,
            };
            let mut es = menu::with_folders(entries, &target, &folders, current);
            for e in &mut es {
                if let Entry::Sub { items, .. } = e {
                    for it in items.iter_mut() {
                        if let Act::FolderColor(Some(i)) = it.act {
                            it.dot = theme.swatches.get(i).copied();
                        }
                    }
                }
            }
            es
        };
        let vp = window.viewport_size();
        let vp = (vp.width.pt(), vp.height.pt());
        let h = menu::menu_h(&entries);
        let rect = ix.and_then(|i| self.row_rect(i));
        let origin = match (at, rect) {
            (MenuAnchor::Mouse(p), _) => menu::place((p.x.pt(), p.y.pt()), p.y.pt(), menu::MENU_W, h, vp),
            (MenuAnchor::RowMore, Some((_, top, right))) => menu::place((right - menu::MENU_W, top + 22.), top, menu::MENU_W, h, vp),
            (_, Some((left, top, _))) => {
                let depth = row.as_ref().map_or(0, |r| r.depth) as f32;
                menu::place((left + 22. + depth * 12., top + 22.), top, menu::MENU_W, h, vp)
            }
            (_, None) => {
                let st = self.scroll.0.borrow();
                let b = st.base_handle.bounds();
                menu::place((b.origin.x.pt() + 12., b.origin.y.pt() + 4.), b.origin.y.pt(), menu::MENU_W, h, vp)
            }
        };
        let enabled: Vec<bool> = entries.iter().map(Entry::enabled).collect();
        let hi = if matches!(at, MenuAnchor::Mouse(_)) { None } else { menu::step(&enabled, None, 1) };
        self.menu = Some(RowMenu { row: row.map(|r| r.id), target, entries, origin, hi, sub: None, sub_hi: None });
        window.focus(&self.menu_focus, cx);
        if self._menu_blur.is_none() {
            self._menu_blur = Some(cx.on_focus_out(&self.menu_focus, window, |t, _, _, cx| {
                if t.menu.take().is_some() {
                    cx.notify();
                }
            }));
        }
        cx.notify();
    }

    pub fn close_menu(&mut self, refocus: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(m) = self.menu.take() {
            self.menu_closed = Some((m.row, Instant::now()));
        }
        if refocus {
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }

    pub fn show_row_actions(&mut self, at: Option<gpui::Point<gpui::Pixels>>, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected_ix().or((!self.rows.is_empty()).then_some(0));
        let anchor = match at {
            Some(p) => MenuAnchor::Mouse(p),
            None => MenuAnchor::Row,
        };
        self.open_menu(ix, anchor, window, cx);
    }

    fn more_clicked(&mut self, ix: Option<usize>, at: Option<gpui::Point<gpui::Pixels>>, window: &mut Window, cx: &mut Context<Self>) {
        let row = ix.and_then(|i| self.rows.get(i)).map(|r| r.id.clone());
        let row = row.or_else(|| if ix.is_none() { self.selected.clone() } else { None });
        if let Some((closed, at_)) = &self.menu_closed
            && *closed == row
            && at_.elapsed() < Duration::from_millis(400)
        {
            self.menu_closed = None;
            return;
        }
        match ix {
            Some(i) => self.open_menu(Some(i), MenuAnchor::RowMore, window, cx),
            None => self.show_row_actions(at, window, cx),
        }
    }

    fn menu_enabled(&self) -> Vec<bool> {
        self.menu.as_ref().map(|m| m.entries.iter().map(Entry::enabled).collect()).unwrap_or_default()
    }

    fn sub_items(&self) -> Option<&[menu::Item]> {
        let m = self.menu.as_ref()?;
        match m.entries.get(m.sub?) {
            Some(Entry::Sub { items, .. }) => Some(items),
            _ => None,
        }
    }

    fn menu_move(&mut self, dir: isize, cx: &mut Context<Self>) {
        let sub_enabled: Option<Vec<bool>> = self.sub_items().map(|i| i.iter().map(menu::Item::enabled).collect());
        let enabled = self.menu_enabled();
        let Some(m) = self.menu.as_mut() else { return };
        match (m.sub_hi, sub_enabled) {
            (Some(_), Some(en)) => m.sub_hi = menu::step(&en, m.sub_hi, dir),
            _ => {
                m.hi = menu::step(&enabled, m.hi, dir);
                m.sub = None;
                m.sub_hi = None;
            }
        }
        cx.notify();
    }

    fn menu_enter_sub(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(m) = self.menu.as_mut() else { return false };
        let Some(i) = m.hi else { return false };
        let Some(Entry::Sub { items, .. }) = m.entries.get(i) else { return false };
        let en: Vec<bool> = items.iter().map(menu::Item::enabled).collect();
        m.sub = Some(i);
        m.sub_hi = menu::step(&en, None, 1);
        cx.notify();
        true
    }

    fn menu_leave_sub(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(m) = self.menu.as_mut() else { return false };
        if m.sub.is_none() {
            return false;
        }
        m.sub = None;
        m.sub_hi = None;
        cx.notify();
        true
    }

    fn menu_confirm(&mut self, alt: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.menu.as_ref() else { return };
        if let (Some(si), Some(items)) = (m.sub_hi, self.sub_items()) {
            if let Some(it) = items.get(si).filter(|i| i.enabled()) {
                let act = it.act;
                self.run_menu(act, alt, window, cx);
            }
            return;
        }
        match m.hi.and_then(|i| m.entries.get(i)) {
            Some(Entry::Item(it)) if it.enabled() => {
                let act = it.act;
                self.run_menu(act, alt, window, cx);
            }
            Some(Entry::Sub { .. }) => {
                self.menu_enter_sub(cx);
            }
            _ => {}
        }
    }

    fn run_menu(&mut self, act: Act, alt: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.menu.take() else { return };
        window.focus(&self.focus, cx);
        let ix = m.row.as_ref().and_then(|id| self.rows.iter().position(|r| &r.id == id));
        let copy = |text: String, cx: &mut Context<Self>| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            let shown: String = if text.chars().count() > 60 { format!("{}…", text.chars().take(59).collect::<String>()) } else { text };
            cx.emit(TreeEvent::Notice(format!("Copied {shown}")));
        };
        match (act, &m.target) {
            (Act::NewFolder, _) => self.new_folder(None, window, cx),
            (Act::MoveToNewFolder, Target::Conn { id, .. }) => self.new_folder(Some(id.clone()), window, cx),
            (Act::MoveToFolder(i), Target::Conn { id, name, .. }) => {
                if let Some(d) = self.store.read(cx).folders.get(i).cloned() {
                    self.move_to_folder(id, name, Some(&d), cx);
                }
            }
            (Act::RemoveFromFolder, Target::Conn { id, name, .. }) => self.move_to_folder(id, name, None, cx),
            (Act::RenameFolder, Target::Folder { id, .. }) => self.start_naming(id.clone(), window, cx),
            (Act::DeleteFolder, Target::Folder { id, name, .. }) => {
                let id = id.clone();
                self.change_folders(format!("Delete folder '{name}'"), |f| f.retain(|d| d.id != id), cx);
            }
            (Act::FolderColor(c), Target::Folder { id, name, .. }) => {
                let id = id.clone();
                let swatch = c.and_then(|i| crate::theme::SWATCHES.get(i)).map(|s| s.to_string());
                self.change_folders(format!("Color folder '{name}'"), |f| {
                    if let Some(d) = f.iter_mut().find(|d| d.id == id) {
                        d.color = swatch;
                    }
                }, cx);
            }
            (Act::Reload, _) => {
                if let Some(i) = ix {
                    self.reload_row(i, cx);
                }
            }
            (Act::Retry, _) => {
                if let Some(Kind::Error { retry }) = ix.map(|i| self.rows[i].kind.clone()) {
                    self.retry(retry, cx);
                }
            }
            (Act::Connect, Target::Conn { id, .. }) => {
                let id = id.clone();
                self.expanded.insert(cid(&id));
                self.store.update(cx, |s, cx| s.connect(&id, None, cx));
                self.rebuild(cx);
            }
            (Act::CopyName, t) => {
                let name = match t {
                    Target::Db { db, .. } | Target::DocDb { db, .. } => db.clone(),
                    Target::Coll { name, .. } => name.clone(),
                    Target::Schema { schema, .. } => schema.clone(),
                    Target::Rel { name, .. } | Target::Column { name } | Target::Index { name, .. } | Target::Object { name, .. } => name.clone(),
                    _ => return,
                };
                copy(name, cx);
            }
            (Act::CopyQualified, Target::Rel { schema, name, .. } | Target::Object { schema, name, .. }) => {
                copy(self.target_dialect(&m.target, cx).qualified(Some(schema), name), cx)
            }
            (Act::CopyDefinition, Target::Index { definition: Some(d), .. }) => copy(format!("{d};"), cx),
            (Act::CopyDefinition, Target::Object { what: Obj::Trigger { definition: Some(d), .. }, .. }) => copy(format!("{d};"), cx),
            (Act::CopyDefinition, Target::Object { schema, name, what: Obj::Constraint { table, definition: Some(def) }, .. }) => {
                let d = self.target_dialect(&m.target, cx);
                copy(format!("ALTER TABLE {} ADD CONSTRAINT {} {def};", d.qualified(Some(schema), table), d.quote_ident(name)), cx)
            }
            (Act::CopyUrl, Target::Conn { id, .. }) => {
                if let Some(p) = self.store.read(cx).profile(id).cloned() {
                    copy(p.url(), cx);
                }
            }
            (Act::RevealFile, Target::Conn { id, .. }) => {
                if let Some(p) = self.store.read(cx).profile(id).filter(|p| p.engine.is_file()) {
                    cx.reveal_path(&p.file_path());
                }
            }
            (Act::Pin | Act::Unpin, Target::Rel { conn, db, schema, name, kind, .. }) => {
                let pin = act == Act::Pin;
                self.set_pinned(Pin::new(conn, db, schema, name, *kind), pin, ix, cx);
            }
            (act, target) => cx.emit(TreeEvent::RowAction { act, target: target.clone(), alt }),
        }
        cx.notify();
    }

    /// Under the tree's last row, still in the list.
    fn below_rows(&self, pos: gpui::Point<gpui::Pixels>) -> bool {
        let st = self.scroll.0.borrow();
        let b = st.base_handle.bounds();
        if b.size.width.pt() <= 0. {
            return false;
        }
        let end = b.origin.y.pt() + 2. + self.rows.len() as f32 * 22. + st.base_handle.offset().y.pt();
        b.contains(&pos) && pos.y.pt() > end
    }

    /// One folder to show and search, or all of them.
    pub fn set_scope(&mut self, folder: Option<String>, cx: &mut Context<Self>) {
        if let Some(f) = &folder {
            self.shut_folders.remove(f);
        }
        self.scope = folder;
        self.scope_menu = false;
        self.rebuild(cx);
        if self.filtering(cx)
            && let Some(r) = self.rows.iter().find(|r| r.hl.is_some())
        {
            self.selected = Some(r.id.clone());
        }
        cx.notify();
    }

    /// The search field's folder chip: shown once there is a folder. A folder picked there is
    /// the only one the explorer shows, and the only one searched.
    fn render_scope(&self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let folders = self.store.read(cx).folders.clone();
        if folders.is_empty() {
            return None;
        }
        let current = self.scope.as_ref().and_then(|s| folders.iter().find(|d| d.id == *s));
        let tip = match current {
            Some(d) => ui::tooltip(format!("Only '{}'", d.name), Some("The explorer shows and searches this folder's connections only"), None),
            None => ui::tooltip("All folders".to_string(), Some("Pick a folder to show and search its connections only"), None),
        };
        let menu = self.scope_menu.then(|| {
            let pick = |f: Option<String>| cx.listener(move |t: &mut SchemaTree, _: &ClickEvent, _, cx| t.set_scope(f.clone(), cx));
            let mut rows: Vec<AnyElement> = vec![ui::menu_item(t, "xscope:all", "All folders", self.scope.is_none().then_some("✓"), true).cursor_pointer().on_click(pick(None)).into_any_element()];
            rows.push(ui::menu_sep(t).into_any_element());
            for d in &folders {
                let on = self.scope.as_deref() == Some(d.id.as_str());
                rows.push(
                    ui::menu_item(t, SharedString::from(format!("xscope:{}", d.id)), "", on.then_some("✓"), true)
                        .relative()
                        .cursor_pointer()
                        .child(
                            div()
                                .absolute()
                                .left(px(10.))
                                .top_0()
                                .bottom_0()
                                .right(px(28.))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.))
                                .child(ui::folder_icon(true, None, 13., folder_color(d, t).unwrap_or(t.tx3)))
                                .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(d.name.clone())),
                        )
                        .on_click(pick(Some(d.id.clone())))
                        .into_any_element(),
                );
            }
            div().absolute().right_0().top(px(22.)).child(
                deferred(
                    anchored().anchor(gpui::Anchor::TopRight).snap_to_window_with_margin(px(8.)).child(ui::menu_in(
                        "xscope-menu-in",
                        ui::popover(t)
                            .w(px(200.))
                            .on_mouse_down_out(cx.listener(|t, _, _, cx| {
                                t.scope_menu = false;
                                ui::menu_shut("x-scope");
                                cx.notify();
                            }))
                            .p(px(0.))
                            .overflow_hidden()
                            .child(div().id("xscope-menu").max_h(px(320.)).overflow_y_scroll().p(px(4.)).children(rows)),
                    )),
                )
                .priority(3),
            )
        });
        Some(div().flex_none().relative().child(scope_chip(t, "x-scope", current.map(|d| (d.name.as_str(), folder_color(d, t))), self.scope_menu, tip).on_click(cx.listener(|t, _, _, cx| {
            if !t.scope_menu && ui::menu_just_shut("x-scope") {
                return;
            }
            t.scope_menu = !t.scope_menu;
            cx.notify();
        }))).children(menu).into_any_element())
    }

    #[cfg(feature = "checks")]
    pub fn demo_scope(&self) -> (Option<String>, bool) {
        (self.scope.clone(), self.scope_menu)
    }

    #[cfg(feature = "checks")]
    pub fn demo_naming(&self, cx: &App) -> Option<(String, String)> {
        self.naming.as_ref().map(|(f, input)| (f.clone(), input.read(cx).text().to_string()))
    }

    #[cfg(feature = "checks")]
    pub fn demo_rows(&self) -> Vec<(usize, String)> {
        self.rows.iter().map(|r| (r.depth, r.label.to_string())).collect()
    }

    /// The folders changed in one write, undoable as one step.
    fn change_folders(&mut self, label: String, f: impl FnOnce(&mut Vec<dataman_core::ConnFolder>), cx: &mut Context<Self>) {
        let before = self.store.read(cx).folders.clone();
        if let Err(e) = self.store.update(cx, |s, cx| s.edit_folders(f, cx)) {
            cx.emit(TreeEvent::Notice(format!("Could not save the folders: {e:#}")));
        }
        let after = self.store.read(cx).folders.clone();
        if after != before {
            cx.emit(TreeEvent::Did(crate::undo::Entry::new(label, crate::undo::Op::ConnFolders { from: before, to: after })));
        }
        self.folders_changed(cx);
    }

    pub fn folders_changed(&mut self, cx: &mut Context<Self>) {
        self.rebuild(cx);
        cx.notify();
    }

    /// A connection into a folder (opened, so it is seen there), or out to the top level.
    fn move_to_folder(&mut self, conn: &str, name: &str, to: Option<&dataman_core::ConnFolder>, cx: &mut Context<Self>) {
        let label = match to {
            Some(d) => format!("Move '{name}' to folder '{}'", d.name),
            None => format!("Move '{name}' out of its folder"),
        };
        let (c, dest) = (conn.to_string(), to.map(|d| d.id.clone()));
        if let Some(d) = &dest {
            self.shut_folders.remove(d);
        }
        self.change_folders(label, |f| set_folder(f, &c, dest.as_deref()), cx);
        if let Some(ix) = self.rows.iter().position(|r| r.id.as_ref() == cid(conn)) {
            self.select(ix, cx);
        }
    }

    /// A new folder (with `conn` in it), open, selected and named in place.
    fn new_folder(&mut self, conn: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let d = dataman_core::ConnFolder::new("New folder", &self.store.read(cx).folders);
        let (id, label) = (d.id.clone(), format!("New folder '{}'", d.name));
        self.change_folders(label, |f| {
            f.push(d);
            if let Some(c) = &conn {
                set_folder(f, c, Some(id.as_str()));
            }
        }, cx);
        if self.scope.is_some() {
            self.scope = Some(id.clone());
            self.rebuild(cx);
        }
        if let Some(ix) = self.rows.iter().position(|r| r.id.as_ref() == fid(&id)) {
            self.select(ix, cx);
            self.start_naming(id, window, cx);
        }
    }

    fn start_naming(&mut self, folder: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.store.read(cx).folders.iter().find(|d| d.id == folder).map(|d| d.name.clone()) else { return };
        let input = cx.new(|cx| TextInput::new(cx, "name").bare());
        input.update(cx, |i, cx| {
            i.set_text(name, cx);
            i.select_all(cx);
        });
        cx.subscribe(&input, |t: &mut SchemaTree, _, ev: &InputEvent, cx| {
            if let InputEvent::Submit = ev {
                t.commit_naming(cx);
            }
        })
        .detach();
        window.focus(&input.focus_handle(cx), cx);
        self.naming = Some((folder, input));
        // The row (or the tree's own track_focus) takes the focus back on this same click after
        // this handler runs: the field reclaims it once the event is done.
        cx.defer_in(window, |t, window, cx| {
            if let Some((_, input)) = &t.naming {
                window.focus(&input.focus_handle(cx), cx);
            }
        });
        cx.notify();
    }

    fn commit_naming(&mut self, cx: &mut Context<Self>) {
        let Some((id, input)) = self.naming.take() else { return };
        let name = input.read(cx).text().trim().to_string();
        let from = self.store.read(cx).folders.iter().find(|d| d.id == id).map(|d| d.name.clone());
        match from {
            Some(from) if !name.is_empty() && from != name => self.change_folders(format!("Rename folder '{from}'"), |f| {
                if let Some(d) = f.iter_mut().find(|d| d.id == id) {
                    d.name = name;
                }
            }, cx),
            _ => cx.notify(),
        }
        self.focus_back = true;
    }

    fn set_pinned(&mut self, pin: Pin, on: bool, ix: Option<usize>, cx: &mut Context<Self>) {
        let core = self.store.read(cx).core.clone();
        let engine = self.store.read(cx).profile(&pin.connection).map(|p| p.engine).unwrap_or_default();
        let what = engine.dialect().qualified(Some(&pin.schema), &pin.name);
        let before = core.pins.of(&pin.connection);
        let saved = if on { core.pins.pin(pin.clone()) } else { core.pins.unpin(&pin.connection, &pin.database, &pin.schema, &pin.name) };
        if let Err(e) = saved {
            cx.emit(TreeEvent::Notice(format!("Could not save the pinned tables: {e:#}")));
            return;
        }
        let after = core.pins.of(&pin.connection);
        if after != before {
            let op = crate::undo::Op::Pins { conn: pin.connection.clone(), from: before, to: after };
            cx.emit(TreeEvent::Did(crate::undo::Entry::new(format!("{} {what}", if on { "Pin" } else { "Unpin" }), op)));
        }
        if on {
            self.expanded.remove(&pinned_id(&pin.connection));
        }
        self.rebuild(cx);
        match self.selected_ix() {
            Some(i) => self.scroll.scroll_to_item(i, gpui::ScrollStrategy::Nearest),
            None if !self.rows.is_empty() => self.select(ix.unwrap_or(0).min(self.rows.len() - 1), cx),
            None => {}
        }
        cx.emit(TreeEvent::Notice(format!("{} {what}", if on { "Pinned" } else { "Unpinned" })));
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub fn demo_row_point(&self, id: &str) -> Option<gpui::Point<gpui::Pixels>> {
        let ix = self.rows.iter().position(|r| r.id.as_ref() == id)?;
        let (left, top, right) = self.row_rect(ix)?;
        Some(point(px((left + right) / 2.), px(top + 11.)))
    }

    #[cfg(feature = "checks")]
    pub fn demo_pinned(&self, conn: &str) -> Vec<(String, String)> {
        let prefix = format!("{}/", pinned_id(conn));
        self.rows
            .iter()
            .filter(|r| r.id.starts_with(&prefix) && matches!(r.kind, Kind::Rel { .. }))
            .map(|r| (r.label.to_string(), r.meta.to_string()))
            .collect()
    }

    #[cfg(feature = "checks")]
    pub fn demo_menu_item_point(&self, act: Act) -> Option<gpui::Point<gpui::Pixels>> {
        let m = self.menu.as_ref()?;
        let i = m.entries.iter().position(|e| matches!(e, Entry::Item(it) if it.act == act))?;
        let y = m.origin.1 + menu::entry_top(&m.entries, i) + menu::ITEM_H / 2.;
        Some(point(px(m.origin.0 + 40.), px(y)))
    }

    #[cfg(feature = "checks")]
    pub fn demo_menu(&mut self, id: &str, sub: bool, mouse: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.rows.iter().position(|r| r.id.as_ref() == id) else { return };
        let at = match (mouse, self.row_rect(ix)) {
            (true, Some((left, top, _))) => MenuAnchor::Mouse(point(px(left + 90.), px(top + 14.))),
            _ => MenuAnchor::Row,
        };
        self.open_menu(Some(ix), at, window, cx);
        self.demo_menu_highlight(sub, mouse, cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_menu_acts(&self) -> Vec<Act> {
        let Some(m) = &self.menu else { return vec![] };
        m.entries
            .iter()
            .flat_map(|e| match e {
                Entry::Item(i) => vec![i.act],
                Entry::Sub { items, .. } => items.iter().map(|i| i.act).collect(),
                Entry::Sep => vec![],
            })
            .collect()
    }

    #[cfg(feature = "checks")]
    pub fn demo_menu_checked(&self, act: Act) -> Option<bool> {
        let m = self.menu.as_ref()?;
        m.entries.iter().find_map(|e| match e {
            Entry::Item(i) if i.act == act => Some(i.checked),
            _ => None,
        })
    }

    #[cfg(feature = "checks")]
    pub fn demo_menu_run(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        self.run_menu(act, false, window, cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_row_lock(&self, id: &str) -> Option<bool> {
        self.rows.iter().find(|r| r.id.as_ref() == id).map(|r| r.lock)
    }

    #[cfg(feature = "checks")]
    pub fn demo_menu_highlight(&mut self, sub: bool, mouse: bool, cx: &mut Context<Self>) {
        if let Some(m) = self.menu.as_mut() {
            if mouse {
                m.hi = Some(2);
            }
            if sub && let Some(i) = m.entries.iter().position(|e| matches!(e, Entry::Sub { .. })) {
                m.hi = Some(i);
                m.sub = Some(i);
                m.sub_hi = Some(4);
            }
        }
        cx.notify();
    }

    fn render_menu(&self, window: &Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(m) = &self.menu else { return vec![] };
        let t = cx.theme();
        let vp = window.viewport_size();
        let vp = (vp.width.pt(), vp.height.pt());
        let sub = m.sub.and_then(|i| match m.entries.get(i) {
            Some(Entry::Sub { items, .. }) => {
                let h = menu::sub_h(items);
                Some((menu::place_sub(m.origin, menu::entry_top(&m.entries, i), h, vp), h, items.clone()))
            }
            _ => None,
        });
        let sub_rect = sub.as_ref().map(|((x, y), h, _)| (*x, *y, menu::SUB_W, *h));
        let mut items: Vec<AnyElement> = Vec::new();
        for (i, e) in m.entries.iter().enumerate() {
            match e {
                Entry::Sep => items.push(ui::menu_sep(&t).into_any_element()),
                Entry::Item(it) => {
                    let hi = m.hi == Some(i) && m.sub_hi.is_none();
                    let el = menu_row(&t, ("menu-item", i), it.label.clone(), it.icon, it.dot, it.key, it.disabled, it.bold, it.danger, it.checked, hi, false)
                        .on_hover(cx.listener(move |tr, h: &bool, _, cx| {
                            if let (true, Some(m)) = (*h, tr.menu.as_mut()) {
                                m.hi = Some(i);
                                m.sub = None;
                                m.sub_hi = None;
                                cx.notify();
                            }
                        }));
                    let act = it.act;
                    items.push(if it.enabled() {
                        el.on_click(cx.listener(move |tr, ev: &ClickEvent, window, cx| tr.run_menu(act, ev.modifiers().alt, window, cx)))
                            .into_any_element()
                    } else {
                        el.into_any_element()
                    });
                }
                Entry::Sub { label, icon: ic, .. } => {
                    let open = m.sub == Some(i);
                    let hi = m.hi == Some(i) && m.sub_hi.is_none() || open && m.sub_hi.is_some();
                    let el = menu_row(&t, ("menu-item", i), (*label).into(), *ic, None, None, None, false, false, false, hi || open, true)
                        .on_hover(cx.listener(move |tr, h: &bool, _, cx| {
                            if let (true, Some(m)) = (*h, tr.menu.as_mut()) {
                                m.hi = Some(i);
                                m.sub = Some(i);
                                m.sub_hi = None;
                                cx.notify();
                            }
                        }))
                        .on_click(cx.listener(move |tr, _, _, cx| {
                            if let Some(m) = tr.menu.as_mut() {
                                m.hi = Some(i);
                            }
                            tr.menu_enter_sub(cx);
                        }));
                    items.push(el.into_any_element());
                }
            }
        }
        let main = ui::popover(&t)
            .id("row-menu")
            .w(px(menu::MENU_W))
            .text_size(px(12.))
            .font_family(crate::theme::mono())
            .track_focus(&self.menu_focus)
            .key_context(MENU_CONTEXT)
            .on_action(cx.listener(|tr, _: &MenuUp, _, cx| tr.menu_move(-1, cx)))
            .on_action(cx.listener(|tr, _: &MenuDown, _, cx| tr.menu_move(1, cx)))
            .on_action(cx.listener(|tr, _: &MenuRight, _, cx| {
                tr.menu_enter_sub(cx);
            }))
            .on_action(cx.listener(|tr, _: &MenuLeft, _, cx| {
                tr.menu_leave_sub(cx);
            }))
            .on_action(cx.listener(|tr, _: &MenuConfirm, window, cx| tr.menu_confirm(false, window, cx)))
            .on_action(cx.listener(|tr, _: &MenuConfirmAlt, window, cx| tr.menu_confirm(true, window, cx)))
            .on_action(cx.listener(|tr, _: &MenuClose, window, cx| {
                if !tr.menu_leave_sub(cx) {
                    tr.close_menu(true, window, cx);
                }
            }))
            .on_mouse_down_out(cx.listener(move |tr, ev: &MouseDownEvent, window, cx| {
                let (x, y) = (ev.position.x.pt(), ev.position.y.pt());
                if sub_rect.is_some_and(|(sx, sy, sw, sh)| x >= sx && x < sx + sw && y >= sy && y < sy + sh) {
                    return;
                }
                tr.close_menu(true, window, cx);
            }))
            .children(items);
        let mut out = vec![deferred(anchored().position(point(px(m.origin.0), px(m.origin.1))).child(ui::menu_in("row-menu-in", main))).priority(2).into_any_element()];
        if let Some(((x, y), _, items)) = sub {
            let rows: Vec<AnyElement> = items
                .iter()
                .enumerate()
                .map(|(j, it)| {
                    let hi = m.sub_hi == Some(j);
                    let act = it.act;
                    let el = menu_row(&t, ("sub-item", j), it.label.clone(), it.icon, it.dot, it.key, it.disabled, it.bold, it.danger, it.checked, hi, false).on_hover(cx.listener(
                        move |tr, h: &bool, _, cx| {
                            if let (true, Some(m)) = (*h, tr.menu.as_mut()) {
                                m.sub_hi = Some(j);
                                cx.notify();
                            }
                        },
                    ));
                    if it.enabled() {
                        el.on_click(cx.listener(move |tr, ev: &ClickEvent, window, cx| tr.run_menu(act, ev.modifiers().alt, window, cx)))
                            .into_any_element()
                    } else {
                        el.into_any_element()
                    }
                })
                .collect();
            let pop = ui::popover(&t)
                .id("row-submenu")
                .w(px(menu::SUB_W))
                .text_size(px(12.))
                .font_family(crate::theme::mono())
                .children(rows);
            out.push(deferred(anchored().position(point(px(x), px(y))).child(ui::menu_in("row-submenu-in", pop))).priority(3).into_any_element());
        }
        out
    }

    fn fit_meta(&self, r: &Row) -> SharedString {
        let w = self.scroll.0.borrow().base_handle.bounds().size.width.pt();
        if w <= 0. || r.meta.is_empty() || matches!(r.meta_kind, Meta::NotReported) {
            return r.meta.clone();
        }
        let extra = if r.chip.is_some() || r.lar || r.tunnel || r.lock { 40. } else { 0. };
        let em = crate::theme::advance();
        let room = w - 65. - extra - r.depth as f32 * 12. - r.label.chars().count() as f32 * 12. * em;
        match fit_meta_text(&r.meta, r.meta_short.as_ref().map(|s| s.as_ref()), (room / (11. * em)).floor() as i64) {
            Some(s) => s.into(),
            None => r.meta.clone(),
        }
    }

    /// Where the dragged connection would land as a place in the connection order (the end:
    /// their count); none onto a folder.
    #[cfg(feature = "checks")]
    pub fn conn_drop(&self, cx: &App) -> Option<usize> {
        let profiles = &self.store.read(cx).profiles;
        match self.conn_drop.as_ref()? {
            ConnDrop::At { before: Some(b), .. } => profiles.iter().position(|p| p.id == *b),
            ConnDrop::At { before: None, .. } => Some(profiles.len()),
            ConnDrop::Into(_) => None,
        }
    }

    #[cfg(feature = "checks")]
    pub fn demo_conn_drop(&self) -> Option<ConnDrop> {
        self.conn_drop.clone()
    }

    /// What a drag over row `ix` would do: onto a folder's row, into it; on a connection's row
    /// (or one of its children), before or after it in its group. None when nothing would change.
    fn conn_target(&self, dragged: &str, ix: usize, bounds: &gpui::Bounds<gpui::Pixels>, pos: gpui::Point<gpui::Pixels>, cx: &App) -> Option<ConnDrop> {
        if !bounds.contains(&pos) {
            return None;
        }
        let head = (0..=ix.min(self.rows.len().checked_sub(1)?)).rev().find(|&j| matches!(self.rows[j].kind, Kind::Conn { .. } | Kind::Folder { .. }))?;
        let drop = match &self.rows[head].kind {
            Kind::Folder { id } => ConnDrop::Into(id.clone()),
            Kind::Conn { id } => {
                let store = self.store.read(cx);
                let group = store.folder_of(id).map(|d| d.id.clone());
                let past = head != ix || pos.y > bounds.center().y;
                let before = if past {
                    let same = |p: &&dataman_core::ConnectionProfile| store.folder_of(&p.id).map(|d| &d.id) == group.as_ref();
                    store.profiles.iter().filter(same).skip_while(|p| p.id != *id).nth(1).map(|p| p.id.clone())
                } else {
                    Some(id.clone())
                };
                ConnDrop::At { folder: group, before }
            }
            _ => return None,
        };
        self.drop_result(dragged, &drop, cx).map(|_| drop)
    }

    /// The connection order and the folders after the drop; None when neither changes.
    fn drop_result(&self, dragged: &str, drop: &ConnDrop, cx: &App) -> Option<(Vec<String>, Vec<dataman_core::ConnFolder>)> {
        let store = self.store.read(cx);
        let old: Vec<String> = store.profiles.iter().map(|p| p.id.clone()).collect();
        let from = old.iter().position(|i| i == dragged)?;
        let mut order = old.clone();
        let mut folders = store.folders.clone();
        match drop {
            ConnDrop::Into(d) => set_folder(&mut folders, dragged, Some(d.as_str())),
            ConnDrop::At { folder, before } => {
                order.remove(from);
                let group = |i: &String| dataman_core::config::folder_of(&folders, i).map(|d| d.id.clone()) == *folder;
                let at = match before {
                    Some(b) if b == dragged => from,
                    Some(b) => order.iter().position(|i| i == b)?,
                    None => match order.iter().rposition(group) {
                        Some(last) => last + 1,
                        None if folder.is_none() => order.len(),
                        None => from,
                    },
                };
                order.insert(at, dragged.to_string());
                set_folder(&mut folders, dragged, folder.as_deref());
            }
        }
        (order != old || folders != store.folders).then_some((order, folders))
    }

    fn drop_conn(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(drop) = self.conn_drop.take() else { return };
        let Some((order, folders)) = self.drop_result(id, &drop, cx) else { return cx.notify() };
        let store = self.store.read(cx);
        let (old_order, old_folders): (Vec<String>, _) = (store.profiles.iter().map(|p| p.id.clone()).collect(), store.folders.clone());
        let name = store.profile(id).map(|p| p.name.clone()).unwrap_or_default();
        let into = match &drop {
            ConnDrop::Into(d) | ConnDrop::At { folder: Some(d), .. } => store.folders.iter().find(|f| f.id == *d).map(|f| f.name.clone()),
            ConnDrop::At { folder: None, .. } => None,
        };
        let moved_folder = folders != old_folders;
        let mut ops = Vec::new();
        if order != old_order {
            self.store.update(cx, |s, cx| s.set_order(&order, cx));
            ops.push(crate::undo::Op::ConnOrder { from: old_order, to: order });
        }
        if moved_folder {
            if let ConnDrop::Into(d) = &drop {
                self.shut_folders.remove(d);
            }
            if let Err(e) = self.store.update(cx, |s, cx| s.edit_folders(|f| *f = folders.clone(), cx)) {
                cx.emit(TreeEvent::Notice(format!("Could not save the folders: {e:#}")));
            }
            ops.push(crate::undo::Op::ConnFolders { from: old_folders, to: folders });
        }
        let label = match (moved_folder, into) {
            (true, Some(f)) => format!("Move '{name}' to folder '{f}'"),
            (true, None) => format!("Move '{name}' out of its folder"),
            (false, _) => format!("Move connection '{name}'"),
        };
        let op = if ops.len() == 1 { ops.remove(0) } else { crate::undo::Op::Many(ops) };
        cx.emit(TreeEvent::Did(crate::undo::Entry::new(label, op)));
        self.rebuild(cx);
        cx.notify();
    }

    /// The line a drop would draw at row `ix`: above it (false) or under it (true).
    fn drop_line_at(&self, ix: usize) -> Option<bool> {
        let r = &self.rows[ix];
        match self.conn_drop.as_ref()? {
            ConnDrop::At { before: Some(b), .. } => matches!(&r.kind, Kind::Conn { id } if id == b).then_some(false),
            ConnDrop::At { folder: None, before: None } => (ix + 1 == self.rows.len()).then_some(true),
            ConnDrop::At { folder: Some(f), before: None } => {
                let inside = |r: &Row| r.in_folder.as_deref() == Some(f.as_str());
                (inside(r) && !self.rows.get(ix + 1).is_some_and(inside)).then_some(true)
            }
            ConnDrop::Into(_) => None,
        }
    }

    fn render_row(&self, ix: usize, t: &Theme, focused: bool, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let r = &self.rows[ix];
        let reorder = self.filter_stats.is_none();
        let drag_conn = match &r.kind {
            Kind::Conn { id } if reorder => Some(id.clone()),
            _ => None,
        };
        let dragging = reorder && cx.has_active_drag();
        let line = if dragging { self.drop_line_at(ix) } else { None };
        let dropping = dragging && matches!((&r.kind, &self.conn_drop), (Kind::Folder { id }, Some(ConnDrop::Into(d))) if id == d);
        let naming = match (&r.kind, &self.naming) {
            (Kind::Folder { id }, Some((f, input))) if id == f => Some(input.clone()),
            _ => None,
        };
        let sel = self.selected.as_ref() == Some(&r.id);
        let (label_color, bold) = label_style(r.tone, sel, t);
        let hov = t.hov;
        let has_menu = !matches!(r.kind, Kind::Leaf);
        let menu_here = self.menu.as_ref().is_some_and(|m| m.row.as_ref() == Some(&r.id));
        let hovered = self.hovered.as_ref() == Some(&r.id);
        let chevron = r.expandable.then(|| ui::disclosure(gpui::ElementId::Name(format!("chev-{}", r.id).into()), r.expanded, 10., t.tx4));
        let pop = self.pop_now.as_ref().filter(|(id, _)| *id == r.id).map(|(_, f)| *f);
        let label = if let Some(input) = naming {
            div()
                .flex()
                .flex_row()
                .items_center()
                .h(px(18.))
                .border_1()
                .border_color(t.acc)
                .pl(px(4.))
                .pr(px(2.))
                .text_color(t.tx)
                .child(div().flex_1().min_w_0().child(input))
        } else {
            let l = r.label.to_string();
            match &r.hl {
                Some(h) if h.end <= l.len() => {
                    let hit = l[h.clone()].to_string();
                    div()
                        .flex()
                        .flex_row()
                        .child(l[..h.start].to_string())
                        .child(
                            div()
                                .relative()
                                .bg(t.mark)
                                .text_color(t.tx)
                                .border_b_1()
                                .border_color(t.mark_b)
                                .child(hit.clone())
                                .children(pop.map(|f| pop_overlay(hit, f, t))),
                        )
                        .child(l[h.end..].to_string())
                }
                _ => div().child(l),
            }
        };
        let meta_text = self.fit_meta(r);
        let meta = match r.meta_kind {
            _ if meta_text.is_empty() => None,
            Meta::NotReported => Some(ui::not_reported(t).into_any_element()),
            k => {
                let color = match k {
                    Meta::Ok => t.ok,
                    Meta::Warn => t.warn,
                    Meta::Err => t.err,
                    Meta::Off => t.tx4,
                    Meta::LarState => t.lar,
                    Meta::Link => t.acc_t,
                    _ => t.tx3,
                };
                Some(div().flex_none().text_size(px(11.)).whitespace_nowrap().text_color(color).child(meta_text).into_any_element())
            }
        };
        let more = has_menu.then(|| {
            let (pop, prs, tx2) = (t.pop, t.prs, t.tx2);
            div()
                .id(("row-more", ix))
                .absolute()
                .top(px(3.))
                .right(px(3.))
                .size(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .bg(if menu_here { prs } else { pop })
                .when(!menu_here, |d| d.invisible().group_hover(ROW_GROUP, |s| s.visible()))
                .hover(move |s| s.bg(prs))
                .child(icon(Icon::More).size(px(12.)).text_color(tx2))
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_click(cx.listener(move |t, _, window, cx| {
                    cx.stop_propagation();
                    t.more_clicked(Some(ix), None, window, cx);
                }))
        });
        let conn_bounds = (r.depth == 0 || matches!(r.kind, Kind::Conn { .. })).then(|| (self.conn_bounds.clone(), r.id.clone()));
        div()
            .w_full()
            .px(px(6.))
            .when_some(conn_bounds, |d, (all, id)| d.on_children_prepainted(move |b, _, _| {
                if let Some(b) = b.first() {
                    all.borrow_mut().insert(id.clone(), *b);
                }
            }))
            .child(div()
            .id(ix)
            .group(ROW_GROUP)
            .relative()
            .w_full()
            .h(px(22.))
            .pl(px(6. + r.depth as f32 * 12.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.))
            .when(dropping, |d| d.bg(t.acc_s).border_1().border_color(t.acc.opacity(0.45)))
            .when(!dropping && sel && focused, |d| d.bg(t.acc_s).border_1().border_color(t.edge_acc))
            .when(!dropping && sel && !focused, |d| d.bg(t.prs))
            .when(!dropping && !sel, |d| d.hover(move |s| s.bg(hov)))
            .on_click(cx.listener(move |t, ev: &ClickEvent, window, cx| t.on_click(ix, ev, window, cx)))
            .when_some(drag_conn, |d, id| {
                let (label, chip) = (r.label.clone(), r.chip);
                d.on_drag(DraggedConn(id), move |_, _, _, cx| crate::dnd::ghost(label.clone(), chip, cx))
            })
            .when(reorder, |d| {
                d.on_drag_move(cx.listener(move |t, ev: &gpui::DragMoveEvent<DraggedConn>, _, cx| {
                    let pos = ev.event.position;
                    if !ev.bounds.contains(&pos) {
                        return;
                    }
                    let id = ev.drag(cx).0.clone();
                    let target = t.conn_target(&id, ix, &ev.bounds, pos, cx);
                    if target != t.conn_drop {
                        t.conn_drop = target;
                        cx.notify();
                    }
                }))
            })
            .on_hover(cx.listener({
                let id = r.id.clone();
                move |t, on: &bool, _, cx| {
                    if *on {
                        if t.hovered.as_ref() != Some(&id) {
                            t.hovered = Some(id.clone());
                            cx.notify();
                        }
                    } else if t.hovered.as_ref() == Some(&id) {
                        t.hovered = None;
                        cx.notify();
                    }
                }
            }))
            .when(has_menu, |d| {
                d.on_mouse_down(MouseButton::Right, cx.listener(move |t, ev: &MouseDownEvent, window, cx| {
                    t.open_menu(Some(ix), MenuAnchor::Mouse(ev.position), window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }))
                .on_mouse_down(MouseButton::Left, cx.listener(move |t, ev: &MouseDownEvent, window, cx| {
                    if crate::keymap::menu_click(&ev.modifiers) {
                        t.open_menu(Some(ix), MenuAnchor::Mouse(ev.position), window, cx);
                        window.prevent_default();
                        cx.stop_propagation();
                    }
                }))
            })
            .child(div().w(px(10.)).flex_none().flex().justify_center().children(chevron))
            .when_some(r.tip.clone(), |d, tip| d.tip(ui::text_tooltip(tip)))
            .children(r.chip.map(|c| div().mx(px(3.)).when(r.chip_dim, |d| d.opacity(0.45)).child(ui::chip(c, 8.))))
            .children(r.logo.and_then(|e| crate::assets::small_logo(e, 13.)).map(|l| l.when(r.chip_dim, |d| d.opacity(0.5))))
            .children(r.icon.filter(|_| r.chip.is_none()).map(|(i, c)| {
                let c = c.unwrap_or(t.tx3);
                if i != Icon::Schema {
                    return icon(i).size(px(14.)).text_color(c).into_any_element();
                }
                let fill = self.folder_turn.as_ref().filter(|(id, ..)| *id == *r.id).and_then(|(_, at, opening)| crate::motion::folder_fill(*at, *opening));
                ui::folder_icon(r.expanded, fill, 14., if dropping { t.acc_t } else { c })
            }))
            .when(r.blank_icon, |d| d.child(div().flex_none().size(px(14.))))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(label_color)
                    .when(bold, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .when(r.tone == Tone::Italic, |d| d.italic())
                    .child(label),
            )
            .when(r.tunnel, |d| d.child(icon(Icon::Tunnel).size(px(11.)).text_color(t.tx3)))
            .when(r.lock, |d| d.child(icon(Icon::Lock).flex_none().size(px(11.)).text_color(t.tx3)))
            .when(r.lar, |d| d.child(ui::lar_tag(t)))
            // A hover style cannot move the meta: GPUI lays out before it knows the hover.
            .children(meta.map(|m| div().flex_none().when(has_menu && (menu_here || hovered), |d| d.mr(px(MORE_ROOM))).child(m)))
            .children(more)
            .children(line.map(|after| crate::dnd::drop_line(t, false, after))))
    }
}

/// The scope chip at the right end of a search field: "All", or a folder's name cut short
/// with its color.
pub fn scope_chip(t: &Theme, id: &'static str, folder: Option<(&str, Option<Hsla>)>, open: bool, tip: impl Fn(&mut Window, &mut App) -> gpui::AnyView + 'static) -> gpui::Stateful<gpui::Div> {
    let (hov, prs) = (t.hov, t.prs);
    let label = match folder {
        Some((name, _)) => short(name, 10),
        None => "All".to_string(),
    };
    let dot = folder.and_then(|(_, c)| c);
    div()
        .children(ui::here(&id.into()))
        .id(id)
        .flex_none()
        .h(px(20.))
        .max_w(px(120.))
        .px(px(6.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.))
        .text_size(px(11.))
        .whitespace_nowrap()
        .border_1()
        .cursor_pointer()
        .when(open, |d| d.bg(prs).border_color(t.line2).text_color(t.tx))
        .when(!open && folder.is_some(), |d| d.bg(t.acc_s).border_color(t.mark_b).text_color(t.tx).hover(move |s| s.bg(hov)))
        .when(!open && folder.is_none(), |d| d.border_color(t.line).text_color(t.tx3).hover(move |s| s.bg(hov)))
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .when(!open, |d| d.tip(tip))
        .children(folder.map(|_| ui::folder_icon(true, None, 11., dot.unwrap_or(t.tx3))))
        .child(label)
        .child(ui::dropdown_arrow(SharedString::from(format!("{id}-arrow")), open, 7., t.tx3))
}

/// `conn` in folder `to` (out of any other), or in none.
fn set_folder(folders: &mut [dataman_core::ConnFolder], conn: &str, to: Option<&str>) {
    for d in folders.iter_mut() {
        if Some(d.id.as_str()) == to {
            if !d.holds(conn) {
                d.connections.push(conn.to_string());
            }
        } else {
            d.connections.retain(|c| c != conn);
        }
    }
}

pub fn folder_color(d: &dataman_core::ConnFolder, t: &Theme) -> Option<Hsla> {
    d.color.as_deref().and_then(|c| crate::theme::SWATCHES.iter().position(|s| *s == c)).map(|i| t.swatches[i])
}

/// A name cut to `n` characters, an ellipsis ending it.
pub fn short(name: &str, n: usize) -> String {
    if name.chars().count() <= n { name.to_string() } else { format!("{}…", name.chars().take(n - 1).collect::<String>().trim_end()) }
}

fn label_style(tone: Tone, sel: bool, t: &Theme) -> (Hsla, bool) {
    let bold = matches!(tone, Tone::Conn | Tone::ConnLar);
    let color = match tone {
        Tone::ConnOff if sel => t.tx3,
        Tone::ConnOff => t.tx4,
        Tone::ConnWait if sel => t.tx2,
        Tone::ConnWait => t.tx3,
        _ if sel => t.tx,
        Tone::Conn => t.tx,
        Tone::ConnLar => t.lar_t,
        Tone::Muted | Tone::Italic => t.tx3,
        Tone::Error => t.err,
        Tone::Normal => t.tx2,
        Tone::Folder => t.tx,
    };
    (color, bold)
}

/// Deferred, so it paints after the tree and the enlarged box is not cut by the label's clip.
fn pop_overlay(text: String, f: PopFrame, t: &Theme) -> impl IntoElement + use<> {
    let (fill, ink) = (t.find.opacity(f.strength), t.find_tx.opacity(f.strength));
    deferred(
        gpui::canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| {
                // The row's face and size: the text style stack is not restored for a deferred paint.
                let run = gpui::TextRun {
                    len: text.len(),
                    font: gpui::font(crate::theme::mono()),
                    color: ink,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let lh = bounds.size.height - px(1.);
                let line = window.text_system().shape_line(text.clone().into(), px(12. * f.scale), &[run], None);
                let rest = gpui::Bounds::from_corners(point(bounds.left() - px(2.), bounds.top()), point(bounds.right() + px(2.), bounds.bottom()));
                crate::motion::paint_pop(window, cx, rest, 0., fill, f.scale, Some((&line, bounds.origin, lh)));
            },
        )
        .absolute()
        .inset_0(),
    )
}

fn rel_row(c: &str, db: &str, s: &str, r: &dataman_core::schema::Relation, depth: usize, open: bool, stats: Option<&std::collections::HashMap<String, RelStats>>) -> Row {
    let icon = match r.kind {
        RelKind::View => Icon::View,
        RelKind::MaterializedView => Icon::MatView,
        RelKind::Partitioned => Icon::Partition,
        RelKind::Table | RelKind::Foreign => Icon::Table,
    };
    let mut row = Row::new(rid(c, db, s, &r.name), depth, r.name.clone(), Kind::Rel { conn: c.into(), db: db.into(), schema: s.into(), name: r.name.clone(), kind: r.kind })
        .icon(icon)
        .expandable(open);
    match (r.kind, stats.and_then(|m| m.get(&r.name))) {
        (RelKind::Foreign, _) => row = row.meta("foreign", Meta::Plain),
        (RelKind::View, _) | (_, None) => {}
        (_, Some(st)) => {
            let (full, short) = stats_meta(st);
            row = row.meta(full, Meta::Plain);
            row.meta_short = short.map(SharedString::from);
        }
    }
    row
}

fn pin_row(c: &str, conn: &Connection, p: &Pin, depth: usize, open: bool) -> Row {
    let (db, s) = (p.database.as_str(), p.schema.as_str());
    let listed = conn.schema.cached_relations(db, s);
    let kind = match &listed {
        Some(rels) => rels.iter().find(|r| r.name == p.name).map(|r| r.kind),
        None => Some(p.kind()),
    };
    let rel = dataman_core::schema::Relation { schema: s.into(), name: p.name.clone(), kind: kind.unwrap_or(p.kind()), oid: None };
    let mut row = rel_row(c, db, s, &rel, depth, open, None);
    row.id = pin_id(c, db, s, &p.name).into();
    if kind.is_none() {
        return Row { expandable: false, ..row.tone(Tone::Muted).meta("not found", Meta::Off) };
    }
    let place = pin_place(db, s, conn.database(), !conn.caps().schemas);
    if place.is_empty() { row } else { row.meta(place, Meta::Plain) }
}

fn pin_place(db: &str, schema: &str, home: &str, flat: bool) -> String {
    match (flat, db == home) {
        (true, true) => String::new(),
        (true, false) => db.to_string(),
        (false, true) => schema.to_string(),
        (false, false) => format!("{db}.{schema}"),
    }
}

fn stats_meta(st: &RelStats) -> (String, Option<String>) {
    let rows = st.rows.map(approx_count);
    let size = st.bytes.map(human_size);
    match (rows, size) {
        (Some(r), Some(b)) => (format!("{r} · {b}"), Some(r)),
        (Some(r), None) => (r, None),
        (None, Some(b)) => (b, None),
        (None, None) => (String::new(), None),
    }
}

fn group_row(id: &str, depth: usize, label: &'static str, n: usize, open: bool, kind: Kind) -> Row {
    Row::new(id.to_string(), depth, label, kind).icon(Icon::Group).meta(ui_count(n), Meta::Plain).expandable(open)
}

fn fit_meta_text(meta: &str, short: Option<&str>, room: i64) -> Option<String> {
    let n = meta.chars().count() as i64;
    if n <= room {
        return None;
    }
    if let Some(s) = short {
        return Some(s.to_string());
    }
    Some(if room < 8 { String::new() } else { clip(meta, room as usize) })
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n - 1).collect::<String>()) }
}

fn function_meta(f: &dataman_core::schema::Function, schema: &str, pg: bool) -> String {
    let own = format!("{schema}.");
    let ty = |t: &str| short_type(t.strip_prefix(own.as_str()).unwrap_or(t), pg);
    let args: Vec<String> = f.arg_types.iter().map(|a| ty(a)).collect();
    let mut s = format!("({})", args.join(", "));
    if let Some(r) = &f.result {
        let r = match r.strip_prefix("SETOF ") {
            Some(t) => format!("setof {}", ty(t)),
            None => ty(r),
        };
        s.push_str(" → ");
        s.push_str(&r);
    }
    clip(&s, 34)
}

fn obj_kind(c: &str, db: &str, s: &str, name: &str, what: Obj) -> Kind {
    Kind::Object { conn: c.into(), db: db.into(), schema: s.into(), name: name.into(), what }
}

fn function_row(c: &str, db: &str, s: &str, f: &dataman_core::schema::Function, depth: usize, pg: bool) -> Row {
    let procedure = f.kind == FuncKind::Procedure;
    let id = format!("{}/f:{}", gid(c, db, s, G::Functions), f.oid);
    Row::new(id, depth, f.name.clone(), obj_kind(c, db, s, &f.name, Obj::Function { oid: f.oid, procedure }))
        .icon(if procedure { Icon::Procedure } else { Icon::Function })
        .meta(function_meta(f, s, pg), Meta::Plain)
}

fn sequence_row(c: &str, db: &str, s: &str, x: &dataman_core::schema::Sequence, depth: usize, pg: bool) -> Row {
    let meta = match (&x.owned_by, &x.type_name) {
        (Some((t, col)), _) => format!("{t}.{col}"),
        (None, Some(ty)) => short_type(ty, pg),
        (None, None) => String::new(),
    };
    let id = format!("{}/q:{}", gid(c, db, s, G::Sequences), x.name);
    Row::new(id, depth, x.name.clone(), obj_kind(c, db, s, &x.name, Obj::Sequence)).icon(Icon::Sequence).meta(clip(&meta, 30), Meta::Plain)
}

fn type_id(c: &str, db: &str, s: &str, name: &str) -> String {
    format!("{}/t:{name}", gid(c, db, s, G::Types))
}

fn type_row(c: &str, db: &str, s: &str, x: &dataman_core::schema::UserType, depth: usize, open: bool, pg: bool) -> Row {
    let meta = match (&x.kind, &x.base) {
        (TypeKind::Domain, Some(b)) => format!("domain {}", short_type(b, pg)),
        (k, _) => k.label().to_string(),
    };
    let row = Row::new(type_id(c, db, s, &x.name), depth, x.name.clone(), obj_kind(c, db, s, &x.name, Obj::Type)).icon(Icon::Type).meta(clip(&meta, 30), Meta::Plain);
    if x.kind == TypeKind::Enum && !x.labels.is_empty() { row.expandable(open) } else { row }
}

fn trigger_row(id: String, c: &str, db: &str, s: &str, x: &dataman_core::schema::Trigger, depth: usize, in_schema: bool) -> Row {
    let what = Obj::Trigger { table: x.table.clone(), enabled: x.enabled, definition: x.definition.clone() };
    let base = if in_schema { x.table.clone() } else { x.events.clone() };
    let row = Row::new(id, depth, x.name.clone(), obj_kind(c, db, s, &x.name, what)).icon(Icon::Trigger);
    if x.enabled {
        row.meta(clip(&base, 30), Meta::Plain)
    } else {
        row.tone(Tone::Muted).meta(format!("off · {}", clip(&base, 24)), Meta::Off)
    }
}

fn extension_row(c: &str, db: &str, x: &dataman_core::schema::Extension, depth: usize) -> Row {
    let meta = match x.update() {
        Some(v) => format!("{} → {v}", x.version),
        None => x.version.clone(),
    };
    let what = Obj::Extension { update: x.update().map(str::to_string) };
    Row::new(format!("{}/extensions/{}", did(c, db), x.name), depth, x.name.clone(), obj_kind(c, db, "", &x.name, what))
        .icon(Icon::Extension)
        .meta(clip(&meta, 24), Meta::Plain)
}

fn role_row(c: &str, r: &dataman_core::schema::Role, depth: usize) -> Row {
    let flags: Vec<&str> = [(r.superuser, "superuser"), (r.login, "login")].into_iter().filter(|(on, _)| *on).map(|(_, f)| f).collect();
    Row::new(format!("{}/roles/{}", cid(c), r.name), depth, r.name.clone(), obj_kind(c, "", "", &r.name, Obj::Role))
        .icon(Icon::Role)
        .meta(flags.join(" · "), Meta::Plain)
}

#[allow(clippy::too_many_arguments)]
fn menu_row(
    t: &Theme,
    id: impl Into<gpui::ElementId>,
    label: SharedString,
    ic: Icon,
    dot: Option<Hsla>,
    key: Option<&'static str>,
    disabled: Option<&'static str>,
    bold: bool,
    danger: bool,
    checked: bool,
    hi: bool,
    sub: bool,
) -> gpui::Stateful<gpui::Div> {
    let on = hi && disabled.is_none();
    let fg = if disabled.is_some() { t.tx4 } else if on { t.on_acc } else { t.tx };
    let icon_fg = if disabled.is_some() {
        t.tx4
    } else if on {
        t.on_acc
    } else if danger {
        t.err
    } else {
        t.tx3
    };
    let key_fg = if on { t.on_acc.opacity(0.8) } else if disabled.is_some() { t.tx4 } else { t.tx3 };
    div()
        .id(id)
        .h(px(menu::ITEM_H))
        .px(px(10.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.))
        .whitespace_nowrap()
        .text_color(fg)
        .when(on, |d| d.bg(t.acc))
        .child(match dot {
            Some(c) => div().size(px(14.)).flex_none().flex().items_center().justify_center().child(ui::chip(c, 9.)).into_any_element(),
            None => icon(ic).size(px(14.)).text_color(icon_fg).into_any_element(),
        })
        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().when(bold, |d| d.font_weight(FontWeight::SEMIBOLD)).child(label))
        .children(disabled.or(key).map(|k| div().flex_none().text_size(px(11.)).text_color(key_fg).child(crate::ui::keys_text(k))))
        .when(checked, |d| d.child(icon(Icon::Check).flex_none().size(px(12.)).text_color(if on { t.on_acc } else { t.tx2 })))
        .when(sub, |d| d.child(icon(Icon::ChevronRight).size(px(10.)).text_color(key_fg)))
}

fn coll_row(c: &str, db: &str, x: &dataman_core::nosql::Item, depth: usize) -> Row {
    let view = x.kind == dataman_core::nosql::ItemKind::View;
    let icon = match (&x.note, view) {
        (Some(_), _) => Icon::Key,
        (None, true) => Icon::View,
        (None, false) => Icon::Collection,
    };
    let mut r = Row::new(format!("{}/k:{}", did(c, db), x.name), depth, x.name.clone(), Kind::Coll { conn: c.into(), db: db.into(), name: x.name.clone() }).icon(icon);
    if let Some(n) = x.count {
        r = r.meta(approx_count(n as f64), Meta::Plain);
    }
    if let Some(t) = &x.note {
        r = r.meta(t.clone(), Meta::Plain);
    }
    if x.name.starts_with("system.") {
        r = r.tone(Tone::Muted);
    }
    r
}

fn loading_row(id: &str, depth: usize) -> Row {
    Row::new(format!("{id}/loading"), depth, "loading…", Kind::Leaf).tone(Tone::Italic)
}

fn error_row(id: &str, depth: usize, e: &str, retry: &str, t: &Theme) -> Row {
    let first = e.lines().next().unwrap_or(e);
    let mut r = Row::new(format!("{id}/err"), depth, first.to_string(), Kind::Error { retry: retry.to_string() })
        .tone(Tone::Error)
        .icon_colored(Icon::Warning, t.err)
        .meta("retry", Meta::Link);
    r.tip = Some(e.to_string().into());
    r
}

fn short_type(t: &str, pg: bool) -> String {
    if !pg {
        return t.to_string();
    }
    match t {
        "bigint" => "int8".into(),
        "integer" => "int4".into(),
        "smallint" => "int2".into(),
        "boolean" => "bool".into(),
        "double precision" => "float8".into(),
        "real" => "float4".into(),
        "timestamp with time zone" => "timestamptz".into(),
        "timestamp without time zone" => "timestamp".into(),
        "time without time zone" => "time".into(),
        "character varying" => "varchar".into(),
        _ if t.starts_with("character varying(") => format!("varchar{}", &t["character varying".len()..]),
        _ => t.to_string(),
    }
}

fn match_range(name: &str, q: &str) -> Option<Range<usize>> {
    let i = name.to_lowercase().find(q)?;
    (name.is_char_boundary(i) && name.is_char_boundary(i + q.len())).then(|| i..i + q.len())
}

fn nearest(q: &str, names: &[String]) -> Option<String> {
    let best = names.iter().map(|n| (edit_distance(q, &n.to_lowercase()), n)).min_by_key(|(d, n)| (*d, n.len()))?;
    (best.0 <= (q.chars().count() / 3).max(2)).then(|| best.1.clone())
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

type Load = std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>>>>;

fn unit<T: 'static>(t: dataman_core::Task<T>) -> Load {
    Box::pin(async move { t.await.map(|_| ()) })
}

fn join2<A: 'static, B: 'static>(a: dataman_core::Task<A>, b: dataman_core::Task<B>) -> Load {
    join_all(vec![unit(a), unit(b)])
}

fn join_all(loads: Vec<Load>) -> Load {
    Box::pin(async move {
        let mut first = Ok(());
        for l in loads {
            if let (Err(e), Ok(())) = (l.await, &first) {
                first = Err(e);
            }
        }
        first
    })
}

impl Render for SchemaTree {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.version(cx) != self.seen_version {
            self.rebuild(cx);
        }
        match &self.folder_turn {
            Some((_, at, opening)) if crate::motion::folder_fill(*at, *opening).is_some() => window.request_animation_frame(),
            Some(_) => self.folder_turn = None,
            None => {}
        }
        self.pop_now = match self.pop.as_ref().map(|p| (p.target.clone(), p.frame(Instant::now()))) {
            Some((id, Some(f))) => {
                window.request_animation_frame();
                Some((id, f))
            }
            Some((_, None)) => {
                self.pop = None;
                None
            }
            None => None,
        };
        if std::mem::take(&mut self.focus_back) {
            window.focus(&self.focus, cx);
        }
        let t = cx.theme();
        let focused = self.focus.contains_focused(window, cx);
        let filter_focused = self.filter.focus_handle(cx).is_focused(window);
        let filtering = self.filtering(cx);
        let empty = self.store.read(cx).profiles.is_empty();
        let filter_meta: Option<SharedString> = match &self.filter_stats {
            Some((m, total, _)) if filtering => Some(format!("{} of {}", ui_count(*m), ui_count(*total)).into()),
            _ => None,
        };
        let no_match = filtering && self.filter_stats.as_ref().is_some_and(|(m, _, _)| *m == 0);
        let menu_layer = self.render_menu(window, cx);
        let header = div()
            .h(px(34.))
            .flex_none()
            .pl(px(14.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.))
            .child(div().flex_1().child(ui::caps(&t, "EXPLORER")))
            .child(
                ui::icon_button(&t, "x-new", Icon::Plus, 24., 14.)
                    .tip(ui::tooltip("New connection", None, Some("⌘N")))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(TreeEvent::NewConnection))),
            )
            .child({
                let (hov, prs) = (t.hov, t.prs);
                div()
                    .children(ui::here(&"x-new-folder".into()))
                    .id("x-new-folder")
                    .flex_none()
                    .h(px(24.))
                    .px(px(4.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(1.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hov))
                    .active(move |s| s.bg(prs))
                    .tip(ui::tooltip("New folder", None, None))
                    .child(icon(Icon::Plus).size(px(9.)).text_color(t.tx3))
                    .child(icon(Icon::Schema).size(px(14.)).text_color(t.tx3))
                    .on_click(cx.listener(|t, _, window, cx| t.new_folder(None, window, cx)))
            })
            .child(
                ui::icon_button(&t, "x-reload", Icon::Reload, 24., 14.)
                    .on_click(cx.listener(|t, _, _, cx| t.refresh_selected(cx))),
            )
            .child(
                ui::icon_button(&t, "x-more", Icon::More, 24., 14.)
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .on_click(cx.listener(|t, ev: &ClickEvent, window, cx| t.more_clicked(None, Some(ev.position()), window, cx))),
            );
        let filter_box = div()
            .mx(px(8.))
            .mb(px(6.))
            .h(px(28.))
            .flex_none()
            .pl(px(8.))
            .pr(px(6.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.))
            .relative()
            .bg(t.hov)
            .border_1()
            .border_color(if filter_focused { t.mark_b } else { t.field_b })
            .when(filter_focused, |d| d.child(ui::halo_ring(t.mark)))
            .child(icon(Icon::Search).size(px(13.)).text_color(t.tx3))
            .child(div().flex_1().min_w_0().text_color(t.tx).child(self.filter.clone()))
            .children(filter_meta.map(|m| div().flex_none().text_size(px(10.5)).text_color(t.tx3).whitespace_nowrap().child(m)))
            .children(self.render_scope(&t, cx));
        let body: gpui::AnyElement = if empty {
            div()
                .flex_1()
                .pt(px(48.))
                .px(px(20.))
                .flex()
                .flex_col()
                .items_center()
                .child(ui::empty_state(&t, "[ EMPTY ]", 22., None))
                .into_any_element()
        } else if no_match {
            let near = self.filter_stats.as_ref().and_then(|(_, _, n)| n.clone());
            div()
                .flex_1()
                .pt(px(48.))
                .px(px(16.))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.))
                .text_center()
                .child(ui::pixel("[ NO MATCH ]", 20., t.tx4))
                .children(near.map(|n| {
                    div().flex().flex_row().text_color(t.tx2).child("Did you mean ").child(div().text_color(t.acc_t).child(n)).child("?")
                }))
                .into_any_element()
        } else {
            let theme = t.clone();
            uniform_list(
                "schema-tree",
                self.rows.len(),
                cx.processor(move |tree, range: std::ops::Range<usize>, _window, cx| {
                    range.map(|ix| tree.render_row(ix, &theme, focused, cx).into_any_element()).collect()
                }),
            )
            .track_scroll(&self.scroll)
            .flex_1()
            .pt(px(2.))
            .on_mouse_down(MouseButton::Right, cx.listener(|t, ev: &MouseDownEvent, window, cx| {
                if t.below_rows(ev.position) {
                    t.open_menu(None, MenuAnchor::Mouse(ev.position), window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }
            }))
            .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .text_size(px(12.))
            .text_color(t.tx2)
            .on_drag_move(cx.listener(|t, ev: &gpui::DragMoveEvent<DraggedConn>, _, cx| {
                let pos = ev.event.position;
                if !ev.bounds.contains(&pos) {
                    if t.conn_drop.take().is_some() {
                        cx.notify();
                    }
                    return;
                }
                // Under the last row: the end of the top level (out of any folder).
                if t.filter_stats.is_none() && t.below_rows(pos) {
                    let id = ev.drag(cx).0.clone();
                    let end = ConnDrop::At { folder: None, before: None };
                    let target = t.drop_result(&id, &end, cx).map(|_| end);
                    if target != t.conn_drop {
                        t.conn_drop = target;
                        cx.notify();
                    }
                }
            }))
            .on_drop(cx.listener(|t, d: &DraggedConn, _, cx| t.drop_conn(&d.0, cx)))
            .on_action(cx.listener(|t, _: &SelectPrev, _, cx| t.move_sel(-1, cx)))
            .on_action(cx.listener(|t, _: &SelectNext, _, cx| t.move_sel(1, cx)))
            .on_action(cx.listener(|t, _: &Expand, _, cx| {
                if let Some(ix) = t.selected_ix() {
                    t.set_expanded(ix, true, cx)
                }
            }))
            .on_action(cx.listener(|t, _: &Collapse, _, cx| {
                if let Some(ix) = t.selected_ix() {
                    t.set_expanded(ix, false, cx)
                }
            }))
            .on_action(cx.listener(|t, _: &Open, _, cx| {
                if let Some(ix) = t.selected_ix() {
                    t.open(ix, cx)
                }
            }))
            .on_action(cx.listener(|t, _: &ReloadNode, _, cx| {
                if let Some(ix) = t.selected_ix() {
                    t.reload_row(ix, cx);
                }
            }))
            .on_action(cx.listener(|t, _: &ShowRowActions, window, cx| t.show_row_actions(None, window, cx)))
            .on_action(cx.listener(|t, _: &EditConnection, _, cx| {
                if let Some(id) = t.selected_connection() {
                    cx.emit(TreeEvent::EditConnection(id));
                }
            }))
            .on_action(cx.listener(|t, _: &Disconnect, _, cx| {
                if let Some(id) = t.selected_connection() {
                    cx.emit(TreeEvent::Disconnect(id));
                }
            }))
            .on_action(cx.listener(|t, _: &FocusFilter, window, cx| {
                let f = t.filter.focus_handle(cx);
                window.focus(&f, cx);
            }))
            .on_action(cx.listener(|t, _: &Escape, window, cx| {
                if t.naming.take().is_some() {
                    window.focus(&t.focus, cx);
                    cx.notify();
                } else if t.scope_menu {
                    t.scope_menu = false;
                    cx.notify();
                } else {
                    t.clear_filter(cx);
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(|t, _, window, cx| {
                if !t.filter.focus_handle(cx).is_focused(window) {
                    window.focus(&t.focus, cx);
                }
            }))
            .child(header)
            .child(filter_box)
            .child(body)
            .children(menu_layer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dataman_core::schema::{Function, Relation, Trigger, UserType};

    #[test]
    fn groups_split_the_relations() {
        assert_eq!(G::Tables.holds(RelKind::Partitioned), Some(true));
        assert_eq!(G::Tables.holds(RelKind::Foreign), Some(true));
        assert_eq!(G::Tables.holds(RelKind::MaterializedView), Some(false));
        assert_eq!(G::Views.holds(RelKind::MaterializedView), Some(false), "materialized views have their own group");
        assert_eq!(G::MatViews.holds(RelKind::MaterializedView), Some(true));
        assert_eq!(G::Functions.holds(RelKind::Table), None);
        assert_eq!(gid("c", "d", "s", G::Tables), "c:c/d:d/s:s/tables");
        assert_eq!(G::MatViews.label(), "materialized views");
    }

    #[test]
    fn table_rows_show_estimates_and_sizes() {
        let st = RelStats { rows: Some(48_310.), bytes: Some(12 * 1024 * 1024) };
        assert_eq!(stats_meta(&st), ("~48k · 12 MB".to_string(), Some("~48k".to_string())));
        assert_eq!(stats_meta(&RelStats { rows: None, bytes: Some(8192) }), ("8 kB".to_string(), None), "never analyzed: the size alone");
        let rel = |kind| Relation { schema: "s".into(), name: "t".into(), kind, oid: None };
        let mut m = std::collections::HashMap::new();
        m.insert("t".to_string(), st);
        let r = rel_row("c", "d", "s", &rel(RelKind::Table), 4, false, Some(&m));
        assert_eq!((r.meta.as_ref(), r.meta_short.as_ref().map(|s| s.as_ref())), ("~48k · 12 MB", Some("~48k")));
        assert!(rel_row("c", "d", "s", &rel(RelKind::Table), 4, false, None).meta.is_empty());
        assert!(rel_row("c", "d", "s", &rel(RelKind::View), 4, false, Some(&m)).meta.is_empty());
        assert_eq!(rel_row("c", "d", "s", &rel(RelKind::Foreign), 4, false, Some(&m)).meta.as_ref(), "foreign");
        assert_eq!(rel_row("c", "d", "s", &rel(RelKind::Partitioned), 4, false, None).icon.map(|(i, _)| i), Some(Icon::Partition));
    }

    #[test]
    fn pinned_rows_say_where_they_live() {
        assert_eq!(pin_place("app", "public", "app", false), "public");
        assert_eq!(pin_place("analytics", "public", "app", false), "analytics.public");
        assert_eq!(pin_place("shop", "shop", "shop", true), "");
        assert_eq!(pin_place("crm", "crm", "shop", true), "crm");
        let p = pin_id("c", "d", "s", "orders");
        assert_ne!(p, rid("c", "d", "s", "orders"));
        assert!(p.starts_with(&format!("{}/", cid("c"))) && p.starts_with(&format!("{}/", pinned_id("c"))));
        assert_eq!(match_range("Orders", "ord"), Some(0..3));
        assert_eq!(match_range("big_orders", "ord"), Some(4..7));
        assert_eq!(match_range("items", "ord"), None);
    }

    #[test]
    fn object_rows() {
        let f = Function {
            oid: 7,
            name: "order_total".into(),
            kind: FuncKind::Function,
            identity: "o bigint".into(),
            arg_types: vec!["bigint".into(), "character varying".into()],
            result: Some("SETOF s.orders".into()),
            returns_set: true,
        };
        let r = function_row("c", "d", "s", &f, 4, true);
        assert_eq!(r.meta.as_ref(), "(int8, varchar) → setof orders");
        assert_eq!(r.id.as_ref(), "c:c/d:d/s:s/functions/f:7", "overloads differ by oid");
        let p = Function { kind: FuncKind::Procedure, result: None, arg_types: vec!["bigint".into(), "real".into()], ..f };
        assert_eq!(function_row("c", "d", "s", &p, 4, false).meta.as_ref(), "(bigint, real)", "other engines keep their names");
        let p = Function { arg_types: vec![], ..p };
        assert_eq!(function_row("c", "d", "s", &p, 4, true).meta.as_ref(), "()");
        assert_eq!(fit_meta_text("(int8) → numeric", None, 40), None, "fits");
        assert_eq!(fit_meta_text("(int8) → numeric", None, 10).as_deref(), Some("(int8) → …"));
        assert_eq!(fit_meta_text("(int8) → numeric", None, 5).as_deref(), Some(""), "the name first");
        assert_eq!(fit_meta_text("~48k · 12 MB", Some("~48k"), 6).as_deref(), Some("~48k"));
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abc", 4), "abc");
        let e = UserType { name: "mood".into(), kind: TypeKind::Enum, base: None, labels: vec!["a".into()] };
        assert!(type_row("c", "d", "s", &e, 4, false, true).expandable, "an enum opens on its labels");
        let d = UserType { name: "amount".into(), kind: TypeKind::Domain, base: Some("numeric(12,2)".into()), labels: vec![] };
        let dr = type_row("c", "d", "s", &d, 4, false, true);
        assert_eq!((dr.meta.as_ref(), dr.expandable), ("domain numeric(12,2)", false));
        let t = Trigger { name: "audit".into(), table: "orders".into(), enabled: false, events: "after delete".into(), definition: None };
        let tr = trigger_row("x".into(), "c", "d", "s", &t, 4, true);
        assert_eq!((tr.meta.as_ref(), tr.meta_kind == Meta::Off, tr.tone == Tone::Muted), ("off · orders", true, true));
        let on = Trigger { enabled: true, ..t };
        assert_eq!(trigger_row("x".into(), "c", "d", "s", &on, 6, false).meta.as_ref(), "after delete");
    }
}
