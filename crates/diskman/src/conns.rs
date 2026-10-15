//! Saved connections and their live state, shared by the sidebar and the tabs.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{Context, EventEmitter};
use diskman_core::{ConnectionProfile, Connection, Core, SshError};

#[derive(Clone)]
pub enum ConnState {
    Connecting,
    Connected(Arc<Connection>),
    Failed(String),
}

pub enum ConnEvent {
    Connected(String),
    /// Connecting failed; `needs_password` when the server asked for one we do not have, `ssh`
    /// when the tunnel needs the user (an unknown host key, a missing password or passphrase).
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
        self.profiles = self.core.config.connections();
        cx.notify();
    }

    /// Moves connection `from` to `slot` (a drag in the explorer) and saves the order to the file.
    pub fn reorder(&mut self, from: usize, slot: usize, cx: &mut Context<Self>) {
        if diskman_core::order::move_to(&mut self.profiles, from, slot).is_none() {
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
                        // Autocomplete data loads in the background right away.
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

    /// Forgets the connection and ends its SSH tunnel (tabs and live queries may still hold
    /// the connection; nothing gets through the tunnel after this).
    pub fn disconnect(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(ConnState::Connected(c)) = self.states.remove(id) {
            c.close();
        }
        cx.notify();
    }

    /// Ends every SSH tunnel (quitting).
    pub fn close_all(&self) {
        for s in self.states.values() {
            if let ConnState::Connected(c) = s {
                c.close();
            }
        }
    }
}
