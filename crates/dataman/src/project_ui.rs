//! File › Open Project: a folder's `.dataman/project.toml` kept in step with the local files
//! (see `dataman_core::project`). The menus, the palette and the welcome screen read whether
//! projects are on from [`Projects`]; the sync runs every second off the UI thread.

use std::path::PathBuf;
use std::time::Duration;

use dataman_core::Core;
use dataman_core::project::Synced;
use gpui::{App, Context, Window};

use crate::app::Workspace;

/// Whether File › Open Project is on, and the open project's folder name.
#[derive(Clone, Default, PartialEq)]
pub struct Projects {
    pub enabled: bool,
    pub open: Option<String>,
}

impl gpui::Global for Projects {}

pub fn projects(cx: &App) -> Projects {
    cx.try_global::<Projects>().cloned().unwrap_or_default()
}

/// After the settings changed: the menus and the welcome screen follow.
pub fn publish(core: &Core, cx: &mut App) {
    let s = core.settings.get();
    let open = s.project.as_deref().filter(|_| s.project_enabled).map(|p| {
        let p = std::path::Path::new(p);
        p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
    });
    let now = Projects { enabled: s.project_enabled, open };
    if cx.try_global::<Projects>() != Some(&now) {
        cx.set_global(now);
        crate::refresh_menus(cx);
        cx.refresh_windows();
    }
}

impl Workspace {
    pub(crate) fn watch_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let core = self.store.read(cx).core.clone();
        publish(&core, cx);
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let c = core.clone();
                let r = cx.background_executor().spawn(async move { c.project.sync(c.stores()) }).await;
                let synced = match r {
                    Ok(s) => s,
                    Err(e) => {
                        log::debug!("project: {e:#}");
                        Synced::default()
                    }
                };
                let alive = if synced.local() { this.update_in(cx, |w, window, cx| w.project_synced(synced, window, cx)).is_ok() } else { this.upgrade().is_some() };
                if !alive {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        })
        .detach();
    }

    /// What the project changed on this side, shown.
    pub(crate) fn project_synced(&mut self, s: Synced, window: &mut Window, cx: &mut Context<Self>) {
        let core = self.store.read(cx).core.clone();
        if s.connections {
            self.connections_reloaded(window, cx);
        }
        if s.favorites {
            self.history.update(cx, |h, cx| h.reload_favorites(cx));
        }
        if s.settings {
            crate::editor::set_format_options(core.settings.get().format_options());
            crate::editor::set_format_on_save(core.settings.get().format_on_save);
            crate::refresh_menus(cx);
        }
        if let Some(v) = self.settings_view.clone() {
            v.update(cx, |v, cx| v.reload(cx));
        }
        publish(&core, cx);
        cx.notify();
    }

    pub(crate) fn open_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let core = self.store.read(cx).core.clone();
        if !core.settings.get().project_enabled {
            self.notice("Projects are off: Settings › Project turns them on", cx);
            return self.open_settings_at("project", window, cx);
        }
        let picked = pick_folder(cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Some(root)) = picked.await else { return };
            let _ = this.update_in(cx, |w, window, cx| w.open_project_at(&root, window, cx));
        })
        .detach();
    }

    /// The folder becomes the open project; a new `.dataman` shows Settings › Project, to pick
    /// what it shares.
    pub(crate) fn open_project_at(&mut self, root: &std::path::Path, window: &mut Window, cx: &mut Context<Self>) {
        let core = self.store.read(cx).core.clone();
        match core.project.open_project(root, core.stores()) {
            Ok((created, synced)) => {
                self.project_synced(synced, window, cx);
                let name = projects(cx).open.unwrap_or_default();
                if created {
                    self.notice(format!("{name}: created .dataman/project.toml"), cx);
                    self.open_settings_at("project", window, cx);
                } else {
                    self.notice(format!("Opened the project {name}"), cx);
                }
            }
            Err(e) => self.notice(format!("{e:#}"), cx),
        }
    }

    pub(crate) fn close_project(&mut self, cx: &mut Context<Self>) {
        let core = self.store.read(cx).core.clone();
        if let Err(e) = core.project.close(&core.settings) {
            log::warn!("closing the project: {e:#}");
        }
        publish(&core, cx);
        if let Some(v) = self.settings_view.clone() {
            v.update(cx, |v, cx| v.reload(cx));
        }
    }
}

/// A folder from the open panel.
pub fn pick_folder(cx: &App) -> tokio::sync::oneshot::Receiver<Option<PathBuf>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if let Some(p) = crate::sql_picker::take_test_paths() {
        let _ = tx.send(p.into_iter().next());
        return rx;
    }
    let paths = cx.prompt_for_paths(gpui::PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Open Project".into()) });
    cx.spawn(async move |cx| {
        let picked = crate::sql_picker::picked(paths.await, cx).and_then(|p| p.into_iter().next());
        let _ = tx.send(picked);
    })
    .detach();
    rx
}
