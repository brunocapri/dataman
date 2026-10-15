//! The update checker in the app: when to check (after the first frame when the last check is
//! a day old, then on window activation once a day, and from the menu), the background download
//! and verification, and the swap on "Update and restart". The work is `diskman_core::update`,
//! always off the UI thread; nothing polls. Without a feed or a public key the checker is off
//! and every entry (app menu, palette, Settings › About, the title-bar dot) is hidden.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gpui::{AnyWindowHandle, App, AppContext as _, Global, SharedString, WeakEntity};
use diskman_core::Core;
use diskman_core::update::{self, Feed, UpdateConfig};

use crate::app::Workspace;

const DAY: Duration = Duration::from_secs(24 * 3600);

/// Baked in at build time (scripts/bundle.sh passes them through).
const BUILD_FEED: Option<&str> = option_env!("DISKMAN_UPDATE_FEED");
const BUILD_KEY: Option<&str> = option_env!("DISKMAN_UPDATE_PUBKEY");

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    Checking,
    Downloading(String),
    /// Downloaded and verified. `blocked`: why this copy cannot replace itself (not in a
    /// bundle, translocated, not writable), said when the update is asked for.
    Ready { feed: Feed, dmg: PathBuf, blocked: Option<String> },
    Installing(String),
}

pub struct Updates {
    cfg: Option<Arc<UpdateConfig>>,
    pub state: State,
    last_check: Option<SystemTime>,
    /// Its mtime is the last check; its text, the feed of an update found then.
    stamp: PathBuf,
    auto: bool,
    /// The window whose status bar shows the outcome of a manual check.
    ws: Option<(WeakEntity<Workspace>, AnyWindowHandle)>,
}

impl Global for Updates {}

fn updates_mut(cx: &mut App) -> Option<&mut Updates> {
    if cx.has_global::<Updates>() { Some(cx.global_mut::<Updates>()) } else { None }
}

/// Reads the configuration (no I/O beyond the settings already loaded).
pub fn init(core: &Core, cx: &mut App) {
    let s = core.settings.get();
    let feed = s.update_feed.clone().or_else(|| BUILD_FEED.map(str::to_string));
    let cfg = UpdateConfig::new(feed.as_deref(), BUILD_KEY, crate::VERSION).map(Arc::new);
    cx.set_global(Updates {
        cfg,
        state: State::Idle,
        last_check: None,
        stamp: core.dirs.config.join(".update-check"),
        auto: s.update_check,
        ws: None,
    });
}

pub fn enabled(cx: &App) -> bool {
    cx.try_global::<Updates>().is_some_and(|u| u.cfg.is_some())
}

pub fn state(cx: &App) -> State {
    cx.try_global::<Updates>().map(|u| u.state.clone()).unwrap_or(State::Idle)
}

/// The update downloaded and verified.
pub fn ready(cx: &App) -> Option<Feed> {
    match &cx.try_global::<Updates>()?.state {
        State::Ready { feed, .. } => Some(feed.clone()),
        _ => None,
    }
}

/// The app menu's "Update to x.y.z and Restart".
pub fn menu_label(cx: &App) -> Option<SharedString> {
    ready(cx).map(|f| format!("Update to {} and Restart", f.version).into())
}

pub fn set_auto(on: bool, cx: &mut App) {
    if let Some(u) = updates_mut(cx) {
        u.auto = on;
    }
}

fn set_state(state: State, cx: &mut App) {
    if let Some(u) = updates_mut(cx)
        && u.state != state
    {
        u.state = state;
        crate::refresh_menus(cx);
        cx.refresh_windows();
    }
}

fn notice(msg: String, cx: &mut App) {
    if let Some((ws, _)) = cx.try_global::<Updates>().and_then(|u| u.ws.clone()) {
        let _ = ws.update(cx, |w, cx| w.notice(msg, cx));
    }
}

/// After the first frame: the last check's time and any update it found, read off the UI
/// thread; checks when a day has passed, or resumes the download of the update found.
pub fn start(ws: WeakEntity<Workspace>, window: AnyWindowHandle, cx: &mut App) {
    let Some(u) = updates_mut(cx) else { return };
    u.ws = Some((ws, window));
    let Some(cfg) = u.cfg.clone() else { return };
    let stamp = u.stamp.clone();
    let task = cx.background_spawn(async move {
        let at = std::fs::metadata(&stamp).and_then(|m| m.modified()).ok();
        let found = std::fs::read(&stamp)
            .ok()
            .and_then(|b| serde_json::from_slice::<Feed>(&b).ok())
            .filter(|f| update::is_newer(&f.version, &cfg.current));
        (at, found)
    });
    cx.spawn(async move |cx| {
        let (at, found) = task.await;
        log::info!(
            "update: last check {}, pending {}",
            at.and_then(|t| t.elapsed().ok()).map(|d| format!("{} s ago", d.as_secs())).unwrap_or_else(|| "never".into()),
            found.as_ref().map(|f| f.version.as_str()).unwrap_or("none")
        );
        cx.update(|cx| {
            let Some(u) = updates_mut(cx) else { return };
            u.last_check = at;
            let due = u.auto && at.is_none_or(|t| t.elapsed().unwrap_or(DAY) >= DAY);
            match found {
                _ if due => check(false, cx),
                Some(feed) => fetch(feed, cx),
                None => {}
            }
        });
    })
    .detach();
}

/// On window activation: checks if the last check is a day old (no timers in between).
pub fn on_activate(cx: &mut App) {
    let Some(u) = cx.try_global::<Updates>() else { return };
    if u.cfg.is_some() && u.auto && u.state == State::Idle && u.last_check.is_some_and(|t| t.elapsed().unwrap_or(DAY) >= DAY) {
        check(false, cx);
    }
}

