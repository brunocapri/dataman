//! The settings the UI draws from statics: saved through one global and applied in one place.

use std::sync::Arc;

use dataman_core::Core;
use dataman_core::settings::Settings;
use gpui::{App, Global};

/// Where the UI's settings are saved.
pub struct UiPrefs(pub Arc<Core>);

impl Global for UiPrefs {}

/// `f` saved. An Err is said in the window and comes back false: the caller then applies
/// nothing, so what is shown stays what the file holds.
pub fn save(f: impl FnOnce(&mut Settings), cx: &mut App) -> bool {
    let Some(core) = cx.try_global::<UiPrefs>().map(|p| p.0.clone()) else { return true };
    match core.settings.update(f) {
        Ok(()) => true,
        Err(e) => {
            failed(&e, cx);
            false
        }
    }
}

/// A settings write that failed, said in the workspace's notice (and the log).
pub fn failed(e: &anyhow::Error, cx: &mut App) {
    let msg = format!("Could not save the settings: {e:#}");
    log::warn!("{msg}");
    cx.defer(move |cx| {
        for w in cx.windows() {
            if let Some(w) = w.downcast::<crate::app::Workspace>() {
                let _ = w.update(cx, |w, _, cx| w.notice_err(msg.clone(), cx));
                return;
            }
        }
    });
}

/// The statics as `s` has them: at launch, after each change, and when another process (a
/// project's sync) changed the file.
pub fn apply(s: &Settings, cx: &mut App) {
    crate::editor::set_code_size(s.editor_font_size);
    crate::editor::set_format_options(s.format_options());
    crate::editor::set_format_on_save(s.format_on_save);
    dataman_core::result::set_prefetch(s.prefetch_rows);
    crate::motion::set_reduce_motion_pref(s.reduce_motion, cx);
    crate::assets::set_logos(s.db_logos);
}
