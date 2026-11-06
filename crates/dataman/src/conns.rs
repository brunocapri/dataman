use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dataman_core::{ConnFolder, Connection, ConnectionProfile, Core, SshError};
use gpui::{Context, EventEmitter};

#[derive(Clone)]
pub enum ConnState {
    Connecting,
    Connected(Arc<Connection>),
    Failed(String),
}

pub enum ConnEvent {
    Connected(String),
    /// Connected on its own (Settings › Connections), not picked: nothing moves to it.
    Arrived(String),
    /// `cloud`: a cloud sign-in that the user can fix from the dialog (sign in again, install).
    Failed {
        id: String,
        needs_password: bool,
        ssh: Option<SshError>,
        cloud: Option<dataman_core::cloudauth::SignInError>,
    },
}

pub struct ConnStore {
    pub core: Arc<Core>,
    pub profiles: Vec<ConnectionProfile>,
    /// The explorer's folders (none: the explorer as it always was).
    pub folders: Vec<ConnFolder>,
    pub states: HashMap<String, ConnState>,
    /// Connecting on their own: a failure only shows where the connection is listed, never as a
    /// dialog (a password, an SSH host key), until it is picked.
    quiet: HashSet<String>,
    attempts: Attempts,
}

/// Which connect is the live one for each id: a disconnect or forget made while one runs ends
/// it, and what it brings back is closed instead of kept.
#[derive(Default)]
struct Attempts {
    next: u64,
    live: HashMap<String, u64>,
}

impl Attempts {
    fn start(&mut self, id: &str) -> u64 {
        self.next += 1;
        self.live.insert(id.to_string(), self.next);
        self.next
    }

    /// True once, for the live attempt.
    fn finish(&mut self, id: &str, n: u64) -> bool {
        let live = self.live.get(id) == Some(&n);
        if live {
            self.live.remove(id);
        }
        live
    }

    fn end(&mut self, id: &str) {
        self.live.remove(id);
    }
}

impl EventEmitter<ConnEvent> for ConnStore {}

impl ConnStore {
    pub fn new(core: Arc<Core>) -> Self {
        let profiles = core.config.connections();
        let folders = core.config.folders();
        ConnStore { core, profiles, folders, states: HashMap::new(), quiet: HashSet::new(), attempts: Attempts::default() }
    }

    pub fn edit_folders(&mut self, f: impl FnOnce(&mut Vec<ConnFolder>), cx: &mut Context<Self>) -> anyhow::Result<()> {
        let r = self.core.config.edit_folders(f);
        self.folders = self.core.config.folders();
        cx.notify();
        r
    }

    /// The folders as another process (or an undo) left them.
    pub fn reload_folders(&mut self, cx: &mut Context<Self>) {
        let folders = self.core.config.folders();
        if folders != self.folders {
            self.folders = folders;
            cx.notify();
        }
    }

    pub fn folder_of(&self, conn: &str) -> Option<&ConnFolder> {
        dataman_core::config::folder_of(&self.folders, conn)
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let profiles = self.core.config.connections();
        for gone in self.profiles.iter().filter(|p| !profiles.iter().any(|n| n.id == p.id)) {
            let _ = self.core.pins.forget(&gone.id);
        }
        self.profiles = profiles;
        self.folders = self.core.config.folders();
        cx.notify();
    }

    pub fn set_order(&mut self, ids: &[String], cx: &mut Context<Self>) {
        self.profiles.sort_by_key(|p| ids.iter().position(|i| *i == p.id).unwrap_or(usize::MAX));
        if let Err(e) = self.core.config.reorder(ids) {
            log::warn!("saving the connection order: {e:#}");
        }
        cx.notify();
    }

    pub fn profile(&self, id: &str) -> Option<&ConnectionProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn connection(&self, id: &str) -> Option<Arc<Connection>> {
        match self.states.get(id) {
            Some(ConnState::Connected(c)) => Some(c.clone()),
            _ => None,
        }
    }