/// Reads the feed now. `manual` (the menu): the outcome goes to the status bar.
pub fn check(manual: bool, cx: &mut App) {
    let Some(u) = updates_mut(cx) else { return };
    let Some(cfg) = u.cfg.clone() else { return };
    match u.state.clone() {
        State::Ready { feed, .. } => {
            if manual {
                notice(format!("{} ready", feed.version), cx);
            }
            return;
        }
        State::Checking | State::Downloading(_) | State::Installing(_) => return,
        State::Idle => {}
    }
    u.last_check = Some(SystemTime::now());
    let stamp = u.stamp.clone();
    set_state(State::Checking, cx);
    let task = cx.background_spawn(async move {
        let r = update::check(&cfg);
        // The time of this check, and what it found (resumed on the next launch).
        let body = match &r {
            Ok(Some(feed)) => serde_json::to_vec(feed).unwrap_or_default(),
            _ => Vec::new(),
        };
        let _ = std::fs::write(&stamp, body);
        r
    });
    cx.spawn(async move |cx| {
        let r = task.await;
        cx.update(|cx| match r {
            Ok(Some(feed)) => fetch(feed, cx),
            Ok(None) => {
                set_state(State::Idle, cx);
                if manual {
                    notice(format!("{} is the latest", crate::VERSION), cx);
                }
            }
            Err(e) => {
                log::warn!("update check: {e:#}");
                set_state(State::Idle, cx);
                if manual {
                    notice(format!("Update check failed: {e}"), cx);
                }
            }
        });
    })
    .detach();
}

/// Downloads and verifies the feed's DMG in the background.
fn fetch(feed: Feed, cx: &mut App) {
    let Some(cfg) = cx.try_global::<Updates>().and_then(|u| u.cfg.clone()) else { return };
    log::info!("update: {} offered", feed.version);
    set_state(State::Downloading(feed.version.clone()), cx);
    let dir = std::env::temp_dir().join("diskman-updates");
    let task = cx.background_spawn(async move {
        let dmg = update::download(&cfg, &feed, &dir)?;
        let blocked = match std::env::current_exe().ok().and_then(|e| update::bundle_of(&e)) {
            None => Some("not running from Diskman.app".to_string()),
            Some(app) => update::check_writable(&app).err().map(|e| e.to_string()),
        };
        anyhow::Ok(State::Ready { feed, dmg, blocked })
    });
    cx.spawn(async move |cx| {
        let r = task.await;
        cx.update(|cx| match r {
            Ok(ready) => {
                if let State::Ready { feed, blocked, .. } = &ready {
                    log::info!("update: {} downloaded and verified{}", feed.version, blocked.as_ref().map(|b| format!(" ({b})")).unwrap_or_default());
                }
                set_state(ready, cx);
                // The end-to-end test applies it at once.
                if std::env::var_os("DISKMAN_UPDATE_AUTORESTART").is_some()
                    && let Some((ws, window)) = cx.try_global::<Updates>().and_then(|u| u.ws.clone())
                {
                    let _ = window.update(cx, |_, window, cx| {
                        let _ = ws.update(cx, |w, cx| w.install_update(window, cx));
                    });
                }
            }
            Err(e) => {
                log::warn!("update rejected: {e:#}");
                set_state(State::Idle, cx);
            }
        });
    })
    .detach();
}

/// Screenshots: an update ready, as if a feed had offered it.
pub fn demo_ready(cx: &mut App) {
    if let Some(u) = updates_mut(cx) {
        let zero_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        u.cfg = UpdateConfig::new(Some("http://127.0.0.1/feed.json"), Some(zero_key), crate::VERSION).map(Arc::new);
    }
    let feed = Feed {
        version: "0.2.0".into(),
        notes: "Faster grid scrolling; EXPLAIN for LAR.".into(),
        url: String::new(),
        size: 0,
        ed25519: String::new(),
        minimum_macos: None,
        published: None,
    };
    set_state(State::Ready { feed, dmg: PathBuf::new(), blocked: None }, cx);
}

/// Why "Update and restart" cannot run here, if it cannot.
pub fn blocked(cx: &App) -> Option<String> {
    match &cx.try_global::<Updates>()?.state {
        State::Ready { blocked, .. } => blocked.clone(),
        State::Installing(_) => Some("installing".into()),
        _ => Some("no update ready".into()),
    }
}

/// Swaps the app for the downloaded one, off the UI thread; resolves to the app's path (to
/// relaunch). On failure the update stays offered, blocked with the reason.
pub fn install(cx: &mut App) -> Option<gpui::Task<anyhow::Result<PathBuf>>> {
    let State::Ready { feed, dmg, blocked: None } = state(cx) else { return None };
    let app = std::env::current_exe().ok().and_then(|e| update::bundle_of(&e))?;
    log::info!("update: installing {} over {}", feed.version, app.display());
    set_state(State::Installing(feed.version.clone()), cx);
    let (f, d) = (feed.clone(), dmg.clone());
    let task = cx.background_spawn(async move {
        update::install(&d, &app, &f.version)?;
        let _ = std::fs::remove_file(&d);
        anyhow::Ok(app)
    });
    Some(cx.spawn(async move |cx| {
        let r = task.await;
        if let Err(e) = &r {
            log::warn!("update install: {e:#}");
            let blocked = Some(e.to_string());
            cx.update(|cx| set_state(State::Ready { feed, dmg, blocked }, cx));
        }
        r
    }))
}
