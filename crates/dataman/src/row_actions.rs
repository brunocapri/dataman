//! The explorer context menu's actions that reach past the tree (`tree_menu.rs` lists them):
//! query tabs on the row's connection and database, the connection sheet, the history, the
//! export dialog, generated SQL and the "Delete connection?" sheet.
//!
//! Statements that destroy something (DROP, TRUNCATE) only ever open in a new tab with a
//! warning comment: nothing here runs them. What runs at once is read-only (`SELECT`,
//! `count(*)`) or session-local (`SET search_path`).

use gpui::{
    AnyElement, App, ClipboardItem, Context, Entity, Focusable, FontWeight, KeyBinding, MouseButton, Window, actions, div,
    prelude::*,
};
use crate::theme::px;
use dataman_core::schema::RelKind;
use dataman_core::sqlgen;

use crate::app::{Bound, Region, Side, Workspace};
use crate::privileges::{PrivilegesEvent, PrivilegesPanel};
use crate::tab::QueryTab;
use crate::theme::ActiveTheme;
use crate::tree_menu::{Act, Gen, Obj, Target};
use crate::ui::{self, Btn};

actions!(
    row_actions,
    [
        /// The "Delete connection?" sheet: keep it.
        CancelDeleteConnection,
        /// The "Delete connection?" sheet: delete it.
        ConfirmDeleteConnection,
        /// The palette: duplicate the explorer's connection.
        DuplicateConnection,
        /// The palette: delete the explorer's connection (asks first).
        DeleteConnection,
        /// The palette: copy the explorer's connection as a URL (no password).
        CopyConnectionString,
    ]
);

const CONFIRM: &str = "ConfirmDeleteConnection";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONFIRM);
    cx.bind_keys([
        // ↩ and esc keep it (the safe answer); ⌘⌫ deletes.
        KeyBinding::new("escape", CancelDeleteConnection, c),
        KeyBinding::new("enter", CancelDeleteConnection, c),
        KeyBinding::new("cmd-backspace", ConfirmDeleteConnection, c),
    ]);
}

impl Workspace {
    /// A context menu action from the explorer.
    pub(crate) fn on_row_action(&mut self, act: Act, target: Target, alt: bool, window: &mut Window, cx: &mut Context<Self>) {
        match target {
            Target::Empty if act == Act::NewConnection => self.open_dialog(None, window, cx),
            Target::Conn { id, .. } => self.conn_action(act, id, window, cx),
            Target::Db { conn, db, .. } => match act {
                Act::ToggleReadOnly => self.toggle_read_only(&conn, &db, cx),
                Act::NewTab => {
                    self.open_sql_tab(&conn, Some(&db), "", false, window, cx);
                }
                Act::CreateSchema => {
                    self.open_sql_tab(&conn, Some(&db), &sqlgen::create_schema_template(&db), false, window, cx);
                }
                // MySQL: the database is the schema.
                Act::CreateTable => {
                    self.open_sql_tab(&conn, Some(&db), &sqlgen::create_table_template_mysql(&db), false, window, cx);
                }
                // On the connection's own database: a database cannot drop itself (the item is
                // disabled on that one).
                Act::DropDatabase => {
                    let sql = sqlgen::drop_database_template_in(self.conn_dialect(&conn, cx), &db);
                    self.open_sql_tab(&conn, None, &sql, false, window, cx);
                }
                _ => {}
            },
            Target::Schema { conn, db, schema, lar, .. } => match act {
                Act::ImportData => self.import_into(&conn, &db, &schema, None, window, cx),
                Act::NewTab => {
                    // SQL Server has no per-session schema path (a user's default schema).
                    if schema == "public" || self.conn_dialect(&conn, cx) != dataman_core::sql::Dialect::Postgres {
                        self.open_sql_tab(&conn, Some(&db), "", false, window, cx);
                    } else {
                        // The tab's session looks in the schema first.
                        let sql = format!("{}\n\n", sqlgen::search_path_for(&schema));
                        if let Some(tab) = self.open_sql_tab(&conn, Some(&db), &sql, false, window, cx) {
                            let stmt = sqlgen::search_path_for(&schema).trim_end_matches(';').to_string();
                            tab.update(cx, |t, cx| {
                                t.editor.update(cx, |e, cx| e.set_cursor(sql.len(), cx));
                                t.run(vec![stmt], cx);
                            });
                        }
                    }
                }
                Act::CreateTable => {
                    let sql = match self.conn_dialect(&conn, cx) {
                        dataman_core::sql::Dialect::TSql => sqlgen::create_table_template_tsql(&schema),
                        _ => sqlgen::create_table_template(&schema, lar),
                    };
                    self.open_sql_tab(&conn, Some(&db), &sql, false, window, cx);
                }
                Act::DropSchema => {
                    let sql = sqlgen::drop_schema_template_in(self.conn_dialect(&conn, cx), &schema);
                    self.open_sql_tab(&conn, Some(&db), &sql, false, window, cx);
                }
                Act::Privileges => self.open_privileges(&conn, &db, &schema, None, window, cx),
                _ => {}
            },
            Target::Rel { conn, db, schema, name, .. } if act == Act::Privileges => self.open_privileges(&conn, &db, &schema, Some(&name), window, cx),
            Target::Rel { conn, db, schema, name, kind, .. } => self.rel_action(act, &conn, &db, &schema, &name, kind, alt, window, cx),
            Target::Object { conn, db, schema, name, what } => self.object_action(act, &conn, &db, &schema, &name, what, alt, window, cx),
            _ => {}
        }
        cx.notify();
    }