    pub fn connect(&mut self, id: &str, password: Option<String>, cx: &mut Context<Self>) {
        let Some(profile) = self.profile(id).cloned() else { return };
        if matches!(self.states.get(id), Some(ConnState::Connecting | ConnState::Connected(_))) {
            // Picked while it connects on its own: it arrives as picked.
            self.quiet.remove(id);
            return;
        }
        self.states.insert(id.to_string(), ConnState::Connecting);
        let n = self.attempts.start(id);
        cx.notify();
        // A cloud token has no password to ask for: its error says what to run instead.
        let token = profile.auth.is_token();
        let task = self.core.connect(profile, password);
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |s, cx| {
                // Disconnected, forgotten or edited meanwhile: that one wins.
                if !s.attempts.finish(&id, n) {
                    if let Ok(c) = r {
                        c.close();
                    }
                    return;
                }
                match r {
                    Ok(c) => {
                        let db = c.database().to_string();
                        // (Dropping the task handle does not stop the work.)
                        drop(c.preload(&db));
                        s.states.insert(id.clone(), ConnState::Connected(c));
                        if s.quiet.remove(&id) {
                            cx.emit(ConnEvent::Arrived(id));
                        } else {
                            cx.emit(ConnEvent::Connected(id));
                        }
                    }
                    Err(e) => {
                        let ssh = crate::conn_dialog::ssh_error(&e);
                        let msg = match &ssh {
                            Some(s) => s.to_string(),
                            None => format!("{e:#}"),
                        };
                        let needs_password = ssh.is_none() && !token && dataman_core::needs_password(&e);
                        let ssh = ssh.filter(|s| matches!(s, SshError::UnknownHostKey(_) | SshError::SecretNeeded { .. }));
                        let cloud = dataman_core::cloudauth::sign_in_error(&e).filter(|s| s.action.is_some()).cloned();
                        s.states.insert(id.clone(), ConnState::Failed(msg));
                        if !s.quiet.remove(&id) {
                            cx.emit(ConnEvent::Failed { id, needs_password, ssh, cloud });
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Settings › Connections: every saved connection not connected yet connects, quietly. A
    /// DuckDB file is left alone: open, it is locked for every other program.
    pub fn connect_all(&mut self, cx: &mut Context<Self>) {
        let all: Vec<String> = self.profiles.iter().map(|p| p.id.clone()).collect();
        self.connect_quietly(&all, cx);
    }

    pub fn connect_quietly(&mut self, ids: &[String], cx: &mut Context<Self>) {
        if !auto_connect(&self.core) {
            return;
        }
        let ids: Vec<String> = self
            .profiles
            .iter()
            .filter(|p| ids.contains(&p.id) && !p.engine.caps().exclusive_lock && !self.states.contains_key(&p.id))
            .map(|p| p.id.clone())
            .collect();
        for id in ids {
            self.quiet.insert(id.clone());
            self.connect(&id, None, cx);
        }
    }

    pub fn disconnect(&mut self, id: &str, cx: &mut Context<Self>) {
        self.attempts.end(id);
        self.quiet.remove(id);
        if let Some(ConnState::Connected(c)) = self.states.remove(id) {
            c.close();
        }
        cx.notify();
    }

    pub fn forget(&mut self, id: &str, cx: &mut Context<Self>) {
        self.attempts.end(id);
        self.quiet.remove(id);
        self.states.remove(id);
        cx.notify();
    }

    pub fn close_all(&self) {
        for s in self.states.values() {
            if let ConnState::Connected(c) = s {
                c.close();
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Edited {
    Same,
    ReadOnlyDatabases,
    Settings,
    Removed,
}

pub(crate) fn edited(old: &ConnectionProfile, new: Option<&ConnectionProfile>) -> Edited {
    match new {
        None => Edited::Removed,
        Some(n) if n.same_but_agents(old) => Edited::Same,
        Some(n) if (ConnectionProfile { read_only_databases: n.read_only_databases.clone(), ..old.clone() }).same_but_agents(n) => Edited::ReadOnlyDatabases,
        Some(_) => Edited::Settings,
    }
}

impl crate::app::Workspace {
    pub(crate) fn watch_connections(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let core = self.store.read(cx).core.clone();
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                let c = core.clone();
                let changed = cx.background_executor().spawn(async move { c.config.refresh() }).await;
                let alive = if changed {
                    this.update_in(cx, |w, window, cx| {
                        w.connections_reloaded(window, cx);
                        w.check_outside_edits(window, cx);
                    })
                    .is_ok()
                } else {
                    this.upgrade().is_some()
                };
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    pub(crate) fn connections_on_disk(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        self.store.read(cx).core.config.refresh();
        self.connections_reloaded(window, cx);
    }

    /// A live connection whose settings changed reconnects, never ending a transaction: a tab
    /// with one open keeps the old connection until it is done.
    pub(crate) fn connections_reloaded(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        self.store.update(cx, |s, cx| s.reload_folders(cx));
        let old = self.store.read(cx).profiles.clone();
        let new = self.store.read(cx).core.config.connections();
        if old == new {
            return;
        }
        let was_empty = old.is_empty();
        self.store.update(cx, |s, cx| s.reload(cx));
        // Saved elsewhere (another Dataman, an agent, the project): connected as the others were.
        let added: Vec<String> = new.iter().filter(|n| !old.iter().any(|o| o.id == n.id)).map(|n| n.id.clone()).collect();
        if !added.is_empty() {
            self.store.update(cx, |s, cx| s.connect_quietly(&added, cx));
        }
        for p in &old {
            let change = edited(p, new.iter().find(|n| n.id == p.id));
            let state = self.store.read(cx).states.get(&p.id).cloned();
            let held = self.tabs.iter().any(|t| t.read(cx).tx_open() && t.read(cx).connection().is_some_and(|c| c.profile.id == p.id));
            match (change, state) {
                (Edited::Same, _) => {}
                (Edited::ReadOnlyDatabases, Some(ConnState::Connected(c))) => {
                    if let Some(n) = new.iter().find(|n| n.id == p.id) {
                        for db in p.read_only_databases.keys().chain(n.read_only_databases.keys()) {
                            c.set_read_only_for(db, n.read_only_for(db));
                        }
                    }
                }
                (Edited::Settings, Some(ConnState::Connected(_))) => {
                    let id = p.id.clone();
                    self.store.update(cx, |s, cx| {
                        if held {
                            s.forget(&id, cx);
                        } else {
                            s.disconnect(&id, cx);
                        }
                        s.connect(&id, None, cx);
                    });
                }
                // Still connecting with the old settings: it starts over with the new ones.
                (Edited::Settings, Some(ConnState::Connecting)) => {
                    let id = p.id.clone();
                    let quiet = self.store.read(cx).quiet.contains(&id);
                    self.store.update(cx, |s, cx| {
                        s.forget(&id, cx);
                        if quiet {
                            s.quiet.insert(id.clone());
                        }
                        s.connect(&id, None, cx);
                    });
                }
                (Edited::Removed, Some(ConnState::Connected(_))) if !held => self.disconnect(p.id.clone(), cx),
                (Edited::Removed, Some(ConnState::Connecting)) => self.store.update(cx, |s, cx| s.forget(&p.id, cx)),
                (Edited::Settings | Edited::Removed, Some(ConnState::Failed(_))) => self.store.update(cx, |s, cx| s.forget(&p.id, cx)),
                _ => {}
            }
        }
        if self.current_conn.as_ref().is_none_or(|id| !new.iter().any(|p| p.id == *id)) {
            self.current_conn = new.first().map(|p| p.id.clone());
        }
        if was_empty && !new.is_empty() {
            self.focus_editor(window, cx);
        }
        if let Some(v) = self.settings_view.clone() {
            v.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }
}

/// Settings › Connections. The check stages set up their connections themselves.
pub fn auto_connect(core: &Core) -> bool {
    !(cfg!(feature = "checks") && std::env::var_os("DATAMAN_SHOT_STAGE").is_some()) && core.settings.get().auto_connect
}

/// `profiles` as the explorer groups them: each folder with its own (in `profiles`' order), then
/// those in none. Empty groups left out.
pub fn grouped<'a>(folders: &'a [ConnFolder], profiles: &[&'a ConnectionProfile]) -> Vec<(Option<&'a ConnFolder>, Vec<&'a ConnectionProfile>)> {
    let loose = profiles.iter().copied().filter(|p| dataman_core::config::folder_of(folders, &p.id).is_none()).collect();
    folders
        .iter()
        .map(|d| (Some(d), profiles.iter().copied().filter(|p| d.holds(&p.id)).collect()))
        .chain([(None, loose)])
        .filter(|(_, v): &(_, Vec<_>)| !v.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_as_the_explorer() {
        let [a, b, c, d] = ["a", "b", "c", "d"].map(ConnectionProfile::new);
        let mut work = ConnFolder::new("Work", &[]);
        work.connections = vec![c.id.clone(), a.id.clone()];
        let mut empty = ConnFolder::new("Empty", &[]);
        empty.connections = vec!["gone".into()];
        let folders = [empty, work];
        let g = grouped(&folders, &[&a, &b, &c, &d]);
        let names: Vec<(Option<&str>, Vec<&str>)> = g.iter().map(|(f, v)| (f.map(|f| f.name.as_str()), v.iter().map(|p| p.name.as_str()).collect())).collect();
        assert_eq!(names, [(Some("Work"), vec!["a", "c"]), (None, vec!["b", "d"])]);
        assert_eq!(grouped(&[], &[&b]).len(), 1);
        assert!(grouped(&folders, &[]).is_empty());
    }

    #[test]
    fn a_later_connect_or_a_disconnect_wins() {
        let mut a = Attempts::default();
        let first = a.start("x");
        let second = a.start("x");
        assert!(!a.finish("x", first), "superseded");
        assert!(a.finish("x", second));
        assert!(!a.finish("x", second), "only once");
        let n = a.start("x");
        a.end("x");
        assert!(!a.finish("x", n), "disconnected meanwhile");
        let (y, z) = (a.start("y"), a.start("z"));
        a.end("y");
        assert!(!a.finish("y", y));
        assert!(a.finish("z", z), "per connection");
    }

    #[test]
    fn what_an_edit_is() {
        let p = ConnectionProfile::new("a");
        assert_eq!(edited(&p, Some(&p)), Edited::Same);
        assert_eq!(edited(&p, None), Edited::Removed);
        // What agents may do: no reconnect (a write held for an "Always allow" goes on).
        let mut q = p.clone();
        (q.mcp, q.mcp_yolo) = (Some(dataman_core::McpAccess::Full), true);
        q.mcp_allow(&["TRUNCATE t"]);
        assert_eq!(edited(&p, Some(&q)), Edited::Same);
        q.set_read_only_for("shop", true);
        assert_eq!(edited(&p, Some(&q)), Edited::ReadOnlyDatabases);
        q.name = "b".into();
        assert_eq!(edited(&p, Some(&q)), Edited::Settings);
    }
}
