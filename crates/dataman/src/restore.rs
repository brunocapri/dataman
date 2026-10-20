use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, Context, Global, Window};
use dataman_core::groups::{Pane, SplitState};
use dataman_core::session::{Session, SessionStore, VERSION};

use crate::app::Workspace;
use crate::theme::metrics;

pub const SAVE_AFTER: Duration = Duration::from_millis(1500);

pub struct Sessions(pub Option<Arc<SessionStore>>);

impl Global for Sessions {}

pub fn store(cx: &App) -> Option<Arc<SessionStore>> {
    cx.try_global::<Sessions>().and_then(|s| s.0.clone())
}

pub fn dir_for(dirs: &dataman_core::Dirs, throwaway: bool) -> Option<std::path::PathBuf> {
    match std::env::var_os("DATAMAN_SESSION_DIR").filter(|v| !v.is_empty()) {
        Some(d) => Some(d.into()),
        None if throwaway => None,
        None => Some(dirs.session_dir()),
    }
}

impl Workspace {
    pub(crate) fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(store) = store(cx) else { return false };
        let Some(s) = store.load() else { return false };
        self.layout = s.layout.clone().normalized();
        if s.tabs.is_empty() {
            return false;
        }
        let mut files = Vec::new();
        for st in &s.tabs {
            let tab = self.new_tab(window, cx);
            self.fill_tab(&tab, st, cx);
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
        if let Some(sp) = s.split.as_ref().and_then(|sp| sp.normalized(self.tabs.len(), active)) {
            let id = |i: usize| self.tabs[i].entity_id();
            self.split = Some(crate::split::Split {
                right: sp.right.iter().map(|&i| id(i)).collect(),
                focus: if sp.focus_right { Pane::Right } else { Pane::Left },
                other: id(sp.other),
                ratio: sp.ratio,
            });
        }
        self.activate(active, window, cx);
        self.set_nav(s.nav, cx);
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

    pub(crate) fn fill_tab(&mut self, tab: &gpui::Entity<crate::tab::QueryTab>, st: &dataman_core::session::TabState, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let known: HashSet<&str> = store.profiles.iter().map(|p| p.id.as_str()).collect();
        let want = st.conn.clone().filter(|id| known.contains(id.as_str())).map(|id| (id, st.database.clone()));
        let conn = want.as_ref().and_then(|(id, _)| store.connection(id));
        let favorite = st.favorite.as_ref().and_then(|id| store.core.favorites.list().into_iter().find(|f| &f.id == id)).map(|f| (f.id, f.name.into()));
        tab.update(cx, |t, cx| {
            if want.is_some() {
                t.set_connection(None, cx);
            }
            t.restore(st, cx);
            t.favorite = favorite;
            t.want = want;
            if let Some(c) = conn {
                t.adopt(c, cx);
            }
        });
    }

    fn session_snapshot(&mut self, cx: &mut Context<Self>) -> (Session, Vec<(String, Vec<dataman_core::session::SaveItem>)>) {
        let mut files = Vec::new();
        let mut tabs = Vec::new();
        let mut active = 0;
        let mut saved_at = std::collections::HashMap::new();
        for (i, tab) in self.tabs.clone().into_iter().enumerate() {
            if tab.read(cx).monitor.is_some() {
                continue;
            }
            if i == self.active {
                active = tabs.len();
            }
            saved_at.insert(tab.entity_id(), tabs.len());
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
        let split = self.split.as_ref().and_then(|sp| {
            let at = |id: &gpui::EntityId| saved_at.get(id).copied();
            SplitState { right: sp.right.iter().filter_map(at).collect(), focus_right: sp.focus == Pane::Right, other: at(&sp.other).unwrap_or(usize::MAX), ratio: sp.ratio }
                .normalized(tabs.len(), active)
        });
        let session = Session {
            version: VERSION,
            tabs,
            active,
            split,
            explorer_open: self.explorer_open,
            explorer_w: self.explorer_w,
            // The Agents area is not where a relaunch opens (older versions could not read it either).
            nav: if self.view == dataman_core::layout::NavView::Agents { dataman_core::layout::NavView::Explorer } else { self.view },
            side_w: self.side_w,
            editor_frac: self.editor_frac,
            layout: self.layout.clone(),
        };
        (session, files)
    }

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
            t.favorite.as_ref().map(|(id, _)| id.clone()).hash(&mut h);
        }
        (self.active, self.explorer_open, self.explorer_w.to_bits(), self.side_w.to_bits(), self.view).hash(&mut h);
        self.editor_frac.map(f32::to_bits).hash(&mut h);
        self.layout.hash(&mut h);
        self.split.as_ref().map(|s| (&s.right, s.focus, s.other, s.ratio.to_bits())).hash(&mut h);
        h.finish()
    }

    pub(crate) fn schedule_session_save(&mut self, cx: &mut Context<Self>) {
        if store(cx).is_none() || self.session_off {
            return;
        }
        self.session_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            let _ = this.update(cx, |w, cx| w.save_session(false, cx));
        }));
    }

    pub(crate) fn save_session(&mut self, now: bool, cx: &mut Context<Self>) {
        let Some(store) = store(cx).filter(|_| !self.session_off) else { return };
        self.session_task = None;
        let ticket = store.ticket();
        let (session, files) = self.session_snapshot(cx);
        let write = move || {
            let files: Vec<(String, Vec<u8>)> = files.into_iter().map(|(name, items)| (name, dataman_core::session::encode_results(&items))).collect();
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