    /// The grant matrix of a table (or a schema) in the side panel; its GRANT… / REVOKE… open
    /// templates in a new tab.
    pub(crate) fn open_privileges(&mut self, conn: &str, db: &str, schema: &str, table: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.store.read(cx).connection(conn) else {
            self.notice("Connect first", cx);
            return;
        };
        let (db, schema, table) = (db.to_string(), schema.to_string(), table.map(str::to_string));
        let view = cx.new(|cx| PrivilegesPanel::new(c, db, schema, table, cx));
        cx.subscribe_in(&view, window, |w, view, ev: &PrivilegesEvent, window, cx| match ev {
            PrivilegesEvent::Template(sql) => {
                let (conn, db) = {
                    let v = view.read(cx);
                    (v.conn_id.clone(), v.db.clone())
                };
                w.open_sql_tab(&conn, Some(&db), sql, false, window, cx);
            }
            PrivilegesEvent::Close => {
                if matches!(w.side, Some(Side::Feature("privileges", ..))) {
                    w.side = None;
                }
                cx.notify();
            }
        })
        .detach();
        self.side_w = self.side_w.max(420.);
        self.side = Some(Side::Feature("privileges", view.into(), Bound::Explorer));
        cx.notify();
    }

    /// The other objects' actions: definitions and calls in a new tab (or ⌥ at the cursor),
    /// values and versions in the status bar, and templates for anything that changes them.
    #[allow(clippy::too_many_arguments)]
    fn object_action(&mut self, act: Act, conn: &str, db: &str, schema: &str, name: &str, what: Obj, alt: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.store.read(cx).connection(conn) else {
            self.notice("Connect first", cx);
            return;
        };
        let db = if db.is_empty() { c.database().to_string() } else { db.to_string() };
        let d = c.dialect();
        let q = if schema.is_empty() { d.quote_ident(name) } else { d.qualified(Some(schema), name) };
        let template = |w: &mut Self, sql: String, window: &mut Window, cx: &mut Context<Self>| {
            w.open_sql_tab(conn, Some(&db), &sql, false, window, cx);
        };
        let function = |oid: u32| c.schema.cached_functions(&db, schema).and_then(|v| v.iter().find(|f| f.oid == oid).cloned());
        match (act, &what) {
            (Act::ShowDefinition, Obj::Trigger { definition: Some(d), .. }) => {
                let sql = if d.trim_end().ends_with(';') { d.clone() } else { format!("{d};") };
                self.deliver_sql(conn, &db, sql, alt, window, cx)
            }
            (Act::ShowDefinition, _) => {
                let task = match &what {
                    // MySQL's routines have no oid: by name, from the cached list.
                    Obj::Function { oid, .. } => match function(*oid) {
                        Some(f) => c.routine_definition(&db, schema, &f),
                        None => c.function_definition(&db, *oid),
                    },
                    Obj::Type => c.type_definition(&db, schema, name),
                    Obj::Role => c.role_definition(name),
                    _ => return,
                };
                let conn = conn.to_string();
                cx.spawn_in(window, async move |this, cx| {
                    let r = task.await;
                    let _ = this.update_in(cx, |w, window, cx| match r {
                        Ok(Some(sql)) => w.deliver_sql(&conn, &db, sql, alt, window, cx),
                        Ok(None) => w.notice(format!("The server does not report the definition of {q}"), cx),
                        Err(e) => w.notice(format!("Could not read {q}: {e:#}"), cx),
                    });
                })
                .detach();
            }
            (Act::GenerateCall, Obj::Function { oid, .. }) => match function(*oid) {
                Some(f) => self.deliver_sql(conn, &db, sqlgen::function_call_in(d, schema, &f), alt, window, cx),
                None => self.notice(format!("Reload {} to read {q} again", d.quote_ident(schema)), cx),
            },
            (Act::SequenceValue, _) => {
                let task = c.sequence_value(&db, schema, name);
                cx.spawn_in(window, async move |this, cx| {
                    let r = task.await;
                    let _ = this.update(cx, |w, cx| match r {
                        Ok(Some((v, true))) => w.notice(format!("{q} · last value {v}"), cx),
                        Ok(Some((v, false))) => w.notice(format!("{q} · unused, starts at {v}"), cx),
                        Ok(None) => w.notice(format!("The server does not report the value of {q}"), cx),
                        Err(e) => w.notice(format!("Could not read {q}: {e:#}"), cx),
                    });
                })
                .detach();
            }
            (Act::ResetSequence, _) => {
                let owned = c.schema.cached_sequences(&db, schema).and_then(|v| v.iter().find(|s| s.name == name).and_then(|s| s.owned_by.clone()));
                let sql = sqlgen::sequence_reset_template_in(d, schema, name, owned.as_ref().map(|(t, c)| (t.as_str(), c.as_str())));
                template(self, sql, window, cx);
            }
            (Act::EnableTrigger | Act::DisableTrigger, Obj::Trigger { table, .. }) => {
                template(self, sqlgen::trigger_toggle_template_in(d, schema, table, name, act == Act::EnableTrigger), window, cx)
            }
            (Act::AlterRole, _) => template(self, sqlgen::alter_role_template(name), window, cx),
            (Act::ExtensionInfo, _) => {
                let x = c.schema.cached_extensions(&db).and_then(|v| v.into_iter().find(|x| x.name == name));
                match x {
                    Some(x) => {
                        let mut s = format!("{} {}", x.name, x.version);
                        if let Some(sc) = &x.schema {
                            s.push_str(&format!(" · in {sc}"));
                        }
                        match x.update() {
                            Some(v) => s.push_str(&format!(" · {v} available")),
                            None if x.default_version.is_some() => s.push_str(" · up to date"),
                            None => {}
                        }
                        self.notice(s, cx);
                    }
                    None => self.notice(format!("Reload {} to read {q} again", sqlgen::quote_ident(&db)), cx),
                }
            }
            (Act::UpdateExtension, Obj::Extension { update }) => template(self, sqlgen::extension_update_template(name, update.as_deref()), window, cx),
            _ => {}
        }
    }

