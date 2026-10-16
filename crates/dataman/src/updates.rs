//! The update checker in the app: when to check (a few seconds after launch, then on window
//! activation once a day, and from the menu), the notice it raises (the title bar's pill and
//! its sheet), and Install & Relaunch: the download with its progress, the verification and the
//! swap. The work is `dataman_core::update`, always off the UI thread; nothing polls the feed.
//!
//! A check asked for (the menu, the palette, Settings › About) always answers, from "Checking…"
//! to its outcome: in Settings › About when Settings is open, else in the update sheet (its
//! check card, which becomes the update itself when one is found), else in an alert (another
//! sheet is up, or no window is left). One asked for while an automatic check runs joins it;
//! nothing meant for the automatic checks ever skips it.
//!
//! The feed is the latest GitHub release's `feed.json`, signed with the release key below;
//! `DATAMAN_UPDATE_FEED` / `DATAMAN_UPDATE_PUBKEY` at build time and `update_feed` in
//! settings.toml (an empty one turns the checker off) override them.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use gpui::{AnyWindowHandle, App, AppContext as _, Global, SharedString, WeakEntity};
use dataman_core::Core;
use dataman_core::update::{self, Feed, UpdateConfig};

use crate::app::Workspace;

const DAY: Duration = Duration::from_secs(24 * 3600);
/// The launch check waits this long after the first frame.
const LAUNCH_DELAY: Duration = Duration::from_secs(4);

/// Where releases are published, and the key that signs them (its private half never leaves
/// the release machine: scripts/keygen.sh).
pub const DEFAULT_FEED: &str = "https://github.com/brunocapri/dataman/releases/latest/download/feed.json";
pub const DEFAULT_PUBKEY: &str = "FiTN/mz2ZVaeKkSihEQnJmMppTub1f1VgDU3KpLXbeY=";

/// Overrides baked in at build time (scripts/bundle.sh passes them through).
const BUILD_FEED: Option<&str> = option_env!("DATAMAN_UPDATE_FEED");
const BUILD_KEY: Option<&str> = option_env!("DATAMAN_UPDATE_PUBKEY");

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Downloading { got: u64, total: u64 },
    Verifying,
    Installing,
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    Checking,
    /// A newer version, not downloaded yet. `blocked`: why this copy cannot replace itself
    /// (not in a bundle, translocated, not writable).
    Available { feed: Feed, blocked: Option<String> },
    /// Install & Relaunch under way.
    Working { feed: Feed, step: Step },
    /// Downloaded and verified: swapped in as the app quits.
    Ready { feed: Feed, dmg: PathBuf },
    /// The install failed; Retry starts it again.
    Failed { feed: Feed, error: String },
}

/// The last check that finished: when, and why it failed (`None`: the feed was read).
#[derive(Clone, Debug, PartialEq)]
pub struct Checked {
    pub at: SystemTime,
    pub error: Option<String>,
}

impl State {
    /// The version on offer, whatever is being done with it.
    pub fn feed(&self) -> Option<&Feed> {
        match self {
            State::Available { feed, .. } | State::Working { feed, .. } | State::Ready { feed, .. } | State::Failed { feed, .. } => Some(feed),
            State::Idle | State::Checking => None,
        }
    }
}

pub struct Updates {
    cfg: Option<Arc<UpdateConfig>>,
    pub state: State,
    last_check: Option<SystemTime>,
    auto: bool,
    /// The window whose notice shows the outcome of a check.
    ws: Option<(WeakEntity<Workspace>, AnyWindowHandle)>,
    /// "Later": the version whose notice stays hidden until a newer one or the next launch.
    dismissed: Option<String>,
    /// The check under way was asked for, or one was asked for while it ran: its outcome is said.
    manual: bool,
    /// The last check that finished, asked for or not (Settings › About says when).
    checked: Option<Checked>,
    /// The update check stage: the bundle replaced instead of this one, and no relaunch.
    pub(crate) test_target: Option<PathBuf>,
    /// The update check stage: what the last install swapped in.
    pub(crate) installed: Option<PathBuf>,
    /// The update check stage: the alerts said instead of shown (a native alert waits for a
    /// click no check can give).
    pub(crate) alerts: Vec<String>,
}

