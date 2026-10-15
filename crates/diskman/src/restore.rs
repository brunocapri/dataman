//! The workspace kept between launches (`diskman_core::session`): saved a moment after each
//! change (and when a result ends), and on quit; restored when the window opens, the tabs at
//! once and their results as soon as they are read (off the UI thread). Restored tabs wait
//! for their own connection: it connects when they run something (or from the explorer).
//!
//! `--demo`, the checks and the benchmarks never touch the user's session: they keep none
//! unless `DISKMAN_SESSION_DIR` names a directory (the `session-check` stage sets its own).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, Context, Global, Window};
use diskman_core::session::{Session, SessionStore, VERSION};

use crate::app::{Side, Workspace};
use crate::theme::metrics;

/// How long the workspace stays unchanged before it is saved.
pub const SAVE_AFTER: Duration = Duration::from_millis(1500);

/// The session directory in use (`None`: sessions off).
pub struct Sessions(pub Option<Arc<SessionStore>>);

impl Global for Sessions {}

pub fn store(cx: &App) -> Option<Arc<SessionStore>> {
    cx.try_global::<Sessions>().and_then(|s| s.0.clone())
}

/// `DISKMAN_SESSION_DIR`, else the app's own (`~/.config/diskman/session`) — but none for
/// the benchmarks and `--demo` without the variable.
pub fn dir_for(dirs: &diskman_core::Dirs, throwaway: bool) -> Option<std::path::PathBuf> {
    match std::env::var_os("DISKMAN_SESSION_DIR").filter(|v| !v.is_empty()) {
        Some(d) => Some(d.into()),
        None if throwaway => None,
        None => Some(dirs.session_dir()),
    }
}