    fn conn_action(&mut self, act: Act, id: String, window: &mut Window, cx: &mut Context<Self>) {
        match act {
            Act::Disconnect => self.request_disconnect(id, false, window, cx),
            Act::Reconnect => self.request_disconnect(id, true, window, cx),
            Act::NewTab => {
                self.open_sql_tab(&id, None, "", false, window, cx);
            }
            Act::History => {
                // This connection's history, whatever the active tab is on.
                self.history_pin = Some(id.clone());
                self.history.update(cx, |h, cx| h.show(Some(id), cx));
                self.side = Some(Side::History);
                self.focus_region(Region::Side, window, cx);
            }
            Act::Activity => self.open_activity(&id, window, cx),
            Act::EditConnection => self.open_dialog(Some(id), window, cx),
            Act::DuplicateConnection => self.duplicate_connection(&id, window, cx),
            Act::DeleteConnection => self.request_delete_connection(id, false, window, cx),
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rel_action(&mut self, act: Act, conn: &str, db: &str, schema: &str, name: &str, kind: RelKind, alt: bool, window: &mut Window, cx: &mut Context<Self>) {
        let run = |w: &mut Self, sql: String, window: &mut Window, cx: &mut Context<Self>| {
            w.open_sql_tab(conn, Some(db), &sql, true, window, cx);
        };
        let template = |w: &mut Self, sql: String, window: &mut Window, cx: &mut Context<Self>| {
            w.open_sql_tab(conn, Some(db), &sql, false, window, cx);
        };
        let d = self.conn_dialect(conn, cx);
        match act {
            // The data view: pages, filters, server-side sort.
            Act::SelectTop => self.open_relation(conn, db, schema, name, window, cx),
            // The grid still fetches as it scrolls (the core pauses after the first rows).
            Act::SelectAll => run(self, sqlgen::select_star_in(d, schema, name, None), window, cx),
            Act::CountRows => run(self, sqlgen::count_rows_in(d, schema, name), window, cx),
            Act::OpenWithoutRunning => template(self, sqlgen::select_star_in(d, schema, name, Some(1000)), window, cx),
            Act::Generate(g) => self.generate_sql(conn, db, schema, name, g, alt, window, cx),
            Act::ShowDefinition => self.show_view_definition(conn, db, schema, name, alt, window, cx),
            Act::Export => self.export_table(conn, db, schema, name, window, cx),
            Act::ImportData => self.import_into(conn, db, schema, Some(name), window, cx),
            Act::Truncate => template(self, sqlgen::truncate_template_in(d, schema, name), window, cx),
            Act::DropTable => template(self, sqlgen::drop_table_template_in(d, schema, name), window, cx),
            Act::DropView if kind == RelKind::MaterializedView => template(self, sqlgen::drop_view_template(schema, name, true), window, cx),
            Act::DropView => template(self, sqlgen::drop_view_template_in(d, schema, name), window, cx),
            _ => {}
        }
    }

    /// A database's Read Only (its menu): flips it, saves it with the connection and applies it
    /// at once to the live connection and to every tab on it, without reconnecting (their
    /// sessions follow before their next statement; `Connection::set_read_only_for`).
    pub(crate) fn toggle_read_only(&mut self, conn: &str, db: &str, cx: &mut Context<Self>) {
        let Some(mut p) = self.store.read(cx).profile(conn).cloned() else { return };
        let live = self.store.read(cx).connection(conn);
        let on = !live.as_ref().map_or_else(|| p.read_only_for(db), |c| c.read_only_for(db));
        p.set_read_only_for(db, on);
        if let Err(e) = self.store.read(cx).core.config.upsert(p.clone()) {
            self.notice(format!("Could not save the connection: {e:#}"), cx);
            return;
        }
        let mut conns: Vec<std::sync::Arc<dataman_core::Connection>> = live.into_iter().collect();
        for t in &self.tabs {
            if let Some(c) = t.read(cx).connection()
                && c.profile.id == conn
                && !conns.iter().any(|x| std::sync::Arc::ptr_eq(x, c))
            {
                conns.push(c.clone());
            }
        }
        for c in &conns {
            c.set_read_only_for(db, on);
        }
        self.store.update(cx, |s, cx| {
            if let Some(x) = s.profiles.iter_mut().find(|x| x.id == conn) {
                *x = p;
            }
            cx.notify();
        });
        cx.notify();
    }

    /// The SQL dialect of a saved connection (its profile's engine).
    pub(crate) fn conn_dialect(&self, conn: &str, cx: &App) -> dataman_core::sql::Dialect {
        self.store.read(cx).profile(conn).map(|p| p.engine.dialect()).unwrap_or_default()
    }

    /// "Import data…": the file panel, then the import sheet into the table (or a new table in
    /// the schema).
    fn import_into(&mut self, conn: &str, db: &str, schema: &str, table: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.store.read(cx).connection(conn) else {
            self.notice("Connect first", cx);
            return;
        };
        let target = crate::data_import::DataTarget {
            conn_id: conn.to_string(),
            conn: c,
            db: db.to_string(),
            schema: schema.to_string(),
            table: table.map(str::to_string),
        };
        self.import_data(Some(target), None, window, cx);
    }

    /// A new tab on `conn` and database `db` (the connection's own when `None`) with `sql`,
    /// run at once when `run`. `None` (with a notice) when that connection is not connected.
    pub(crate) fn open_sql_tab(
        &mut self,
        conn: &str,
        db: Option<&str>,
        sql: &str,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<QueryTab>> {
        let Some(c) = self.store.read(cx).connection(conn) else {
            self.notice("Connect first", cx);
            return None;
        };
        self.current_conn = Some(conn.to_string());
        let tab = self.new_tab(window, cx);
        let db = db.map(str::to_string);
        let sql = sql.to_string();
        tab.update(cx, |t, cx| {
            t.set_connection(Some(c), cx);
            // A row of another database runs on a session opened on that database.
            if let Some(db) = db {
                t.set_database(db, cx);
            }
            t.editor.update(cx, |e, cx| {
                e.set_text(&sql, cx);
                // A template reads from the top (its warning, its names to edit).
                if !run {
                    e.set_cursor(0, cx);
                }
            });
            if run {
                t.run(vec![sql.trim_end().trim_end_matches(';').to_string()], cx);
            }
        });
        Some(tab)
    }

    /// Generated SQL: a new tab on the table's database, or (⌥) at the active editor's cursor.
    fn deliver_sql(&mut self, conn: &str, db: &str, sql: String, alt: bool, window: &mut Window, cx: &mut Context<Self>) {
        if alt
            && let Some(tab) = self.active_tab().cloned()
        {
            let editor = tab.read(cx).editor.clone();
            editor.update(cx, |e, cx| e.insert_at_cursor(&format!("{sql}\n"), cx));
            window.focus(&editor.focus_handle(cx), cx);
            return;
        }
        self.open_sql_tab(conn, Some(db), &sql, false, window, cx);
    }

    /// Generate SQL ▸: from the cached columns when the explorer has them (else they are read
    /// now); CREATE TABLE always reads the catalog when chosen.
    #[allow(clippy::too_many_arguments)]
    fn generate_sql(&mut self, conn: &str, db: &str, schema: &str, name: &str, g: Gen, alt: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.store.read(cx).connection(conn) else {
            self.notice("Connect first", cx);
            return;
        };
        let (conn, db, schema, name) = (conn.to_string(), db.to_string(), schema.to_string(), name.to_string());
        let d = c.dialect();
        let build = move |cols: &[dataman_core::schema::Column], schema: &str, name: &str| match g {
            Gen::Select => sqlgen::select_columns_in(d, schema, name, cols),
            Gen::Insert => sqlgen::insert_template_in(d, schema, name, cols),
            Gen::Update => sqlgen::update_template_in(d, schema, name, cols),
            Gen::Delete => sqlgen::delete_template_in(d, schema, name, cols),
            Gen::CreateTable => String::new(),
        };
        if g != Gen::CreateTable
            && let Some(cols) = c.schema.cached_full_columns(&db, &schema, &name)
        {
            let sql = build(&cols, &schema, &name);
            self.deliver_sql(&conn, &db, sql, alt, window, cx);
            return;
        }
        let task: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>>>> = if g == Gen::CreateTable {
            Box::pin(c.table_ddl(&db, &schema, &name))
        } else {
            // The columns load on the core; the statement is built when they arrive.
            let cols = c.columns(&db, &schema, &name);
            let (s, n) = (schema.clone(), name.clone());
            Box::pin(async move { cols.await.map(|cols| build(&cols, &s, &n)) })
        };
        cx.spawn_in(window, async move |this, cx| {
            let r = task.await;
            let _ = this.update_in(cx, |w, window, cx| match r {
                Ok(sql) => w.deliver_sql(&conn, &db, sql, alt, window, cx),
                Err(e) => w.notice(format!("Could not read {schema}.{name} from the catalog: {e:#}"), cx),
            });
        })
        .detach();
    }

    /// A view's definition (`pg_get_viewdef`) in a new tab; LAR does not report it.
    #[allow(clippy::too_many_arguments)]
    fn show_view_definition(&mut self, conn: &str, db: &str, schema: &str, name: &str, alt: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.store.read(cx).connection(conn) else {
            self.notice("Connect first", cx);
            return;
        };
        let task = c.view_definition(db, schema, name);
        let (conn, db, label) = (conn.to_string(), db.to_string(), c.dialect().qualified(Some(schema), name));
        cx.spawn_in(window, async move |this, cx| {
            let r = task.await;
            let _ = this.update_in(cx, |w, window, cx| match r {
                Ok(Some(sql)) => w.deliver_sql(&conn, &db, sql, alt, window, cx),
                Ok(None) => w.notice(format!("The server does not report the definition of {label}"), cx),
                Err(e) => w.notice(format!("Could not read {label}: {e:#}"), cx),
            });
        })
        .detach();
    }

    /// The connection sheet prefilled with a copy ("copy of …", a new id). The saved password
    /// is read now and goes under the new id only if the copy is saved.
    fn duplicate_connection(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.store.read(cx).profile(id).cloned() else { return };
        let core = self.store.read(cx).core.clone();
        let password = if p.save_password { core.secrets.get(&p.id).ok().flatten() } else { None };
        let mut copy = p.clone();
        copy.id = dataman_core::ConnectionProfile::new("").id;
        copy.name = format!("copy of {}", p.name);
        self.open_dialog_with(Some(copy), true, password, window, cx);
    }

    /// The palette's connection commands act on the explorer's connection.
    pub(crate) fn copy_connection_string(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.current_conn.as_ref().and_then(|id| self.store.read(cx).profile(id).cloned()) else {
            self.notice("Select a connection in the explorer first", cx);
            return;
        };
        let url = p.url();
        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
        self.notice(format!("Copied {url}"), cx);
    }

    pub(crate) fn ask_delete_connection(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open(cx) {
            return;
        }
        self.modal_return = window.focused(cx);
        self.confirm_delete = Some(id);
        window.focus(&self.confirm_delete_focus, cx);
        cx.notify();
    }

    fn resolve_delete(&mut self, delete: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.confirm_delete.take() else { return };
        self.modal_return = None;
        if delete {
            self.delete_connection(&id, cx);
        }
        let f = self.tree.focus_handle(cx);
        window.focus(&f, cx);
        cx.notify();
    }

    /// Removes a connection: from `connections.toml`, its password from the keychain and its
    /// history. Its tabs stay, without a connection (their sessions end: running statements
    /// get a CancelRequest).
    pub(crate) fn delete_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        let core = self.store.read(cx).core.clone();
        let name = self.store.read(cx).profile(id).map(|p| p.name.clone()).unwrap_or_else(|| id.to_string());
        if let Err(e) = core.remove_connection(id) {
            self.notice(format!("Could not delete {name}: {e:#}"), cx);
            return;
        }
        let _ = core.history.clear(id);
        self.disconnect(id.to_string(), cx);
        self.store.update(cx, |s, cx| s.reload(cx));
        if self.current_conn.as_deref() == Some(id) {
            self.current_conn = self.store.read(cx).profiles.first().map(|p| p.id.clone());
        }
        if self.history_pin.as_deref() == Some(id) {
            self.history_pin = None;
        }
        self.notice(format!("Deleted the connection {name}"), cx);
    }

    /// "Delete connection "name"?": esc / ↩ keep it, ⌘⌫ deletes.
    pub(crate) fn render_confirm_delete(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = self.confirm_delete.as_ref()?;
        let t = cx.theme();
        let store = self.store.read(cx);
        let name = store.profile(id).map(|p| p.name.clone()).unwrap_or_else(|| id.clone());
        let summary = store.profile(id).map(|p| p.summary()).unwrap_or_default();
        let tabs = self.tabs.iter().filter(|tab| tab.read(cx).connection().is_some_and(|c| c.profile.id == *id)).count();
        let mut body = summary;
        if tabs > 0 {
            body.push_str(&format!(" · {tabs} open tab{}", if tabs == 1 { "" } else { "s" }));
        }
        Some(
            div()
                .id("confirm-delete-conn")
                .absolute()
                .inset_0()
                .bg(t.scrim)
                .occlude()
                .key_context(format!("{CONFIRM} Modal").as_str())
                .track_focus(&self.confirm_delete_focus)
                .on_action(cx.listener(|w, _: &CancelDeleteConnection, window, cx| w.resolve_delete(false, window, cx)))
                .on_action(cx.listener(|w, _: &ConfirmDeleteConnection, window, cx| w.resolve_delete(true, window, cx)))
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(MouseButton::Left, cx.listener(|w, _, window, cx| w.resolve_delete(false, window, cx)))
                .child(
                    ui::modal(&t, "confirm-delete-card")
                        .w(px(440.))
                        .child(ui::modal_header(
                            &t,
                            None,
                            format!("Delete the connection \u{201c}{name}\u{201d}?"),
                            ui::modal_close(&t, "dc-close").on_click(cx.listener(|w, _, window, cx| w.resolve_delete(false, window, cx))),
                        ))
                        .child(div().px(px(20.)).py(px(16.)).line_height(px(18.)).text_color(t.tx2).child(body))
                        .child(
                            ui::modal_footer(&t)
                                .child(div().flex_1())
                                .child(
                                    ui::button(&t, "dc-cancel", Btn::Secondary)
                                        .h(px(28.))
                                        .child("Keep it")
                                        .child(ui::key_hint("esc"))
                                        .on_click(cx.listener(|w, _, window, cx| w.resolve_delete(false, window, cx))),
                                )
                                .child(
                                    ui::button(&t, "dc-delete", Btn::Danger)
                                        .h(px(28.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("Delete")
                                        .child(ui::key_hint("⌘⌫"))
                                        .on_click(cx.listener(|w, _, window, cx| w.resolve_delete(true, window, cx))),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The palette's row commands, on the explorer's connection.
    pub(crate) fn row_actions_on_current(&mut self, act: Act, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.current_conn.clone() else {
            self.notice("Select a connection in the explorer first", cx);
            return;
        };
        self.conn_action(act, id, window, cx);
    }
}
