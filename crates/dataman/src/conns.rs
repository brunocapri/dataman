use std::collections::HashMap;
use std::sync::Arc;

use gpui::{Context, EventEmitter};
use dataman_core::{ConnectionProfile, Connection, Core, SshError};

#[derive(Clone)]
pub enum ConnState {
    Connecting,
    Connected(Arc<Connection>),
    Failed(String),
}

pub enum ConnEvent {
    Connected(String),
    Failed { id: String, needs_password: bool, ssh: Option<SshError> },
}

pub struct ConnStore {
    pub core: Arc<Core>,
    pub profiles: Vec<ConnectionProfile>,
    pub states: HashMap<String, ConnState>,
}

impl EventEmitter<ConnEvent> for ConnStore {}

impl ConnStore {
    pub fn new(core: Arc<Core>) -> Self {
        let profiles = core.config.connections();
        ConnStore { core, profiles, states: HashMap::new() }
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let profiles = self.core.config.connections();
        for gone in self.profiles.iter().filter(|p| !profiles.iter().any(|n| n.id == p.id)) {
            let _ = self.core.pins.forget(&gone.id);
        }
        self.profiles = profiles;
        cx.notify();
    }

    pub fn reorder(&mut self, from: usize, slot: usize, cx: &mut Context<Self>) {
        if dataman_core::order::move_to(&mut self.profiles, from, slot).is_none() {
            return;
        }
        let ids: Vec<String> = self.profiles.iter().map(|p| p.id.clone()).collect();
        if let Err(e) = self.core.config.reorder(&ids) {
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
            return;
        }
        self.states.insert(id.to_string(), ConnState::Connecting);
        cx.notify();
        let task = self.core.connect(profile, password);
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |s, cx| {
                match r {
                    Ok(c) => {
                        let db = c.database().to_string();
                        // (Dropping the task handle does not stop the work.)
                        let _ = c.preload(&db);
                        s.states.insert(id.clone(), ConnState::Connected(c));
                        cx.emit(ConnEvent::Connected(id));
                    }
                    Err(e) => {
                        let ssh = crate::conn_dialog::ssh_error(&e);
                        let msg = match &ssh {
                            Some(s) => s.to_string(),
                            None => format!("{e:#}"),
                        };
                        let needs_password = ssh.is_none() && (msg.contains("password") || msg.contains("28P01"));
                        let ssh = ssh.filter(|s| matches!(s, SshError::UnknownHostKey(_) | SshError::SecretNeeded { .. }));
                        s.states.insert(id.clone(), ConnState::Failed(msg));
                        cx.emit(ConnEvent::Failed { id, needs_password, ssh });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn disconnect(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(ConnState::Connected(c)) = self.states.remove(id) {
            c.close();
        }
        cx.notify();
    }

    pub fn forget(&mut self, id: &str, cx: &mut Context<Self>) {
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
                let alive = if changed { this.update_in(cx, |w, window, cx| w.connections_reloaded(window, cx)).is_ok() } else { this.upgrade().is_some() };
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
        let old = self.store.read(cx).profiles.clone();
        let new = self.store.read(cx).core.config.connections();
        if old == new {
            return;
        }
        let was_empty = old.is_empty();
        self.store.update(cx, |s, cx| s.reload(cx));
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
                (Edited::Removed, Some(ConnState::Connected(_))) if !held => self.disconnect(p.id.clone(), cx),
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

#[cfg(test)]
mod tests {
    use super::*;

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
