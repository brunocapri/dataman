//! The explorer's context menu, as data: which actions a row offers in its current state, the
//! keyboard movement over them and where the menu goes on screen. Everything here is a pure
//! function of what is already in memory: building a menu does no I/O (the DDL, a view's
//! definition or missing columns are read only when their item is chosen).
//!
//! The tree (`tree.rs`) draws it and does the local actions (reload, copy, insert); the
//! workspace (`row_actions.rs`) does the rest.

use dataman_core::Caps;
use dataman_core::schema::RelKind;

use crate::assets::Icon;

/// What a menu is about: a tree row, reduced to what its actions need.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// No row selected (the header's ⋯).
    Empty,
    Conn { id: String, name: String, state: ConnPhase },
    /// `ro`: the database is read-only (its own setting, else the connection's).
    Db { conn: String, db: String, current: bool, lar: bool, ro: bool },
    /// `ro`: the database is read-only (import is refused).
    Schema { conn: String, db: String, schema: String, lar: bool, ro: bool },
    /// A group row: "tables", "views", "columns", "indexes".
    Group,
    Rel { conn: String, db: String, schema: String, name: String, kind: RelKind, lar: bool, ro: bool },
    Column { name: String },
    Index { name: String, definition: Option<String> },
    /// A function, sequence, type, trigger, constraint, extension or role (`schema` is empty
    /// for extensions and roles, `db` for roles).
    Object { conn: String, db: String, schema: String, name: String, what: Obj },
    /// A failed load.
    Error,
}

/// The other object kinds, with what their menus need.
#[derive(Clone, Debug, PartialEq)]
pub enum Obj {
    Function { oid: u32, procedure: bool },
    Sequence,
    Type,
    EnumLabel,
    Trigger { table: String, enabled: bool, definition: Option<String> },
    Constraint { table: String, definition: Option<String> },
    Extension { update: Option<String> },
    Role,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnPhase {
    Off,
    Connecting,
    Connected,
    Failed,
}

/// Generated statements (Generate SQL ▸).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gen {
    Select,
    Insert,
    Update,
    Delete,
    CreateTable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    NewConnection,
    Connect,
    Disconnect,
    Reconnect,
    NewTab,
    Reload,
    History,
    EditConnection,
    DuplicateConnection,
    CopyUrl,
    DeleteConnection,
    CopyName,
    CopyQualified,
    CreateSchema,
    DropDatabase,
    CreateTable,
    DropSchema,
    SelectTop,
    SelectAll,
    CountRows,
    OpenWithoutRunning,
    Generate(Gen),
    Export,
    /// Import a file: into the table, or (on a schema) into a new table.
    ImportData,
    Truncate,
    DropTable,
    ShowDefinition,
    DropView,
    CopyDefinition,
    Retry,
    /// The grant matrix of a table or schema, in the side panel.
    Privileges,
    GenerateCall,
    SequenceValue,
    ResetSequence,
    EnableTrigger,
    DisableTrigger,
    AlterRole,
    ExtensionInfo,
    UpdateExtension,
    /// The connection's activity monitor (a tab).
    Activity,
    /// A database's Read Only (a check mark when on): saved with the connection, applied to
    /// its open sessions without reconnecting.
    ToggleReadOnly,
    /// A table or view into its connection's "pinned" group, and out of it (the tree's own).
    Pin,
    Unpin,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub act: Act,
    pub label: &'static str,
    pub icon: Icon,
    /// The shortcut that does the same on the selected row.
    pub key: Option<&'static str>,
    /// Why it is unavailable: a short hint drawn in tx4 in the right column, in place of
    /// the shortcut (the menu is 260 pt wide, so a few words at most).
    pub disabled: Option<&'static str>,
    /// The row's default action (double-click, ↩).
    pub bold: bool,
    /// Writes a destructive statement (never runs it).
    pub danger: bool,
    /// A setting that is on: a check mark in the right column.
    pub checked: bool,
}