impl Global for Updates {}

fn updates_mut(cx: &mut App) -> Option<&mut Updates> {
    if cx.has_global::<Updates>() { Some(cx.global_mut::<Updates>()) } else { None }
}

/// Reads the configuration (no I/O beyond the settings already loaded).
pub fn init(core: &Core, cx: &mut App) {
    let s = core.settings.get();
    let feed = s.update_feed.clone().or_else(|| BUILD_FEED.map(str::to_string)).unwrap_or_else(|| DEFAULT_FEED.to_string());
    let key = BUILD_KEY.unwrap_or(DEFAULT_PUBKEY);
    let cfg = UpdateConfig::new(Some(&feed), Some(key), crate::VERSION).map(Arc::new);
    cx.set_global(Updates {
        cfg,
        state: State::Idle,
        last_check: None,
        auto: s.update_check,
        ws: None,
        dismissed: None,
        manual: false,
        checked: None,
        test_target: None,
        installed: None,
        alerts: Vec::new(),
    });
}

pub fn enabled(cx: &App) -> bool {
    cx.try_global::<Updates>().is_some_and(|u| u.cfg.is_some())
}

pub fn state(cx: &App) -> State {
    cx.try_global::<Updates>().map(|u| u.state.clone()).unwrap_or(State::Idle)
}

/// The last check that finished.
pub fn checked(cx: &App) -> Option<Checked> {
    cx.try_global::<Updates>()?.checked.clone()
}

/// When a check finished, as Settings and the check card say it: "14:32" today, else
/// "Oct 15, 14:32".
pub fn when(at: SystemTime) -> String {
    let at = chrono::DateTime::<chrono::Local>::from(at);
    if at.date_naive() == chrono::Local::now().date_naive() { at.format("%H:%M").to_string() } else { at.format("%b %-d, %H:%M").to_string() }
}

/// A check's outcome as an alert says it: the message, the detail, and whether it failed
/// (`None` before any outcome).
pub fn outcome_text(cx: &App) -> Option<(String, String, bool)> {
    let u = cx.try_global::<Updates>()?;
    if let Some(f) = u.state.feed() {
        return Some((format!("Dataman {} is available", f.version), format!("You have {}.", crate::VERSION), false));
    }
    match &u.checked {
        Some(Checked { error: None, .. }) => Some(("Dataman is up to date".into(), format!("{} is the newest version.", crate::VERSION), false)),
        Some(Checked { error: Some(e), .. }) => Some(("Could not check for updates".into(), e.clone(), true)),
        None => None,
    }
}

/// The version on offer.
pub fn offered(cx: &App) -> Option<Feed> {
    cx.try_global::<Updates>()?.state.feed().cloned()
}

/// Whether the title bar shows the notice: an update on offer, unless Later hid that version
/// (an install under way or failed always shows).
pub fn notice_shown(cx: &App) -> bool {
    let Some(u) = cx.try_global::<Updates>() else { return false };
    match &u.state {
        State::Available { feed, .. } => u.dismissed.as_deref() != Some(feed.version.as_str()),
        s => s.feed().is_some(),
    }
}

