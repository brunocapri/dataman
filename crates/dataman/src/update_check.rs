use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels, Point};
use dataman_core::update::{Feed, ThrowawayKey};

use crate::app::Workspace;
use crate::updates::{self, State, Step};

const NEW: &str = "99.0.0";
#[cfg(target_os = "macos")]
const BUNDLE_ID: &str = "dev.dataman.update-check";

pub fn is_stage(s: &str) -> bool {
    s == "update-check"
}

fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn input(cx: &mut AsyncWindowContext, e: PlatformInput) {
    let _ = cx.update(|window, cx| {
        window.dispatch_event(e, cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn click(cx: &mut AsyncWindowContext, p: Point<Pixels>) {
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: None, modifiers: Modifiers::default() }));
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: p, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: p, modifiers: Modifiers::default(), click_count: 1 }));
}

fn press(cx: &mut AsyncWindowContext, k: &str) {
    draw(cx);
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(crate::keymap::keystroke(k), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

async fn wait(cx: &mut AsyncWindowContext, secs: u64, mut done: impl FnMut(&mut AsyncWindowContext) -> bool) -> bool {
    for _ in 0..secs * 20 {
        draw(cx);
        if done(cx) {
            return true;
        }
        pause(cx, 50).await;
    }
    false
}

fn menu_check(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::app::CheckForUpdates), cx));
    draw(cx);
}

fn snap(cx: &mut AsyncWindowContext, name: &str) {
    let Some(dir) = std::env::var_os("DATAMAN_SHOT_DIR") else { return };
    let path = Path::new(&dir).join(format!("{name}.png"));
    draw(cx);
    let _ = cx.update(|window, cx| crate::bench::capture(window, &path.to_string_lossy(), cx));
}

fn state(cx: &mut AsyncWindowContext) -> State {
    cx.update(|_, cx| updates::state(cx)).unwrap_or(State::Idle)
}

#[cfg(not(target_os = "macos"))]
const APP: &str = "Dataman.AppImage";
#[cfg(target_os = "macos")]
const APP: &str = "Dataman.app";

/// An AppImage stand-in: a script that says its version, as `dataman --version` does.
#[cfg(not(target_os = "macos"))]
fn stand_in(app: &Path, version: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(app.parent().unwrap_or(Path::new("/")))?;
    std::fs::write(app, format!("#!/bin/sh\necho {version}\n"))?;
    std::fs::set_permissions(app, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn artifact(work: &Path, _stage_dir: &Path) -> anyhow::Result<Vec<u8>> {
    let new = work.join(format!("Dataman-{NEW}.AppImage"));
    stand_in(&new, NEW)?;
    Ok(std::fs::read(&new)?)
}

#[cfg(target_os = "macos")]
fn artifact(work: &Path, stage_dir: &Path) -> anyhow::Result<Vec<u8>> {
    let dmg_path = work.join(format!("Dataman-{NEW}.dmg"));
    stand_in(&stage_dir.join(APP), NEW)?;
    sh(Command::new("/usr/bin/hdiutil").args(["create", "-quiet", "-volname", "Dataman", "-fs", "HFS+", "-format", "UDZO", "-ov", "-srcfolder"]).arg(stage_dir).arg(&dmg_path))?;
    Ok(std::fs::read(&dmg_path)?)
}

#[cfg(target_os = "macos")]
fn stand_in(app: &Path, version: &str) -> anyhow::Result<()> {
    let macos = app.join("Contents/MacOS");
    std::fs::create_dir_all(&macos)?;
    std::fs::copy("/usr/bin/true", macos.join("dataman"))?;
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\
         <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>\
         <key>CFBundleExecutable</key><string>dataman</string>\
         <key>CFBundlePackageType</key><string>APPL</string>\
         <key>CFBundleShortVersionString</key><string>{version}</string>\
         <key>CFBundleVersion</key><string>{version}</string>\
         </dict></plist>\n"
    );
    std::fs::write(app.join("Contents/Info.plist"), plist)?;
    sh(Command::new("/usr/bin/codesign").args(["--force", "--sign", "-"]).arg(app))
}

#[cfg(target_os = "macos")]
fn sh(cmd: &mut Command) -> anyhow::Result<()> {
    let out = cmd.output()?;
    if !out.status.success() {
        anyhow::bail!("{:?}: {}", cmd, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[derive(Clone, Default)]
struct Site {
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    log: Arc<Mutex<Vec<String>>>,
    feed_delay: Arc<AtomicU64>,
}

impl Site {
    fn put(&self, path: &str, body: Vec<u8>) {
        self.files.lock().unwrap().insert(path.to_string(), body);
    }

    fn remove(&self, path: &str) {
        self.files.lock().unwrap().remove(path);
    }

    fn serve(&self) -> anyhow::Result<u16> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let site = self.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let site = site.clone();
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut line = String::new();
                    let mut reader = BufReader::new(match stream.try_clone() {
                        Ok(s) => s,
                        Err(_) => return,
                    });
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let mut h = String::new();
                    while reader.read_line(&mut h).is_ok_and(|n| n > 2) {
                        h.clear();
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    site.log.lock().unwrap().push(path.clone());
                    if path == "/feed.json" {
                        std::thread::sleep(Duration::from_millis(site.feed_delay.load(Ordering::Relaxed)));
                    }
                    let body = site.files.lock().unwrap().get(&path).cloned();
                    let Some(body) = body else {
                        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        return;
                    };
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    if stream.write_all(head.as_bytes()).is_err() {
                        return;
                    }
                    let slow = body.len() > 4096;
                    let step = if slow { body.len().div_ceil(12) } else { body.len().max(1) };
                    for chunk in body.chunks(step) {
                        if stream.write_all(chunk).is_err() {
                            return;
                        }
                        let _ = stream.flush();
                        if slow {
                            std::thread::sleep(Duration::from_millis(80));
                        }
                    }
                });
            }
        });
        Ok(port)
    }
}

