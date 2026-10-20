//! Embedded assets: the icon set (one stroke path per icon, 16-unit grid) and the welcome
//! illustration. Everything is compiled in (`include_bytes!`); there is no file IO at runtime.
//!
//! Icons render with `svg()`, tinted by the svg element's own text color (it is not inherited
//! from the parent, so always set it): `icon(Icon::Run).size(px(14.)).text_color(t.tx2)`.
//! GPUI rasterizes each one once per size.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString, Svg, prelude::*, svg};

macro_rules! assets {
    ($($path:literal),* $(,)?) => {
        static FILES: &[(&str, &[u8])] = &[$(($path, include_bytes!(concat!("../assets/", $path)))),*];
    };
}

assets!(
    "icons/activity.svg",
    "icons/agent.svg",
    "icons/braces.svg",
    "icons/arrow-right.svg",
    "icons/check.svg",
    "icons/chevron-d.svg",
    "icons/chevron-l.svg",
    "icons/chevron-r.svg",
    "icons/chevron-u.svg",
    "icons/close.svg",
    "icons/code.svg",
    "icons/collapse.svg",
    "icons/expand.svg",
    "icons/command.svg",
    "icons/constraint.svg",
    "icons/copy.svg",
    "icons/database.svg",
    "icons/edit.svg",
    "icons/export.svg",
    "icons/extension.svg",
    "icons/filter.svg",
    "icons/function.svg",
    "icons/group.svg",
    "icons/hash.svg",
    "icons/history.svg",
    "icons/import.svg",
    "icons/index.svg",
    "icons/info.svg",
    "icons/key.svg",
    "icons/keyboard.svg",
    "icons/label.svg",
    "icons/link.svg",
    "icons/live.svg",
    "icons/lock.svg",
    "icons/matview.svg",
    "icons/more.svg",
    "icons/panel-right.svg",
    "icons/partition.svg",
    "icons/pause.svg",
    "icons/pin.svg",
    "icons/plug.svg",
    "icons/plus.svg",
    "icons/procedure.svg",
    "icons/reload.svg",
    "icons/role.svg",
    "icons/schema.svg",
    "icons/schema-fill.svg",
    "icons/search.svg",
    "icons/sequence.svg",
    "icons/sidebar.svg",
    "icons/sidebar-right.svg",
    "icons/star.svg",
    "icons/star-fill.svg",
    "icons/stop.svg",
    "icons/table.svg",
    "icons/tab.svg",
    "icons/trash.svg",
    "icons/trigger.svg",
    "icons/tunnel.svg",
    "icons/type.svg",
    "icons/view.svg",
    "icons/warning.svg",
    "logos/elasticsearch.svg",
    "logos/mongodb.svg",
    "logos/mysql.svg",
    "logos/opensearch.svg",
    "logos/postgres.svg",
    "logos/redis.svg",
    "logos/sql-server.svg",
    "art/welcome-dark.svg",
    "art/welcome-light.svg",
);

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(FILES.iter().find(|(p, _)| *p == path).map(|(_, b)| Cow::Borrowed(*b)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(FILES.iter().filter(|(p, _)| p.starts_with(path)).map(|(p, _)| SharedString::from(*p)).collect())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    /// Run: an SF-Symbols-style `arrow.right` (round caps, short head).
    Run,
    Stop,
    Pause,
    History,
    Plus,
    Reload,
    ChevronRight,
    ChevronDown,
    /// Previous match (find bar).
    ChevronUp,
    /// Previous page (a table's data view).
    ChevronLeft,
    Database,
    /// A schema (a folder): outlined while open, `SchemaFilled` while shut.
    Schema,
    SchemaFilled,
    Group,
    Table,
    View,
    Key,
    /// Search by shortcut (Settings › Keyboard).
    Keyboard,
    Index,
    Search,
    Filter,
    Close,
    Check,
    Copy,
    Export,
    /// Import data from a file.
    Import,
    Edit,
    Trash,
    Sidebar,
    /// The sidebar glyph mirrored: the explorer in the right dock.
    SidebarRight,
    PanelRight,
    /// The result panel zoomed over the editor, and back.
    Expand,
    Collapse,
    Lock,
    Warning,
    Info,
    More,
    /// A query tab (command palette).
    Tab,
    /// A command (command palette).
    Command,
    /// Generated SQL (`</>`).
    Code,
    /// Count rows.
    Hash,
    /// Connect / disconnect.
    Plug,
    /// The connection goes through an SSH tunnel (explorer).
    Tunnel,
    // Explorer object kinds.
    MatView,
    Function,
    Procedure,
    Sequence,
    Type,
    /// An enum's label.
    Label,
    Trigger,
    Extension,
    Role,
    Constraint,
    /// A foreign key.
    Link,
    Partition,
    /// The activity monitor.
    Activity,
    /// A favorite query, unfavorited.
    Star,
    /// A favorite query, favorited.
    StarFill,
    /// A pinned table (the explorer's "pinned" group, Pin table / Unpin table).
    Pin,
    /// An agent's tab, an agent's statement in the history (a four-point sparkle).
    Agent,
    // LAR (violet, LAR panels only).
    Live,
    /// A collection of documents (MongoDB's, an index's): `{ }`.
    Collection,
}

impl Icon {
    pub fn path(self) -> &'static str {
        match self {
            Icon::Run => "icons/arrow-right.svg",
            Icon::Stop => "icons/stop.svg",
            Icon::Pause => "icons/pause.svg",
            Icon::History => "icons/history.svg",
            Icon::Plus => "icons/plus.svg",
            Icon::Reload => "icons/reload.svg",
            Icon::ChevronRight => "icons/chevron-r.svg",
            Icon::ChevronDown => "icons/chevron-d.svg",
            Icon::ChevronUp => "icons/chevron-u.svg",
            Icon::ChevronLeft => "icons/chevron-l.svg",
            Icon::Database => "icons/database.svg",
            Icon::Schema => "icons/schema.svg",
            Icon::SchemaFilled => "icons/schema-fill.svg",
            Icon::Group => "icons/group.svg",
            Icon::Table => "icons/table.svg",
            Icon::View => "icons/view.svg",
            Icon::Key => "icons/key.svg",
            Icon::Keyboard => "icons/keyboard.svg",
            Icon::Index => "icons/index.svg",
            Icon::Search => "icons/search.svg",
            Icon::Filter => "icons/filter.svg",
            Icon::Close => "icons/close.svg",
            Icon::Check => "icons/check.svg",
            Icon::Copy => "icons/copy.svg",
            Icon::Export => "icons/export.svg",
            Icon::Import => "icons/import.svg",
            Icon::Edit => "icons/edit.svg",
            Icon::Trash => "icons/trash.svg",
            Icon::Sidebar => "icons/sidebar.svg",
            Icon::SidebarRight => "icons/sidebar-right.svg",
            Icon::PanelRight => "icons/panel-right.svg",
            Icon::Expand => "icons/expand.svg",
            Icon::Collapse => "icons/collapse.svg",
            Icon::Lock => "icons/lock.svg",
            Icon::Warning => "icons/warning.svg",
            Icon::Info => "icons/info.svg",
            Icon::More => "icons/more.svg",
            Icon::Tab => "icons/tab.svg",
            Icon::Command => "icons/command.svg",
            Icon::Code => "icons/code.svg",
            Icon::Hash => "icons/hash.svg",
            Icon::Plug => "icons/plug.svg",
            Icon::Tunnel => "icons/tunnel.svg",
            Icon::MatView => "icons/matview.svg",
            Icon::Function => "icons/function.svg",
            Icon::Procedure => "icons/procedure.svg",
            Icon::Sequence => "icons/sequence.svg",
            Icon::Type => "icons/type.svg",
            Icon::Label => "icons/label.svg",
            Icon::Trigger => "icons/trigger.svg",
            Icon::Extension => "icons/extension.svg",
            Icon::Role => "icons/role.svg",
            Icon::Constraint => "icons/constraint.svg",
            Icon::Link => "icons/link.svg",
            Icon::Partition => "icons/partition.svg",
            Icon::Activity => "icons/activity.svg",
            Icon::Star => "icons/star.svg",
            Icon::StarFill => "icons/star-fill.svg",
            Icon::Pin => "icons/pin.svg",
            Icon::Agent => "icons/agent.svg",
            Icon::Live => "icons/live.svg",
            Icon::Collection => "icons/braces.svg",
        }
    }
}

