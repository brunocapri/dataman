//! File › Open Project: a folder's `.dataman/project.toml` kept in step with the local files
//! (see `dataman_core::project`). The menus, the palette and the welcome screen read whether
//! projects are on from [`Projects`]; the sync runs every second off the UI thread. A file that
//! loosens protection or moves a connection is held: the review sheet shows what it changes,
//! Not now is the default, and Apply anyway applies that very file and logs it in Audit.

use std::path::PathBuf;
use std::time::Duration;

use dataman_core::Core;
use dataman_core::agents_config::Change;
use dataman_core::project::Synced;
use dataman_mcp::audit::{ApprovalNote, Audit, Decision, Entry, Outcome};
use gpui::{AnyElement, App, Context, FontWeight, MouseButton, SharedString, Window, actions, div, prelude::*};

use crate::app::Workspace;
use crate::theme::{ActiveTheme, Theme, px};
use crate::ui::{self, Btn};

actions!(project_ui, [ReviewKeep, ReviewApply]);

const SHEET: &str = "ProjectReview";

pub fn bind_keys(cx: &mut App) {
    let c = Some(SHEET);
    cx.bind_keys([crate::keymap::kb("escape", ReviewKeep, c), crate::keymap::kb("enter", ReviewKeep, c), crate::keymap::kb("cmd-enter", ReviewApply, c)]);
}

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
                let alive = if synced.local() || synced.review { this.update_in(cx, |w, window, cx| w.project_synced(synced, window, cx)).is_ok() } else { this.upgrade().is_some() };
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
        if s.review {
            self.show_project_review(window, cx);
        }
        cx.notify();
    }

    /// The held file's review, over whatever is open (Settings included).
    pub(crate) fn show_project_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let core = self.store.read(cx).core.clone();
        let Some(r) = core.project.review() else { return };
        if self.project_review.is_none() {
            self.project_review_return = window.focused(cx);
        }
        self.project_review = Some(r);
        window.focus(&self.project_review_focus, cx);
        cx.notify();
    }

    /// Apply: the file as shown, logged in Audit as the user's own act. Not now: held, without
    /// asking again until the file changes; Settings › Project shows it.
    pub(crate) fn close_project_review(&mut self, apply: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(r) = self.project_review.take() else { return };
        let core = self.store.read(cx).core.clone();
        match self.project_review_return.take() {
            Some(f) => window.focus(&f, cx),
            None => self.focus_editor(window, cx),
        }
        if !apply {
            core.project.decline();
            self.notice("The project's file waits: Settings › Project reviews it", cx);
            if let Some(v) = self.settings_view.clone() {
                v.update(cx, |v, cx| v.reload(cx));
            }
            return cx.notify();
        }
        let started = std::time::Instant::now();
        let result = core.project.accept(core.stores(), &r);
        let reasons: Vec<String> = r.conns.iter().flat_map(|c| c.changes.iter().filter(|x| x.loosens).map(move |x| format!("{} › {}", c.name, x.what))).chain(r.settings.iter().filter(|x| x.loosens).map(|x| x.what.clone())).take(20).collect();
        let root = core.project.info(&core.settings.get()).map(|i| dataman_core::config::tilde(&i.root.to_string_lossy())).unwrap_or_default();
        let (outcome, error) = match &result {
            Ok(_) => (Outcome::Ok, None),
            Err(e) => (Outcome::Error, Some(format!("{e:#}"))),
        };
        Audit::new(core.dirs.mcp_audit_file()).write(&Entry {
            id: None,
            ts: chrono::Utc::now(),
            client: "Dataman".into(),
            transport: "app".into(),
            tool: "project_review".into(),
            connection: None,
            database: None,
            sql: None,
            query: Some(format!("Applied the project's file of {root}: {} loosening protection", r.loosened())),
            rows: None,
            ms: started.elapsed().as_secs_f64() * 1000.,
            outcome,
            error,
            approval: Some(ApprovalNote { decision: Decision::Approved, by: Some(dataman_mcp::audit::local_user()), reasons, rows: None }),
            draft: false,
            favorite: None,
            allowed_by: None,
            rewritten: None,
            seen: None,
            login: None,
        });
        match result {
            Ok(s) => {
                self.notice("Applied the project's file", cx);
                self.project_synced(s, window, cx);
            }
            Err(e) => {
                self.notice(format!("{e:#}"), cx);
                self.show_project_review(window, cx);
            }
        }
    }

    pub(crate) fn render_project_review(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let r = self.project_review.as_ref()?;
        let t = cx.theme();
        let name = projects(cx).open.unwrap_or_default();
        let mut body = div().flex().flex_col().gap(px(14.)).child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .line_height(px(18.))
                .child(div().text_color(t.tx2).child("Its .dataman/project.toml gives agents or this Mac more than they have now, or sends connections elsewhere. Nothing of it is applied, and the file is not synced, until you apply it."))
                .child(div().children(ui::here(&"project-review-loosens".into())).text_color(t.warn).child(format!("{} loosen protection", r.loosened()))),
        );
        let mut k = 0;
        let mut section = |title: String, tag: Option<&'static str>, changes: &[Change]| {
            let rows: Vec<AnyElement> = changes
                .iter()
                .map(|c| {
                    k += 1;
                    change_row(&t, "project-review-change", k - 1, c)
                })
                .collect();
            body = std::mem::replace(&mut body, div()).child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().flex().flex_row().gap(px(8.)).child(ui::caps(&t, title)).children(tag.map(|s| div().text_size(px(10.5)).text_color(t.tx3).child(s))))
                    .children(rows),
            );
        };
        for c in &r.conns {
            let tag = match (c.new, c.moved) {
                (true, _) => Some("new connection"),
                (_, true) => Some("moves elsewhere: its saved password stays behind"),
                _ => None,
            };
            section(c.name.to_uppercase(), tag, &c.changes);
        }
        if !r.settings.is_empty() {
            section("SETTINGS".into(), None, &r.settings);
        }
        Some(
            div()
                .id("project-review-sheet")
                .absolute()
                .inset_0()
                .bg(t.scrim)
                .occlude()
                .key_context(format!("{SHEET} Modal").as_str())
                .track_focus(&self.project_review_focus)
                .on_action(cx.listener(|w, _: &ReviewKeep, window, cx| w.close_project_review(false, window, cx)))
                .on_action(cx.listener(|w, _: &ReviewApply, window, cx| w.close_project_review(true, window, cx)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .flex()
                .items_center()
                .justify_center()
                .child(
                    ui::modal(&t, "project-review-card")
                        .w(px(680.))
                        .child(ui::modal_header(&t, None, format!("Review the project {name}"), ui::modal_close(&t, "project-review-close").on_click(cx.listener(|w, _, window, cx| w.close_project_review(false, window, cx)))))
                        .child(div().id("project-review-body").max_h(px(460.)).overflow_y_scroll().px(px(20.)).py(px(16.)).child(body))
                        .child(
                            ui::modal_footer(&t)
                                .text_size(px(11.))
                                .gap(px(16.))
                                .child(div().flex_1().min_w_0().text_color(t.tx3).child("Applied all at once, and logged in Audit."))
                                .child(
                                    div()
                                        .flex_none()
                                        .flex()
                                        .flex_row()
                                        .gap(px(8.))
                                        .child(
                                            ui::button(&t, "project-review-keep", Btn::Primary)
                                                .h(px(28.))
                                                .px(px(10.))
                                                .gap(px(8.))
                                                .child("Not now")
                                                .child(ui::key_hint("↩"))
                                                .on_click(cx.listener(|w, _, window, cx| w.close_project_review(false, window, cx))),
                                        )
                                        .child(
                                            ui::button(&t, "project-review-apply", Btn::Danger)
                                                .h(px(28.))
                                                .px(px(10.))
                                                .gap(px(8.))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child("Apply anyway")
                                                .child(ui::key_hint("⌘↩"))
                                                .on_click(cx.listener(|w, _, window, cx| w.close_project_review(true, window, cx))),
                                        ),
                                ),
                        ),
                )
                .into_any_element(),
        )
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
                if synced.review {
                    self.notice(format!("{name}: its file waits for your review"), cx);
                } else if created {
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

/// One line of a review: what, from, to, and whether it loosens. `prefix`: the checks' probe.
pub(crate) fn change_row(t: &Theme, prefix: &str, k: usize, c: &Change) -> AnyElement {
    div()
        .children(ui::here(&SharedString::from(format!("{prefix}-{k}")).into()))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.))
        .py(px(4.))
        .border_b_1()
        .border_color(t.line)
        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx).child(c.what.clone()))
        .child(div().flex_none().max_w(px(170.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx3).child(c.from.clone()))
        .child(div().flex_none().text_color(t.tx3).child("→"))
        .child(div().flex_none().max_w(px(170.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(if c.loosens { t.warn } else { t.tx2 }).child(c.to.clone()))
        .child(div().flex_none().w(px(64.)).text_size(px(10.5)).text_color(t.warn).child(if c.loosens { "loosens" } else { "" }))
        .into_any_element()
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