impl Item {
    fn new(act: Act, label: &'static str, icon: Icon) -> Item {
        Item { act, label, icon, key: None, disabled: None, bold: false, danger: false, checked: false }
    }
    fn checked(mut self, on: bool) -> Item {
        self.checked = on;
        self
    }
    fn key(mut self, k: &'static str) -> Item {
        self.key = Some(k);
        self
    }
    fn off(mut self, why: Option<&'static str>) -> Item {
        self.disabled = why;
        self
    }
    fn bold(mut self) -> Item {
        self.bold = true;
        self
    }
    fn danger(mut self) -> Item {
        self.danger = true;
        self
    }
    pub fn enabled(&self) -> bool {
        self.disabled.is_none()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Item(Item),
    Sep,
    /// A submenu (Generate SQL ▸).
    Sub { label: &'static str, icon: Icon, items: Vec<Item> },
}

impl Entry {
    pub fn enabled(&self) -> bool {
        match self {
            Entry::Item(i) => i.enabled(),
            Entry::Sep => false,
            Entry::Sub { items, .. } => items.iter().any(Item::enabled),
        }
    }
}

pub const NOT_CONNECTED: &str = "connect first";
pub const READ_ONLY: &str = "read-only";

/// The menu of a row.
pub fn entries(t: &Target) -> Vec<Entry> {
    use Entry::{Item as I, Sep};
    let it = Item::new;
    match t {
        Target::Empty => vec![I(it(Act::NewConnection, "New connection…", Icon::Plus).key("⌘N"))],
        Target::Conn { state, .. } => {
            let connected = *state == ConnPhase::Connected;
            let offline = (!connected).then_some(NOT_CONNECTED);
            let link = match state {
                ConnPhase::Off => it(Act::Connect, "Connect", Icon::Plug).key("↩"),
                ConnPhase::Connecting => it(Act::Connect, "Connecting…", Icon::Plug).off(Some("waiting")),
                ConnPhase::Connected => it(Act::Disconnect, "Disconnect", Icon::Plug).key("⌘⌫"),
                ConnPhase::Failed => it(Act::Reconnect, "Reconnect", Icon::Reload),
            };
            let mut v = vec![I(link)];
            if connected {
                v.push(I(it(Act::Reconnect, "Reconnect", Icon::Reload)));
            }
            v.extend([
                I(it(Act::NewTab, "New query tab", Icon::Tab).key("⌘T").off(offline)),
                Sep,
                I(it(Act::Reload, "Reload schema", Icon::Reload).key("⌘R").off(offline)),
                I(it(Act::History, "Query history", Icon::History)),
                I(it(Act::Activity, "Activity monitor", Icon::Activity).off(offline)),
                Sep,
                I(it(Act::EditConnection, "Edit connection…", Icon::Edit).key("⌘I")),
                I(it(Act::DuplicateConnection, "Duplicate connection…", Icon::Copy)),
                I(it(Act::CopyUrl, "Copy connection string", Icon::Copy)),
                Sep,
                I(it(Act::DeleteConnection, "Delete connection…", Icon::Trash).danger()),
            ]);
            v
        }
        Target::Db { current, ro, .. } => vec![
            I(it(Act::NewTab, "New query tab", Icon::Tab)),
            I(it(Act::Reload, "Reload", Icon::Reload).key("⌘R")),
            I(it(Act::CopyName, "Copy name", Icon::Copy)),
            Sep,
            I(it(Act::ToggleReadOnly, "Read Only", Icon::Lock).checked(*ro)),
            Sep,
            I(it(Act::CreateSchema, "Create schema…", Icon::Schema)),
            I(it(Act::DropDatabase, "Drop database…", Icon::Trash)
                .danger()
                .off(current.then_some("in use"))),
        ],
        Target::Schema { lar, ro, .. } => vec![
            I(it(Act::NewTab, "New query tab", Icon::Tab)),
            I(it(Act::Reload, "Reload", Icon::Reload).key("⌘R")),
            I(it(Act::CopyName, "Copy name", Icon::Copy)),
            I(it(Act::Privileges, "Privileges", Icon::Lock).off(lar.then_some("not reported"))),
            Sep,
            I(it(Act::CreateTable, "Create table…", Icon::Table)),
            I(it(Act::ImportData, "Import data…", Icon::Import).off(ro.then_some(READ_ONLY))),
            I(it(Act::DropSchema, "Drop schema…", Icon::Trash).danger()),
        ],
        Target::Rel { kind, lar, .. } if kind.is_view() => vec![
            I(it(Act::SelectTop, "Select 1000 rows", Icon::Run).key("↩").bold()),
            I(it(Act::CountRows, "Count rows", Icon::Hash)),
            Sep,
            I(it(Act::CopyName, "Copy name", Icon::Copy)),
            I(it(Act::CopyQualified, "Copy qualified name", Icon::Copy)),
            Sep,
            I(it(Act::ShowDefinition, "Show definition", Icon::Code).off(lar.then_some("not reported"))),
            I(it(Act::Generate(Gen::Select), "Generate SELECT", Icon::Code)),
            Sep,
            I(it(Act::DropView, "Drop view…", Icon::Trash).danger()),
        ],
        Target::Rel { lar, ro, .. } => vec![
            I(it(Act::SelectTop, "Select 1000 rows", Icon::Run).key("↩").bold()),
            I(it(Act::SelectAll, "Select all rows", Icon::Run)),
            I(it(Act::CountRows, "Count rows", Icon::Hash)),
            I(it(Act::OpenWithoutRunning, "Open without running", Icon::Tab)),
            Sep,
            I(it(Act::CopyName, "Copy name", Icon::Copy)),
            I(it(Act::CopyQualified, "Copy qualified name", Icon::Copy)),
            Sep,
            Entry::Sub {
                label: "Generate SQL",
                icon: Icon::Code,
                items: vec![
                    it(Act::Generate(Gen::Select), "SELECT", Icon::Code),
                    it(Act::Generate(Gen::Insert), "INSERT", Icon::Code),
                    it(Act::Generate(Gen::Update), "UPDATE", Icon::Code),
                    it(Act::Generate(Gen::Delete), "DELETE", Icon::Code),
                    it(Act::Generate(Gen::CreateTable), "CREATE TABLE", Icon::Code),
                ],
            },
            I(it(Act::Export, "Export…", Icon::Export)),
            I(it(Act::ImportData, "Import data…", Icon::Import).off(ro.then_some(READ_ONLY))),
            I(it(Act::Privileges, "Privileges", Icon::Lock).off(lar.then_some("not reported"))),
            I(it(Act::Reload, "Reload", Icon::Reload).key("⌘R")),
            Sep,
            I(it(Act::Truncate, "Truncate…", Icon::Trash).danger()),
            I(it(Act::DropTable, "Drop table…", Icon::Trash).danger()),
        ],
        Target::Group => vec![I(it(Act::Reload, "Reload", Icon::Reload).key("⌘R"))],
        Target::Column { .. } => vec![I(it(Act::CopyName, "Copy name", Icon::Copy))],
        Target::Index { definition, .. } => vec![
            I(it(Act::CopyDefinition, "Copy definition", Icon::Copy).off(definition.is_none().then_some("not reported"))),
            I(it(Act::CopyName, "Copy name", Icon::Copy)),
        ],
        Target::Error => vec![I(it(Act::Retry, "Retry", Icon::Reload))],
        Target::Object { what, .. } => object_entries(what),
    }
}

/// The menus of the other object kinds. Statements that change something open as templates.
fn object_entries(what: &Obj) -> Vec<Entry> {
    use Entry::{Item as I, Sep};
    let it = Item::new;
    let copy = I(it(Act::CopyName, "Copy name", Icon::Copy));
    let qualified = I(it(Act::CopyQualified, "Copy qualified name", Icon::Copy));
    match what {
        Obj::Function { .. } => vec![
            I(it(Act::ShowDefinition, "Show definition", Icon::Code).key("↩").bold()),
            I(it(Act::GenerateCall, "Generate call", Icon::Code)),
            Sep,
            copy,
            qualified,
        ],
        Obj::Sequence => vec![
            I(it(Act::SequenceValue, "Current value", Icon::Hash).key("↩").bold()),
            Sep,
            copy,
            qualified,
            Sep,
            I(it(Act::ResetSequence, "Reset…", Icon::Reload).danger()),
        ],
        Obj::Type => vec![I(it(Act::ShowDefinition, "Show definition", Icon::Code).key("↩").bold()), Sep, copy, qualified],
        Obj::EnumLabel => vec![copy],
        Obj::Trigger { enabled, definition, .. } => vec![
            I(it(Act::ShowDefinition, "Show definition", Icon::Code).key("↩").bold().off(definition.is_none().then_some("not reported"))),
            I(it(Act::CopyDefinition, "Copy definition", Icon::Copy).off(definition.is_none().then_some("not reported"))),
            copy,
            Sep,
            I(it(Act::EnableTrigger, "Enable…", Icon::Check).off(enabled.then_some("enabled"))),
            I(it(Act::DisableTrigger, "Disable…", Icon::Stop).danger().off((!enabled).then_some("disabled"))),
        ],
        Obj::Constraint { definition, .. } => {
            vec![I(it(Act::CopyDefinition, "Copy definition", Icon::Copy).off(definition.is_none().then_some("not reported"))), copy]
        }
        Obj::Extension { update } => vec![
            I(it(Act::ExtensionInfo, "Version info", Icon::Info).key("↩").bold()),
            I(it(Act::UpdateExtension, "Update…", Icon::Reload).off(update.is_none().then_some("up to date"))),
            Sep,
            copy,
        ],
        Obj::Role => vec![
            I(it(Act::ShowDefinition, "Show definition", Icon::Code).key("↩").bold()),
            I(it(Act::AlterRole, "Alter role…", Icon::Edit)),
            Sep,
            copy,
        ],
    }
}

/// The menu of a row on an engine with `caps`: what the engine cannot do is left out (never
/// shown disabled), and a database that is its own schema (MySQL) creates tables itself.
pub fn entries_for(t: &Target, caps: &Caps) -> Vec<Entry> {
    let mut es = entries(t);
    if !caps.schemas {
        for e in &mut es {
            if let Entry::Item(i) = e
                && i.act == Act::CreateSchema
            {
                *i = Item::new(Act::CreateTable, "Create table…", Icon::Table);
            }
        }
    }
    let keep = |a: Act| match a {
        Act::Activity => caps.activity,
        Act::ImportData => caps.import,
        Act::Privileges => caps.privileges,
        Act::EnableTrigger | Act::DisableTrigger => caps.toggle_triggers,
        Act::SequenceValue | Act::ResetSequence => caps.sequences,
        Act::ExtensionInfo | Act::UpdateExtension => caps.extensions,
        Act::AlterRole => caps.roles,
        Act::CreateSchema | Act::DropSchema => caps.schemas,
        _ => true,
    };
    es.retain_mut(|e| match e {
        Entry::Item(i) => keep(i.act),
        Entry::Sub { items, .. } => {
            items.retain(|i| keep(i.act));
            !items.is_empty()
        }
        Entry::Sep => true,
    });
    // No separator first, last or twice in a row once items are gone.
    let mut out: Vec<Entry> = Vec::with_capacity(es.len());
    for e in es {
        if matches!(e, Entry::Sep) && out.last().is_none_or(|l| matches!(l, Entry::Sep)) {
            continue;
        }
        out.push(e);
    }
    while matches!(out.last(), Some(Entry::Sep)) {
        out.pop();
    }
    out
}

/// A table's or view's menu with Pin (or Unpin, when `pinned`) in a section of its own after
/// the opening items; other menus as they are. Pins live outside the menu model (the tree's
/// store), so this is a step after [`entries_for`].
pub fn with_pin(mut es: Vec<Entry>, t: &Target, pinned: bool) -> Vec<Entry> {
    let Target::Rel { kind, .. } = t else { return es };
    let view = kind.is_view();
    let item = match (pinned, view) {
        (false, false) => Item::new(Act::Pin, "Pin table", Icon::Pin),
        (false, true) => Item::new(Act::Pin, "Pin view", Icon::Pin),
        (true, false) => Item::new(Act::Unpin, "Unpin table", Icon::Pin),
        (true, true) => Item::new(Act::Unpin, "Unpin view", Icon::Pin),
    };
    match es.iter().position(|e| matches!(e, Entry::Sep)) {
        Some(i) => {
            es.insert(i, Entry::Item(item));
            es.insert(i, Entry::Sep);
        }
        None => es.push(Entry::Item(item)),
    }
    es
}

/// The row's default action (↩, double-click): its bold item, when enabled.
pub fn default_act(t: &Target) -> Option<Act> {
    entries(t).into_iter().find_map(|e| match e {
        Entry::Item(i) if i.bold && i.enabled() => Some(i.act),
        _ => None,
    })
}

// ---------------------------------------------------------------------------------------------
// Geometry and movement

pub const ITEM_H: f32 = 26.;
pub const SEP_H: f32 = 9.;
pub const PAD: f32 = 4.;
pub const MENU_W: f32 = 260.;
pub const SUB_W: f32 = 236.;
/// Kept free around a menu (the window edge).
pub const MARGIN: f32 = 8.;

pub fn entry_h(e: &Entry) -> f32 {
    if matches!(e, Entry::Sep) { SEP_H } else { ITEM_H }
}

pub fn menu_h(entries: &[Entry]) -> f32 {
    entries.iter().map(entry_h).sum::<f32>() + 2. * PAD
}

pub fn sub_h(items: &[Item]) -> f32 {
    items.len() as f32 * ITEM_H + 2. * PAD
}

/// The y of entry `ix` inside its menu.
pub fn entry_top(entries: &[Entry], ix: usize) -> f32 {
    PAD + entries.iter().take(ix).map(entry_h).sum::<f32>()
}

/// The next enabled entry from `from` in direction `dir` (±1), wrapping; the first (or last)
/// one when nothing is highlighted. Separators and disabled items are skipped, as on macOS.
pub fn step(enabled: &[bool], from: Option<usize>, dir: isize) -> Option<usize> {
    let n = enabled.len();
    if n == 0 || !enabled.iter().any(|e| *e) {
        return None;
    }
    let mut i = match from {
        Some(i) => i as isize,
        None if dir > 0 => -1,
        None => n as isize,
    };
    for _ in 0..n {
        i = (i + dir).rem_euclid(n as isize);
        if enabled[i as usize] {
            return Some(i as usize);
        }
    }
    None
}

/// Where a `w × h` menu goes for an anchor point: below and to the right of it, flipped above
/// (or to the left) when it would leave the window, then clamped inside it with a margin.
/// `above` is the y to flip to (the top of the row the menu belongs to; the point itself for
/// the mouse). Returns the top-left corner.
pub fn place(anchor: (f32, f32), above: f32, w: f32, h: f32, viewport: (f32, f32)) -> (f32, f32) {
    let (vw, vh) = viewport;
    let mut x = anchor.0;
    if x + w > vw - MARGIN {
        x = (anchor.0 - w).max(MARGIN);
    }
    let mut y = anchor.1;
    if y + h > vh - MARGIN && above - h >= MARGIN {
        y = above - h;
    }
    x = x.min(vw - MARGIN - w).max(MARGIN);
    y = y.min(vh - MARGIN - h).max(MARGIN);
    (x, y)
}

/// Where a submenu goes: right of its parent menu at the item's height, or left of it when
/// there is no room on the right; clamped inside the window.
pub fn place_sub(menu: (f32, f32), item_top: f32, h: f32, viewport: (f32, f32)) -> (f32, f32) {
    let (vw, vh) = viewport;
    let right = menu.0 + MENU_W - 2.;
    let x = if right + SUB_W <= vw - MARGIN { right } else { (menu.0 - SUB_W + 2.).max(MARGIN) };
    let y = (menu.1 + item_top - PAD).min(vh - MARGIN - h).max(MARGIN);
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acts(t: &Target) -> Vec<Act> {
        entries(t)
            .into_iter()
            .flat_map(|e| match e {
                Entry::Item(i) => vec![i.act],
                Entry::Sub { items, .. } => items.into_iter().map(|i| i.act).collect(),
                Entry::Sep => vec![],
            })
            .collect()
    }

    fn item(t: &Target, act: Act) -> Item {
        entries(t)
            .into_iter()
            .flat_map(|e| match e {
                Entry::Item(i) => vec![i],
                Entry::Sub { items, .. } => items,
                Entry::Sep => vec![],
            })
            .find(|i| i.act == act)
            .unwrap_or_else(|| panic!("{act:?} not in the menu of {t:?}"))
    }

    fn conn(state: ConnPhase) -> Target {
        Target::Conn { id: "c1".into(), name: "local".into(), state }
    }

    fn rel(kind: RelKind, lar: bool) -> Target {
        Target::Rel { conn: "c1".into(), db: "app".into(), schema: "public".into(), name: "orders".into(), kind, lar, ro: false }
    }

    #[test]
    fn connection_menu_follows_the_state() {
        let off = conn(ConnPhase::Off);
        assert_eq!(acts(&off)[0], Act::Connect);
        assert!(!acts(&off).contains(&Act::Disconnect));
        assert_eq!(item(&off, Act::NewTab).disabled, Some(NOT_CONNECTED));
        assert_eq!(item(&off, Act::Reload).disabled, Some(NOT_CONNECTED));
        // Things that work offline stay enabled.
        for a in [Act::History, Act::EditConnection, Act::DuplicateConnection, Act::CopyUrl, Act::DeleteConnection] {
            assert!(item(&off, a).enabled(), "{a:?}");
        }
        let on = conn(ConnPhase::Connected);
        assert_eq!(&acts(&on)[..3], &[Act::Disconnect, Act::Reconnect, Act::NewTab]);
        assert_eq!(item(&on, Act::Disconnect).key, Some("⌘⌫"));
        assert!(item(&on, Act::NewTab).enabled() && item(&on, Act::Reload).enabled());
        let failed = conn(ConnPhase::Failed);
        assert_eq!(acts(&failed)[0], Act::Reconnect);
        assert_eq!(acts(&failed).iter().filter(|a| **a == Act::Reconnect).count(), 1);
        let connecting = conn(ConnPhase::Connecting);
        assert!(!item(&connecting, Act::Connect).enabled());
        assert!(item(&on, Act::DeleteConnection).danger);
        assert_eq!(item(&on, Act::Reload).key, Some("⌘R"));
        assert_eq!(item(&on, Act::EditConnection).key, Some("⌘I"));
        assert_eq!(item(&on, Act::History).key, None);
    }

    #[test]
    fn database_menu_guards_the_current_database() {
        let cur = Target::Db { conn: "c1".into(), db: "app".into(), current: true, lar: false, ro: false };
        let other = Target::Db { conn: "c1".into(), db: "old".into(), current: false, lar: false, ro: true };
        assert_eq!(item(&cur, Act::DropDatabase).disabled, Some("in use"));
        assert!(item(&other, Act::DropDatabase).enabled());
        assert_eq!(acts(&other), vec![Act::NewTab, Act::Reload, Act::CopyName, Act::ToggleReadOnly, Act::CreateSchema, Act::DropDatabase]);
        // Read Only: a check mark when on, never disabled.
        assert!(item(&other, Act::ToggleReadOnly).checked && item(&other, Act::ToggleReadOnly).enabled());
        assert!(!item(&cur, Act::ToggleReadOnly).checked);
    }

    #[test]
    fn schema_menu() {
        let s = Target::Schema { conn: "c1".into(), db: "app".into(), schema: "public".into(), lar: false, ro: false };
        assert_eq!(acts(&s), vec![Act::NewTab, Act::Reload, Act::CopyName, Act::Privileges, Act::CreateTable, Act::ImportData, Act::DropSchema]);
        assert!(item(&s, Act::DropSchema).danger);
        assert!(item(&s, Act::Privileges).enabled() && item(&s, Act::ImportData).enabled());
        let lar = Target::Schema { conn: "c1".into(), db: "app".into(), schema: "public".into(), lar: true, ro: false };
        assert_eq!(item(&lar, Act::Privileges).disabled, Some("not reported"));
        // Read-only: import is refused, the rest stays.
        let ro = Target::Schema { conn: "c1".into(), db: "app".into(), schema: "public".into(), lar: false, ro: true };
        assert_eq!(item(&ro, Act::ImportData).disabled, Some(READ_ONLY));
        assert!(item(&ro, Act::CreateTable).enabled());
    }

    #[test]
    fn import_on_tables_only() {
        let t = rel(RelKind::Table, false);
        assert!(item(&t, Act::ImportData).enabled());
        let ro = Target::Rel { conn: "c1".into(), db: "app".into(), schema: "public".into(), name: "orders".into(), kind: RelKind::Table, lar: false, ro: true };
        assert_eq!(item(&ro, Act::ImportData).disabled, Some(READ_ONLY));
        assert!(!acts(&rel(RelKind::View, false)).contains(&Act::ImportData));
    }

    fn obj(what: Obj) -> Target {
        Target::Object { conn: "c1".into(), db: "app".into(), schema: "public".into(), name: "x".into(), what }
    }

    #[test]
    fn object_menus() {
        let f = obj(Obj::Function { oid: 1, procedure: false });
        assert_eq!(acts(&f), vec![Act::ShowDefinition, Act::GenerateCall, Act::CopyName, Act::CopyQualified]);
        assert_eq!(default_act(&f), Some(Act::ShowDefinition));
        let s = obj(Obj::Sequence);
        assert_eq!(acts(&s), vec![Act::SequenceValue, Act::CopyName, Act::CopyQualified, Act::ResetSequence]);
        assert!(item(&s, Act::ResetSequence).danger, "Reset… writes a destructive template");
        assert_eq!(acts(&obj(Obj::Type)), vec![Act::ShowDefinition, Act::CopyName, Act::CopyQualified]);
        assert_eq!(acts(&obj(Obj::EnumLabel)), vec![Act::CopyName]);
        assert_eq!(default_act(&obj(Obj::EnumLabel)), None);
        // A trigger offers the toggle it does not have.
        let on = obj(Obj::Trigger { table: "t".into(), enabled: true, definition: Some("CREATE TRIGGER …".into()) });
        assert_eq!(item(&on, Act::EnableTrigger).disabled, Some("enabled"));
        assert!(item(&on, Act::DisableTrigger).enabled() && item(&on, Act::DisableTrigger).danger);
        let off = obj(Obj::Trigger { table: "t".into(), enabled: false, definition: None });
        assert!(item(&off, Act::EnableTrigger).enabled());
        assert_eq!(item(&off, Act::DisableTrigger).disabled, Some("disabled"));
        assert_eq!(default_act(&off), None, "no definition reported: ↩ does nothing");
        let c = obj(Obj::Constraint { table: "t".into(), definition: None });
        assert_eq!(item(&c, Act::CopyDefinition).disabled, Some("not reported"));
        let e = obj(Obj::Extension { update: None });
        assert_eq!(item(&e, Act::UpdateExtension).disabled, Some("up to date"));
        assert!(item(&obj(Obj::Extension { update: Some("1.6".into()) }), Act::UpdateExtension).enabled());
        assert_eq!(acts(&obj(Obj::Role)), vec![Act::ShowDefinition, Act::AlterRole, Act::CopyName]);
        // Tables and views keep their default.
        assert_eq!(default_act(&rel(RelKind::Table, false)), Some(Act::SelectTop));
        for t in [f, s, on, e] {
            assert!(menu_h(&entries(&t)) < 600. - 2. * MARGIN - 40.);
        }
    }

    #[test]
    fn table_menu_has_the_default_first_and_generate_sql_as_a_submenu() {
        let t = rel(RelKind::Table, false);
        let es = entries(&t);
        let Entry::Item(first) = &es[0] else { panic!() };
        assert_eq!((first.act, first.label, first.bold, first.key), (Act::SelectTop, "Select 1000 rows", true, Some("↩")));
        let sub = es.iter().find_map(|e| if let Entry::Sub { items, .. } = e { Some(items.clone()) } else { None }).unwrap();
        assert_eq!(
            sub.iter().map(|i| i.act).collect::<Vec<_>>(),
            [Gen::Select, Gen::Insert, Gen::Update, Gen::Delete, Gen::CreateTable].map(Act::Generate).to_vec()
        );
        for a in [Act::SelectAll, Act::CountRows, Act::OpenWithoutRunning, Act::CopyName, Act::CopyQualified, Act::Export, Act::ImportData, Act::Privileges, Act::Reload] {
            assert!(item(&t, a).enabled(), "{a:?}");
        }
        assert_eq!(item(&rel(RelKind::Table, true), Act::Privileges).disabled, Some("not reported"));
        assert!(item(&t, Act::Truncate).danger && item(&t, Act::DropTable).danger);
        // Only the table's default is bold; destructive items come last.
        assert_eq!(es.iter().filter(|e| matches!(e, Entry::Item(i) if i.bold)).count(), 1);
        assert!(matches!(es.last(), Some(Entry::Item(i)) if i.act == Act::DropTable));
        // Partitioned and foreign tables are tables.
        assert!(acts(&rel(RelKind::Partitioned, false)).contains(&Act::Truncate));
        // The same items on LAR (the DDL says what LAR does not report).
        assert_eq!(acts(&rel(RelKind::Table, true)), acts(&t));
    }

    #[test]
    fn view_menu() {
        let v = rel(RelKind::View, false);
        assert_eq!(
            acts(&v),
            vec![
                Act::SelectTop,
                Act::CountRows,
                Act::CopyName,
                Act::CopyQualified,
                Act::ShowDefinition,
                Act::Generate(Gen::Select),
                Act::DropView
            ]
        );
        assert!(item(&v, Act::ShowDefinition).enabled());
        assert!(item(&v, Act::SelectTop).bold);
        let lar = rel(RelKind::View, true);
        assert!(item(&lar, Act::ShowDefinition).disabled.unwrap().starts_with("not reported"));
        assert!(acts(&rel(RelKind::MaterializedView, false)).contains(&Act::DropView));
        assert!(!acts(&v).contains(&Act::Truncate));
    }

    /// Pin table / Unpin table: its own section after the opening items, never the default,
    /// only on tables and views; still fits a small window.
    #[test]
    fn pinning_tables_and_views() {
        let pg = Caps::of(dataman_core::config::Engine::Postgres);
        let flat = |es: &[Entry]| -> Vec<Option<Act>> {
            es.iter()
                .map(|e| match e {
                    Entry::Item(i) => Some(i.act),
                    _ => None,
                })
                .collect()
        };
        let t = rel(RelKind::Table, false);
        let es = with_pin(entries_for(&t, &pg), &t, false);
        assert_eq!(
            &flat(&es)[..7],
            &[Some(Act::SelectTop), Some(Act::SelectAll), Some(Act::CountRows), Some(Act::OpenWithoutRunning), None, Some(Act::Pin), None]
        );
        let pin = es.iter().find_map(|e| if let Entry::Item(i) = e { (i.act == Act::Pin).then(|| i.clone()) } else { None }).unwrap();
        assert_eq!((pin.label, pin.icon, pin.enabled(), pin.bold), ("Pin table", Icon::Pin, true, false));
        assert_eq!(es.len(), entries_for(&t, &pg).len() + 2);
        // Pinned: Unpin in its place; the default stays Select.
        let un = with_pin(entries_for(&t, &pg), &t, true);
        assert!(flat(&un).contains(&Some(Act::Unpin)) && !flat(&un).contains(&Some(Act::Pin)));
        assert_eq!(default_act(&t), Some(Act::SelectTop));
        // Views say so.
        let v = rel(RelKind::View, false);
        let labels: Vec<&str> = with_pin(entries(&v), &v, true).iter().filter_map(|e| if let Entry::Item(i) = e { Some(i.label) } else { None }).collect();
        assert!(labels.contains(&"Unpin view"), "{labels:?}");
        // No separator twice in a row, first or last.
        for es in [&es, &un] {
            assert!(!matches!(es.first(), Some(Entry::Sep)) && !matches!(es.last(), Some(Entry::Sep)));
            assert!(!es.windows(2).any(|w| matches!(w, [Entry::Sep, Entry::Sep])));
        }
        assert!(menu_h(&es) < 600. - 2. * MARGIN - 40.);
        // Other rows: unchanged.
        for other in [conn(ConnPhase::Connected), Target::Group, Target::Column { name: "id".into() }, obj(Obj::Sequence)] {
            assert_eq!(with_pin(entries(&other), &other, false), entries(&other), "{other:?}");
        }
    }

    #[test]
    fn leaves_groups_and_errors() {
        assert_eq!(acts(&Target::Column { name: "id".into() }), vec![Act::CopyName]);
        let ix = Target::Index { name: "i".into(), definition: Some("CREATE INDEX i ON t (a)".into()) };
        assert!(item(&ix, Act::CopyDefinition).enabled());
        let lar_ix = Target::Index { name: "i".into(), definition: None };
        assert!(!item(&lar_ix, Act::CopyDefinition).enabled());
        assert_eq!(acts(&Target::Group), vec![Act::Reload]);
        assert_eq!(acts(&Target::Error), vec![Act::Retry]);
        assert_eq!(acts(&Target::Empty), vec![Act::NewConnection]);
    }

    #[test]
    fn every_menu_fits_a_small_window() {
        // 600 pt is the window's minimum height.
        for t in [conn(ConnPhase::Connected), rel(RelKind::Table, false), rel(RelKind::View, false)] {
            assert!(menu_h(&entries(&t)) < 600. - 2. * MARGIN - 40., "{t:?}");
        }
    }

    /// MySQL: what the engine cannot do is not there at all, separators stay tidy.
    #[test]
    fn engine_caps_hide_items() {
        let my = Caps::of(dataman_core::config::Engine::MySql);
        let pg = Caps::of(dataman_core::config::Engine::Postgres);
        let flat = |es: Vec<Entry>| -> Vec<Act> {
            es.into_iter()
                .flat_map(|e| match e {
                    Entry::Item(i) => vec![i.act],
                    Entry::Sub { items, .. } => items.into_iter().map(|i| i.act).collect(),
                    Entry::Sep => vec![],
                })
                .collect()
        };
        let db = Target::Db { conn: "c1".into(), db: "shop".into(), current: false, lar: false, ro: false };
        assert_eq!(flat(entries_for(&db, &my)), vec![Act::NewTab, Act::Reload, Act::CopyName, Act::ToggleReadOnly, Act::CreateTable, Act::DropDatabase]);
        assert_eq!(flat(entries_for(&db, &pg)), acts(&db), "PostgreSQL unchanged");
        let t = flat(entries_for(&rel(RelKind::Table, false), &my));
        assert!(!t.contains(&Act::ImportData) && !t.contains(&Act::Privileges) && t.contains(&Act::Export));
        let c = flat(entries_for(&conn(ConnPhase::Connected), &my));
        assert!(!c.contains(&Act::Activity) && c.contains(&Act::History));
        let tr = obj(Obj::Trigger { table: "t".into(), enabled: true, definition: Some("CREATE TRIGGER …".into()) });
        let es = entries_for(&tr, &my);
        assert_eq!(flat(es.clone()), vec![Act::ShowDefinition, Act::CopyDefinition, Act::CopyName]);
        assert!(!matches!(es.last(), Some(Entry::Sep)) && !es.windows(2).any(|w| matches!(w, [Entry::Sep, Entry::Sep])));
        // SQL Server: schemas and their objects, triggers that can be disabled; no monitor,
        // imports, privileges or roles.
        let ms = Caps::of(dataman_core::config::Engine::SqlServer);
        assert!(flat(entries_for(&db, &ms)).contains(&Act::CreateSchema));
        let t = flat(entries_for(&rel(RelKind::Table, false), &ms));
        assert!(!t.contains(&Act::ImportData) && !t.contains(&Act::Privileges) && t.contains(&Act::Export));
        assert!(!flat(entries_for(&conn(ConnPhase::Connected), &ms)).contains(&Act::Activity));
        assert!(flat(entries_for(&tr, &ms)).contains(&Act::DisableTrigger));
    }

    #[test]
    fn stepping_skips_separators_and_disabled_items() {
        let en = [true, false, true, false, true];
        assert_eq!(step(&en, None, 1), Some(0));
        assert_eq!(step(&en, None, -1), Some(4));
        assert_eq!(step(&en, Some(0), 1), Some(2));
        assert_eq!(step(&en, Some(2), 1), Some(4));
        assert_eq!(step(&en, Some(4), 1), Some(0), "wraps");
        assert_eq!(step(&en, Some(0), -1), Some(4), "wraps back");
        assert_eq!(step(&[false, false], None, 1), None);
        assert_eq!(step(&[], None, 1), None);
        // The real table menu: ↓ from the top lands on items, never on a separator.
        let es = entries(&rel(RelKind::Table, false));
        let enabled: Vec<bool> = es.iter().map(Entry::enabled).collect();
        let mut at = None;
        for _ in 0..es.len() * 2 {
            at = step(&enabled, at, 1);
            assert!(!matches!(es[at.unwrap()], Entry::Sep));
        }
    }

    #[test]
    fn placement_stays_inside_the_window() {
        let vp = (1000., 700.);
        // Room below and right: at the point.
        assert_eq!(place((100., 100.), 100., 260., 300., vp), (100., 100.));
        // Near the bottom: flips above the row.
        assert_eq!(place((100., 600.), 578., 260., 300., vp), (100., 278.));
        // Near the right edge: to the left of the point.
        assert_eq!(place((900., 100.), 100., 260., 300., vp), (640., 100.));
        // Taller than the room on both sides: clamped with the margin.
        assert_eq!(place((100., 300.), 300., 260., 690., vp), (100., MARGIN));
        let (x, y) = place((-50., -50.), -50., 260., 300., vp);
        assert_eq!((x, y), (MARGIN, MARGIN));
        // Submenus: right of the menu, or left of it at the window's edge.
        assert_eq!(place_sub((100., 100.), 40., 200., vp), (100. + MENU_W - 2., 136.));
        assert_eq!(place_sub((800., 100.), 40., 200., vp), (800. - SUB_W + 2., 136.));
        assert_eq!(place_sub((100., 600.), 200., 200., vp).1, 700. - MARGIN - 200.);
    }

    #[test]
    fn heights_add_up() {
        let es = entries(&Target::Schema { conn: "c".into(), db: "d".into(), schema: "s".into(), lar: false, ro: false });
        assert_eq!(menu_h(&es), 7. * ITEM_H + SEP_H + 2. * PAD);
        assert_eq!(entry_top(&es, 0), PAD);
        assert_eq!(entry_top(&es, 5), PAD + 4. * ITEM_H + SEP_H);
    }
}