impl Workspace {
    /// The saved workspace back (at startup): false when there is none, and the window opens
    /// with one empty tab as before.
    pub(crate) fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(store) = store(cx) else { return false };
        let Some(s) = store.load() else { return false };
        if s.tabs.is_empty() {
            return false;
        }
        let known: HashSet<String> = self.store.read(cx).profiles.iter().map(|p| p.id.clone()).collect();
        let mut files = Vec::new();
        for st in &s.tabs {
            let tab = self.new_tab(window, cx);
            // A tab whose connection is gone keeps its SQL and takes the next one, as new tabs do.
            let want = st.conn.clone().filter(|id| known.contains(id)).map(|id| (id, st.database.clone()));
            let conn = want.as_ref().and_then(|(id, _)| self.store.read(cx).connection(id));
            tab.update(cx, |t, cx| {
                if want.is_some() {
                    t.set_connection(None, cx);
                }
                t.restore(st, cx);
                t.want = want;
                if let Some(c) = conn {
                    t.adopt(c, cx);
                }
            });
            if let Some(f) = &st.results {
                files.push((self.tabs.len() - 1, f.clone(), st.shown, st.closed.clone()));
            }
        }
        self.explorer_open = s.explorer_open;
        if s.explorer_w.is_finite() && s.explorer_w > 0. {
            self.explorer_w = s.explorer_w.clamp(metrics::EXPLORER_MIN, metrics::EXPLORER_MAX);
        }
        if s.side_w.is_finite() && s.side_w > 0. {
            self.side_w = s.side_w.clamp(metrics::SIDE_MIN, metrics::SIDE_MAX);
        }
        self.editor_frac = s.editor_frac.filter(|f| f.is_finite() && *f > 0. && *f < 1.);
        let active = s.active.min(self.tabs.len() - 1);
        self.activate(active, window, cx);
        if s.history_open {
            self.side = Some(Side::History);
            self.sync_bound_views(window, cx);
        }
        // The results, read off the UI thread.
        let tabs: Vec<_> = files.iter().map(|(i, ..)| self.tabs[*i].downgrade()).collect();
        let jobs: Vec<_> = files.into_iter().map(|(_, f, shown, closed)| (f, shown, closed)).collect();
        cx.spawn(async move |_, cx| {
            let read = cx
                .background_executor()
                .spawn(async move {
                    jobs.into_iter().map(|(f, shown, closed)| (store.load_results(&f), f, shown, closed)).collect::<Vec<_>>()
                })
                .await;
            for (tab, (items, file, shown, closed)) in tabs.into_iter().zip(read) {
                let (Some(tab), Some(items)) = (tab.upgrade(), items) else { continue };
                let _ = tab.update(cx, |t, cx| t.restore_results(items, shown, closed, Some(file), cx));
            }
        })
        .detach();
        true
    }

    /// The workspace as the session keeps it, and the results files to write (tab, file name,
    /// statements): only the tabs whose run changed since their file was written.
    fn session_snapshot(&mut self, cx: &mut Context<Self>) -> (Session, Vec<(String, Vec<diskman_core::session::SaveItem>)>) {
        let mut files = Vec::new();
        let mut tabs = Vec::new();
        let mut active = 0;
        for (i, tab) in self.tabs.clone().into_iter().enumerate() {
            // An activity monitor is a live view of a server, not a query: it does not come back.
            if tab.read(cx).monitor.is_some() {
                continue;
            }
            if i == self.active {
                active = tabs.len();
            }
            let mut st = tab.read(cx).session_state(cx);
            let items = tab.read(cx).save_items();
            if !items.is_empty() {
                let key = tab.read(cx).results_key();
                let name = match tab.read(cx).saved_results.clone() {
                    Some((k, name)) if k == key => name,
                    _ => {
                        let name = format!("r{}-{key:x}.bin", tab.read(cx).id);
                        files.push((name.clone(), items));
                        tab.update(cx, |t, _| t.saved_results = Some((key, name.clone())));
                        name
                    }
                };
                st.results = Some(name);
            }
            tabs.push(st);
        }
        let session = Session {
            version: VERSION,
            tabs,
            active,
            explorer_open: self.explorer_open,
            explorer_w: self.explorer_w,
            history_open: matches!(self.side, Some(Side::History)),
            side_w: self.side_w,
            editor_frac: self.editor_frac,
        };
        (session, files)
    }

    /// What a save would change, cheaply: the render compares it to the last one and saves a
    /// moment after it changes (a cursor blink or a redraw changes nothing).
    pub(crate) fn session_fingerprint(&self, cx: &App) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for t in &self.tabs {
            let t = t.read(cx);
            let e = t.editor.read(cx);
            (t.id, e.revision(), e.selection_ends(), t.results_key(), t.shown_key()).hash(&mut h);
            let (sx, sy) = e.scroll_points();
            (sx.to_bits(), sy.to_bits()).hash(&mut h);
            (t.connection().map(|c| c.profile.id.clone()), t.database().to_string(), t.want.clone()).hash(&mut h);
        }
        (self.active, self.explorer_open, self.explorer_w.to_bits(), self.side_w.to_bits(), matches!(self.side, Some(Side::History))).hash(&mut h);
        self.editor_frac.map(f32::to_bits).hash(&mut h);
        h.finish()
    }

    /// Saves a moment from now (each call pushes it back).
    pub(crate) fn schedule_session_save(&mut self, cx: &mut Context<Self>) {
        if store(cx).is_none() || self.session_off {
            return;
        }
        self.session_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            let _ = this.update(cx, |w, cx| w.save_session(false, cx));
        }));
    }

    /// Writes the session: off the UI thread, or (`now`: quitting, closing the window) before
    /// returning.
    pub(crate) fn save_session(&mut self, now: bool, cx: &mut Context<Self>) {
        let Some(store) = store(cx).filter(|_| !self.session_off) else { return };
        self.session_task = None;
        let ticket = store.ticket();
        let (session, files) = self.session_snapshot(cx);
        let write = move || {
            let files: Vec<(String, Vec<u8>)> = files.into_iter().map(|(name, items)| (name, diskman_core::session::encode_results(&items))).collect();
            if let Err(e) = store.save(ticket, &session, &files) {
                log::warn!("session: {e:#}");
            }
        };
        if now {
            write();
        } else {
            cx.background_executor().spawn(async move { write() }).detach();
        }
    }
}