fn feed_json(version: &str, url: &str, dmg: &[u8], sig: String) -> Vec<u8> {
    let mut feed = Feed {
        version: version.into(),
        notes: "A stand-in release for the update check.\nIts DMG holds a small signed bundle.".into(),
        url: url.into(),
        size: dmg.len() as u64,
        ed25519: sig,
        minimum_macos: None,
        published: None,
        assets: Default::default(),
    };
    if !cfg!(target_os = "macos") {
        // As a release's feed: the top-level download is the Mac's, this one is in `assets`.
        let asset = dataman_core::update::Asset { url: feed.url.clone(), size: feed.size, ed25519: feed.ed25519.clone() };
        feed.assets.insert(dataman_core::update::platform(), asset);
        (feed.url, feed.size) = ("http://127.0.0.1:9/Dataman.dmg".into(), 1);
    }
    serde_json::to_vec(&feed).unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn plist_version(app: &Path) -> String {
    dataman_core::update::plist_value(app, "CFBundleShortVersionString").unwrap_or_default()
}

#[cfg(not(target_os = "macos"))]
fn plist_version(app: &Path) -> String {
    Command::new(app).arg("--version").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn installed_ok(target: &Path) -> (bool, bool) {
    let signed = Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(target).status().is_ok_and(|s| s.success());
    let quarantined = Command::new("/usr/bin/xattr").args(["-p", "com.apple.quarantine"]).arg(target).output().is_ok_and(|o| o.status.success());
    (signed, quarantined)
}

#[cfg(not(target_os = "macos"))]
fn installed_ok(target: &Path) -> (bool, bool) {
    use std::os::unix::fs::PermissionsExt as _;
    (std::fs::metadata(target).is_ok_and(|m| m.permissions().mode() & 0o111 != 0), false)
}

pub async fn run(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let work = std::env::temp_dir().join(format!("dataman-update-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let apps = work.join("Applications");
    let target: PathBuf = apps.join(APP);
    let stage_dir = work.join("new");
    let built = (|| -> anyhow::Result<Vec<u8>> {
        stand_in(&target, "0.0.1")?;
        artifact(&work, &stage_dir)
    })();
    let dmg = match built {
        Ok(b) => b,
        Err(e) => {
            report("the stand-in bundles and their DMG are built", false, format!("{e:#}"));
            return false;
        }
    };
    report("the stand-in bundles and their DMG are built", plist_version(&target) == "0.0.1" && !dmg.is_empty(), format!("{} bytes", dmg.len()));

    let site = Site::default();
    let port = match site.serve() {
        Ok(p) => p,
        Err(e) => {
            report("the feed is served on 127.0.0.1", false, format!("{e:#}"));
            return false;
        }
    };
    report("the feed is served on a free loopback port (not 7700)", port != 7700, format!("port {port}"));
    let base = format!("http://127.0.0.1:{port}");
    let key = match ThrowawayKey::new() {
        Ok(k) => k,
        Err(e) => {
            report("a key for the run", false, format!("{e:#}"));
            return false;
        }
    };
    let dmg_url = format!("{base}/Dataman-{NEW}.dmg");
    site.put(&format!("/Dataman-{NEW}.dmg"), dmg.clone());
    let handle = cx.update(|window, _| window.window_handle()).ok();
    let _ = cx.update(|_, cx| {
        updates::configure_for_test(&format!("{base}/feed.json"), &key.public(), target.clone(), cx);
        updates::set_ws_for_test(handle.map(|h| (ws.downgrade(), h)), cx);
    });

    site.put("/feed.json", feed_json(crate::VERSION, &dmg_url, &dmg, key.sign(&dmg)));
    site.feed_delay.store(700, Ordering::Relaxed);
    let sheet_open = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).update_sheet).unwrap_or(false);
    let up_to_date = |cx: &mut AsyncWindowContext| cx.update(|_, cx| updates::checked(cx).is_some_and(|c| c.error.is_none())).unwrap_or(false);
    menu_check(cx);
    let checking = wait(cx, 3, |cx| sheet_open(cx) && state(cx) == State::Checking).await;
    snap(cx, "check-menu-checking");
    report("Check for Updates (the menu): the sheet says \"Checking…\" while the feed is read", checking, format!("{:?}, sheet {}", state(cx), sheet_open(cx)));
    let said = wait(cx, 10, |cx| state(cx) == State::Idle && up_to_date(cx)).await;
    let text = cx.update(|_, cx| updates::outcome_text(cx).map(|o| o.0)).ok().flatten().unwrap_or_default();
    snap(cx, "check-menu-up-to-date");
    report("then \"Dataman is up to date\", in the sheet", said && sheet_open(cx) && text == "Dataman is up to date", format!("{:?}, sheet {}, {text:?}", state(cx), sheet_open(cx)));
    let buttons = cx.update(|_, cx| ws.read(cx).update_button_bounds.borrow().len()).unwrap_or(0);
    press(cx, "enter");
    report("its OK (↩) closes it", buttons == 1 && !sheet_open(cx), format!("{buttons} buttons, sheet {}", sheet_open(cx)));

    let _ = cx.update(|_, cx| updates::check(false, cx));
    draw(cx);
    let was_auto = state(cx) == State::Checking && !sheet_open(cx);
    menu_check(cx);
    let joined = wait(cx, 10, |cx| state(cx) == State::Idle && sheet_open(cx) && up_to_date(cx)).await;
    report("asked for during an automatic check: the outcome is still said", was_auto && joined, format!("auto first {was_auto}, {:?}, sheet {}", state(cx), sheet_open(cx)));
    press(cx, "enter");
    let fresh = cx.update(|_, cx| updates::checked(cx).is_some_and(|c| c.at.elapsed().unwrap_or_default() < Duration::from_secs(5))).unwrap_or(false);
    report("a check asked for right after another still reads the feed (no throttle)", fresh, String::new());

    let settings = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).settings_view.clone()).ok().flatten();
    let status = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).settings_view.as_ref().and_then(|v| v.read(cx).demo_update_status(cx))).ok().flatten().unwrap_or_default();
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_settings_at("about", window, cx)));
    draw(cx);
    draw(cx);
    let point = settings(cx).and_then(|v| cx.update(|_, cx| v.read(cx).demo_check_now_point()).ok().flatten());
    let before = site.log.lock().unwrap().len();
    if let Some(p) = point {
        click(cx, p);
    }
    let checking = wait(cx, 3, |cx| state(cx) == State::Checking).await;
    let shown = status(cx);
    snap(cx, "check-settings-checking");
    if let Some(p) = point {
        click(cx, p);
    }
    report("Check now: \"Checking…\" beside it, Settings open", point.is_some() && checking && shown == "◌ Checking…" && settings(cx).is_some(), format!("point {point:?}, {:?}, {shown:?}", state(cx)));
    let said = wait(cx, 10, |cx| state(cx) == State::Idle && status(cx).starts_with("Dataman is up to date · checked ")).await;
    snap(cx, "check-settings-up-to-date");
    report("then \"Dataman is up to date · checked HH:MM\"", said, status(cx));
    let requests = site.log.lock().unwrap().len() - before;
    report("Check now is off while the check runs (one request for two clicks)", requests == 1, format!("{requests} requests"));
    report("no sheet over Settings for it", !sheet_open(cx) && settings(cx).is_some(), format!("sheet {}", sheet_open(cx)));

    if let Some(v) = settings(cx) {
        let _ = cx.update(|_, cx| v.update(cx, |v, cx| v.show_section("appearance", cx)));
    }
    draw(cx);
    menu_check(cx);
    let about = wait(cx, 3, |cx| settings(cx).is_some_and(|v| cx.update(|_, cx| v.read(cx).demo_section() == "About").unwrap_or(false))).await;
    let said = wait(cx, 10, |cx| state(cx) == State::Idle && status(cx).starts_with("Dataman is up to date")).await;
    report("Check for Updates with Settings open: About says it", about && said && !sheet_open(cx), format!("about {about}, {:?}", status(cx)));

    site.remove("/feed.json");
    if let Some(p) = point {
        click(cx, p);
    }
    let failed = wait(cx, 10, |cx| state(cx) == State::Idle && status(cx).starts_with("Check failed: ")).await;
    snap(cx, "check-settings-failed");
    report("a failed check: \"Check failed: …\" beside Check now", failed, status(cx));
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.toggle_settings(window, cx)));
    draw(cx);
    menu_check(cx);
    let failed = wait(cx, 10, |cx| state(cx) == State::Idle && sheet_open(cx) && cx.update(|_, cx| updates::checked(cx).is_some_and(|c| c.error.is_some())).unwrap_or(false)).await;
    draw(cx);
    let buttons = cx.update(|_, cx| ws.read(cx).update_button_bounds.borrow().len()).unwrap_or(0);
    snap(cx, "check-menu-failed");
    report("a failed check from the menu: the sheet says why, with Try Again and OK", failed && buttons == 2, format!("{:?}, {buttons} buttons", state(cx)));
    press(cx, "escape");
    site.put("/feed.json", feed_json(crate::VERSION, &dmg_url, &dmg, key.sign(&dmg)));
    site.feed_delay.store(0, Ordering::Relaxed);

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_import(window, cx)));
    draw(cx);
    let alerts = |cx: &mut AsyncWindowContext| cx.update(|_, cx| cx.try_global::<updates::Updates>().map(|u| u.alerts.clone()).unwrap_or_default()).unwrap_or_default();
    menu_check(cx);
    let alerted = wait(cx, 10, |cx| alerts(cx).iter().any(|a| a == "window: Dataman is up to date")).await;
    report("under another sheet: an alert on the window", alerted && !sheet_open(cx), format!("{:?}", alerts(cx)));
    let _ = cx.update(|_, cx| {
        ws.update(cx, |w, cx| {
            w.import = None;
            cx.notify();
        })
    });

    let _ = cx.update(|_, cx| {
        updates::set_ws_for_test(None, cx);
        updates::check(true, cx);
    });
    let alerted = wait(cx, 10, |cx| alerts(cx).iter().any(|a| a == "app: Dataman is up to date")).await;
    report("with no window: an alert of the app's own", alerted, format!("{:?}", alerts(cx)));
    let _ = cx.update(|_, cx| updates::set_ws_for_test(handle.map(|h| (ws.downgrade(), h)), cx));

    let wrong = ThrowawayKey::new().map(|k| k.sign(&dmg)).unwrap_or_default();
    site.put("/feed.json", feed_json(NEW, &dmg_url, &dmg, wrong));
    let _ = cx.update(|_, cx| updates::check(false, cx));
    let offered = wait(cx, 10, |cx| matches!(state(cx), State::Available { .. })).await;
    let pill = cx.update(|_, cx| ws.read(cx).update_pill_bounds.borrow().first().copied()).ok().flatten();
    let sheet = cx.update(|_, cx| ws.read(cx).update_sheet).unwrap_or(true);
    report("a background check finds 99.0.0: the title bar's notice, no sheet", offered && pill.is_some() && !sheet, format!("{:?}, pill {pill:?}, sheet {sheet}", state(cx)));
    let label = cx.update(|_, cx| updates::notice_shown(cx) && updates::offered(cx).is_some_and(|f| f.version == NEW)).unwrap_or(false);
    report("the notice is for 99.0.0", label, String::new());

    if let Some(b) = pill {
        click(cx, b.center());
    }
    let opened = wait(cx, 3, |cx| cx.update(|_, cx| ws.read(cx).update_sheet).unwrap_or(false)).await;
    let buttons = cx.update(|_, cx| ws.read(cx).update_button_bounds.borrow().clone()).unwrap_or_default();
    report("clicking the notice opens the sheet (Later, Install & Relaunch)", opened && buttons.len() == 2, format!("open {opened}, {} buttons", buttons.len()));
    if let Some(b) = buttons.first() {
        click(cx, b.center());
    }
    draw(cx);
    let (closed, hidden) = cx.update(|_, cx| (!ws.read(cx).update_sheet, !updates::notice_shown(cx))).unwrap_or((false, false));
    report("Later closes the sheet and hides the notice", closed && hidden, format!("closed {closed}, hidden {hidden}"));

    let _ = cx.update(|_, cx| updates::check(true, cx));
    let back = wait(cx, 10, |cx| cx.update(|_, cx| ws.read(cx).update_sheet && updates::notice_shown(cx)).unwrap_or(false)).await;
    report("Check for Updates opens the sheet again", back, format!("{:?}", state(cx)));

    press(cx, "enter");
    let mut progress = false;
    let failed = wait(cx, 20, |cx| {
        let s = state(cx);
        if let State::Working { step: Step::Downloading { got, total }, .. } = &s
            && *got > 0
            && got < total
        {
            progress = true;
        }
        matches!(s, State::Failed { .. })
    })
    .await;
    report("Install & Relaunch shows the download's progress (bytes of total)", progress, format!("{:?}", state(cx)));
    let error = match state(cx) {
        State::Failed { error, .. } => error,
        s => format!("{s:?}"),
    };
    report("a DMG signed with another key is refused, with Retry", failed && error.contains("signature"), error);
    report("the app is untouched after the refusal", plist_version(&target) == "0.0.1", plist_version(&target));
    let notice_failed = cx.update(|_, cx| updates::notice_shown(cx) && ws.read(cx).update_sheet).unwrap_or(false);
    report("the sheet stays up with the error", notice_failed, String::new());

    site.put("/feed.json", feed_json(NEW, &dmg_url, &dmg, key.sign(&dmg)));
    draw(cx);
    let buttons = cx.update(|_, cx| ws.read(cx).update_button_bounds.borrow().clone()).unwrap_or_default();
    if let Some(b) = buttons.get(1) {
        click(cx, b.center());
    }
    let mut verifying = false;
    let installed = wait(cx, 30, |cx| {
        if matches!(state(cx), State::Working { step: Step::Verifying | Step::Installing, .. } | State::Ready { .. }) {
            verifying = true;
        }
        cx.update(|_, cx| cx.try_global::<updates::Updates>().is_some_and(|u| u.installed.is_some())).unwrap_or(false)
    })
    .await;
    report("Retry downloads, verifies and installs", installed, format!("{:?}", state(cx)));
    report("the steps after the download show (verifying / installing)", verifying, String::new());
    report("the bundle is now 99.0.0", plist_version(&target) == NEW, plist_version(&target));
    let (signed, quarantined) = installed_ok(&target);
    report(if cfg!(target_os = "macos") { "its signature is valid" } else { "it is executable" }, signed, String::new());
    report("no quarantine on the installed bundle", !quarantined, String::new());
    let left: Vec<String> = std::fs::read_dir(&apps).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    report("nothing left beside it (staging copy, old app)", left == [APP], format!("{left:?}"));
    let (sheet, notice) = cx.update(|_, cx| (ws.read(cx).update_sheet, updates::notice_shown(cx))).unwrap_or((true, true));
    report("after the install: no sheet, no notice", !sheet && !notice, format!("sheet {sheet}, notice {notice}"));
    let log = site.log.lock().unwrap().clone();
    let dmg_gets = log.iter().filter(|p| p.ends_with(".dmg")).count();
    report("the requests: the feed and the DMG (twice: refused, then installed)", log.iter().any(|p| p == "/feed.json") && dmg_gets == 2, format!("{log:?}"));
    let _ = std::fs::remove_dir_all(&work);
    ok
}