/// An icon element (size and color are up to the caller; default 16 pt).
pub fn icon(i: Icon) -> Svg {
    svg().path(i.path()).flex_none().size(crate::theme::px(16.))
}

/// The database logos are shown (Settings › Appearance › Database logos).
static LOGOS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn logos_on() -> bool {
    LOGOS.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_logos(on: bool) {
    LOGOS.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// An engine's logo, in its own colors (an image: drawn at 64 px, crisp at any size up to
/// 32 pt); the size is up to the caller.
pub fn logo(e: dataman_core::Engine) -> gpui::Img {
    let path: &'static str = match e {
        dataman_core::Engine::Postgres => "logos/postgres.svg",
        dataman_core::Engine::MySql => "logos/mysql.svg",
        dataman_core::Engine::SqlServer => "logos/sql-server.svg",
        dataman_core::Engine::MongoDb => "logos/mongodb.svg",
        dataman_core::Engine::Elasticsearch => "logos/elasticsearch.svg",
        dataman_core::Engine::OpenSearch => "logos/opensearch.svg",
        dataman_core::Engine::Redis => "logos/redis.svg",
    };
    gpui::img(path).flex_none()
}

/// The small logo the explorer and the connection picker put before a connection's name, when
/// logos are on (`None` otherwise).
pub fn small_logo(e: dataman_core::Engine, size: f32) -> Option<gpui::Img> {
    logos_on().then(|| logo(e).size(crate::theme::px(size)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_engine_has_its_logo() {
        for e in dataman_core::Engine::ALL {
            let path = format!("logos/{}.svg", e.slug());
            let bytes = Assets.load(&path).unwrap().unwrap_or_else(|| panic!("{path}"));
            assert!(std::str::from_utf8(&bytes).unwrap().starts_with("<svg"), "{path}");
        }
    }

    #[test]
    fn every_icon_is_embedded() {
        use Icon::*;
        let all = [
            Run, Stop, Pause, History, Plus, Reload, ChevronRight, ChevronDown, ChevronUp, ChevronLeft, Database, Schema,
            SchemaFilled, Group, Table, View, Key, Keyboard, Index, Search, Filter, Close, Check, Copy, Export, Import, Edit, Trash,
            Sidebar, SidebarRight, PanelRight, Expand, Collapse, Lock, Warning, Info, More, Tab, Command, Code, Hash, Plug,
            Tunnel, MatView, Function, Procedure, Sequence, Type, Label, Trigger, Extension, Role,
            Constraint, Link, Partition, Activity, Star, StarFill, Pin, Agent, Live, Collection,
        ];
        for i in all {
            let bytes = Assets.load(i.path()).unwrap().unwrap_or_else(|| panic!("{} missing", i.path()));
            assert!(std::str::from_utf8(&bytes).unwrap().starts_with("<svg"));
        }
        assert!(Assets.load("art/welcome-dark.svg").unwrap().is_some());
    }
}