/// The app menu's and the palette's "Update to x.y.z…" (opens the sheet).
pub fn menu_label(cx: &App) -> Option<SharedString> {
    offered(cx).map(|f| format!("Update to {}…", f.version).into())
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

/// Runs `f` in the workspace's window; `false` when there is none left (closed).
fn with_ws(cx: &mut App, f: impl FnOnce(&mut Workspace, &mut gpui::Window, &mut gpui::Context<Workspace>) + 'static) -> bool {
    let Some((ws, window)) = cx.try_global::<Updates>().and_then(|u| u.ws.clone()) else { return false };
    window.update(cx, |_, window, cx| ws.update(cx, |w, cx| f(w, window, cx)).is_ok()).unwrap_or(false)
}

fn open_sheet(cx: &mut App) {
    with_ws(cx, |w, window, cx| w.open_update_sheet(window, cx));
}

/// A check asked for, shown where it can be seen (`done`: its outcome, else "Checking…"); with
/// no window left, an alert says the outcome. Deferred: asked for from the menu or Settings,
/// this runs inside the window's own update, where the window cannot be entered again.
fn show_check(done: bool, cx: &mut App) {
    cx.defer(move |cx| {
        if !with_ws(cx, move |w, window, cx| w.show_update_check(done, window, cx)) && done {
            crate::update_ui::alert_without_window(cx);
        }
    });
}

/// After the first frame: remembers the window, and checks a few seconds later (unless checks
/// are off in Settings).
pub fn start(ws: WeakEntity<Workspace>, window: AnyWindowHandle, cx: &mut App) {
    let Some(u) = updates_mut(cx) else { return };
    u.ws = Some((ws, window));
    if u.cfg.is_none() || !u.auto {
        return;
    }
    cx.spawn(async move |cx| {
        cx.background_executor().timer(LAUNCH_DELAY).await;
        cx.update(|cx| check(false, cx));
    })
    .detach();
}

/// On window activation: checks if the last check is a day old (no timers in between).
pub fn on_activate(cx: &mut App) {
    let Some(u) = cx.try_global::<Updates>() else { return };
    let idle = matches!(u.state, State::Idle | State::Available { .. });
    if u.cfg.is_some() && u.auto && idle && u.last_check.is_some_and(|t| t.elapsed().unwrap_or(DAY) >= DAY) {
        check(false, cx);
    }
}

/// Why Install & Relaunch cannot replace this copy, if it cannot.
fn blocked_reason(target: Option<PathBuf>) -> Option<String> {
    let app = target.or_else(|| std::env::current_exe().ok().and_then(|e| update::bundle_of(&e)));
    match app {
        None => Some("Not running from Dataman.app".to_string()),
        Some(app) => update::check_writable(&app).err().map(|e| e.to_string()),
    }
}

/// Reads the feed now. `manual` (the menu, the palette, Settings): "Checking…", then "up to
/// date", the failure or the update's sheet (the module's notes say where); otherwise only the
/// title bar's notice comes up.
pub fn check(manual: bool, cx: &mut App) {
    let Some(u) = updates_mut(cx) else { return };
    let Some(cfg) = u.cfg.clone() else { return };
    match &u.state {
        // One under way (the launch's, the daily one): it answers this request too.
        State::Checking => {
            if manual {
                u.manual = true;
                show_check(false, cx);
            }
            return;
        }
        // Already on offer: the menu shows it again.
        State::Working { .. } | State::Ready { .. } | State::Failed { .. } => {
            if manual {
                show_check(true, cx);
            }
            return;
        }
        State::Idle | State::Available { .. } => {}
    }
    u.last_check = Some(SystemTime::now());
    u.manual = manual;
    let target = u.test_target.clone();
    let before = u.state.clone();
    set_state(State::Checking, cx);
    if manual {
        show_check(false, cx);
    }
    let task = cx.background_spawn(async move {
        let r = update::check(&cfg);
        let blocked = blocked_reason(target);
        r.map(|f| f.map(|f| (f, blocked)))
    });
    cx.spawn(async move |cx| {
        let r = task.await;
        cx.update(|cx| match r {
            Ok(Some((feed, blocked))) => {
                log::info!("update: {} offered{}", feed.version, blocked.as_ref().map(|b| format!(" ({b})")).unwrap_or_default());
                let version = feed.version.clone();
                let manual = finish(None, cx);
                set_state(State::Available { feed, blocked }, cx);
                if manual {
                    if let Some(u) = updates_mut(cx) {
                        u.dismissed = None;
                    }
                    show_check(true, cx);
                } else if cx.try_global::<Updates>().is_some_and(|u| u.dismissed.as_deref() != Some(version.as_str())) {
                    crate::notify::update_available(&version, cx);
                }
                // The end-to-end test installs it at once.
                if std::env::var_os("DATAMAN_UPDATE_AUTORESTART").is_some() {
                    open_sheet(cx);
                    install(cx);
                }
            }
            Ok(None) => {
                let manual = finish(None, cx);
                set_state(State::Idle, cx);
                if manual {
                    show_check(true, cx);
                }
            }
            Err(e) => {
                log::warn!("update check: {e:#}");
                let manual = finish(Some(short(&e)), cx);
                // An offer already made stays (the feed was unreachable this time).
                set_state(if matches!(before, State::Available { .. }) { before } else { State::Idle }, cx);
                if manual {
                    show_check(true, cx);
                }
            }
        });
    })
    .detach();
}

/// A check finished: records when and how (before the state changes, so the redraw that
/// follows shows it); whether its outcome is to be said (it was asked for).
fn finish(error: Option<String>, cx: &mut App) -> bool {
    let Some(u) = updates_mut(cx) else { return false };
    u.checked = Some(Checked { at: SystemTime::now(), error });
    std::mem::take(&mut u.manual)
}

/// An error's first line, short enough for a notice or the sheet's status.
fn short(e: &anyhow::Error) -> String {
    let s = e.to_string();
    let s = s.lines().next().unwrap_or("").trim();
    if s.chars().count() > 90 { format!("{}…", s.chars().take(89).collect::<String>()) } else { s.to_string() }
}

/// "Later": hides the notice for this version (until a newer one or the next launch).
pub fn later(cx: &mut App) {
    if let Some(u) = updates_mut(cx) {
        u.dismissed = u.state.feed().map(|f| f.version.clone());
    }
    cx.refresh_windows();
}

/// Install & Relaunch (and Retry): downloads and verifies the DMG in the background, the
/// progress in the state; then the workspace quits like ⌘Q (open transactions are asked about)
/// and [`swap`] puts the new copy in place.
pub fn install(cx: &mut App) {
    let Some(u) = cx.try_global::<Updates>() else { return };
    let Some(cfg) = u.cfg.clone() else { return };
    // Retry reads the feed again first (a release may have been replaced since).
    let (feed, retry) = match &u.state {
        State::Available { feed, blocked: None } => (feed.clone(), false),
        State::Failed { feed, .. } => (feed.clone(), true),
        _ => return,
    };
    log::info!("update: installing {}", feed.version);
    set_state(State::Working { feed: feed.clone(), step: Step::Downloading { got: 0, total: feed.size } }, cx);
    let got = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(feed.size));
    let verifying = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let dir = std::env::temp_dir().join("dataman-updates");
    let task = cx.background_spawn({
        let (feed, got, total, verifying) = (feed.clone(), got.clone(), total.clone(), verifying.clone());
        async move {
            let feed = if retry { update::check(&cfg)?.ok_or_else(|| anyhow::anyhow!("No newer version on offer"))? } else { feed };
            total.store(feed.size, Ordering::Relaxed);
            let dmg = update::download(&cfg, &feed, &dir, &|p| match p {
                update::Phase::Got(n) => got.store(n, Ordering::Relaxed),
                update::Phase::Verifying => verifying.store(true, Ordering::Relaxed),
            })?;
            anyhow::Ok((feed, dmg))
        }
    });
    // The progress line, while the download runs.
    cx.spawn({
        let (feed, done) = (feed.clone(), done.clone());
        async move |cx| {
            while !done.load(Ordering::Relaxed) {
                cx.background_executor().timer(Duration::from_millis(200)).await;
                if done.load(Ordering::Relaxed) {
                    break;
                }
                let step = if verifying.load(Ordering::Relaxed) {
                    Step::Verifying
                } else {
                    Step::Downloading { got: got.load(Ordering::Relaxed), total: total.load(Ordering::Relaxed) }
                };
                cx.update(|cx| {
                    if matches!(state(cx), State::Working { .. }) {
                        set_state(State::Working { feed: feed.clone(), step }, cx);
                    }
                });
            }
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        let r = task.await;
        done.store(true, Ordering::Relaxed);
        cx.update(|cx| match r {
            Ok((feed, dmg)) => {
                log::info!("update: {} downloaded and verified", feed.version);
                set_state(State::Ready { feed, dmg }, cx);
                with_ws(cx, |w, window, cx| w.install_update(window, cx));
            }
            Err(e) => {
                log::warn!("update rejected: {e:#}");
                set_state(State::Failed { feed, error: short(&e) }, cx);
            }
        });
    })
    .detach();
}

/// Whether this run is the update check stage (no relaunch, no quit).
pub fn testing(cx: &App) -> bool {
    cx.try_global::<Updates>().is_some_and(|u| u.test_target.is_some())
}

/// Swaps the app for the downloaded one, off the UI thread; resolves to the app's path (to
/// relaunch). On failure the sheet says why, with Retry.
pub fn swap(cx: &mut App) -> Option<gpui::Task<anyhow::Result<PathBuf>>> {
    let u = cx.try_global::<Updates>()?;
    let State::Ready { feed, dmg } = u.state.clone() else { return None };
    let app = u.test_target.clone().or_else(|| std::env::current_exe().ok().and_then(|e| update::bundle_of(&e)))?;
    log::info!("update: installing {} over {}", feed.version, app.display());
    set_state(State::Working { feed: feed.clone(), step: Step::Installing }, cx);
    let task = cx.background_spawn({
        let (version, dmg) = (feed.version.clone(), dmg.clone());
        async move {
            update::install(&dmg, &app, &version)?;
            let _ = std::fs::remove_file(&dmg);
            anyhow::Ok(app)
        }
    });
    Some(cx.spawn(async move |cx| {
        let r = task.await;
        cx.update(|cx| match &r {
            Err(e) => {
                log::warn!("update install: {e:#}");
                set_state(State::Failed { feed, error: short(e) }, cx);
            }
            Ok(app) if testing(cx) => {
                if let Some(u) = updates_mut(cx) {
                    u.installed = Some(app.clone());
                }
                set_state(State::Idle, cx);
            }
            Ok(_) => {}
        });
        r
    }))
}

/// The update check stage: the feed, key and bundle it plays against.
pub fn configure_for_test(feed: &str, key: &str, target: PathBuf, cx: &mut App) {
    if let Some(u) = updates_mut(cx) {
        u.cfg = UpdateConfig::new(Some(feed), Some(key), crate::VERSION).map(Arc::new);
        u.state = State::Idle;
        u.dismissed = None;
        u.installed = None;
        u.checked = None;
        u.alerts.clear();
        u.test_target = Some(target);
    }
}

/// The update check stage: the window outcomes go to (`None`: as if it had been closed).
pub fn set_ws_for_test(ws: Option<(WeakEntity<Workspace>, AnyWindowHandle)>, cx: &mut App) {
    if let Some(u) = updates_mut(cx) {
        u.ws = ws;
    }
}

/// The update check stage: an alert said instead of shown.
pub(crate) fn record_alert(text: String, cx: &mut App) {
    if let Some(u) = updates_mut(cx) {
        u.alerts.push(text);
    }
}

/// Screenshots: an update on offer (`step`: one under way), as if the feed had offered it.
pub fn demo_offer(step: Option<Step>, cx: &mut App) {
    if let Some(u) = updates_mut(cx) {
        let zero_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        u.cfg = UpdateConfig::new(Some("http://127.0.0.1/feed.json"), Some(zero_key), crate::VERSION).map(Arc::new);
    }
    let feed = Feed {
        version: "0.2.1".into(),
        notes: "Faster result grid scrolling.\nSQL Server: table DDL with indexes and constraints.\nThe connection sheet remembers the last SSH key used.\nFixes a crash when a MySQL result had no columns.".into(),
        url: String::new(),
        size: 34_603_008,
        ed25519: String::new(),
        minimum_macos: None,
        published: None,
    };
    let state = match step {
        Some(step) => State::Working { feed, step },
        None => State::Available { feed, blocked: None },
    };
    set_state(state, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn when_a_check_finished() {
        let now = SystemTime::now();
        let today = when(now);
        assert_eq!(today.len(), 5, "{today}");
        assert!(today.contains(':') && !today.contains(','), "{today}");
        let earlier = when(now - 3 * DAY);
        assert!(earlier.contains(", ") && earlier.len() > 5, "{earlier}");
    }
}
