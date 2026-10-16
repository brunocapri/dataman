//! `update-check` (`--demo`): the update notice end to end, offline. A feed and a DMG served
//! from 127.0.0.1 (a free port of the system's choosing), signed with a key made for the run,
//! offer version 99.0.0 of a small stand-in bundle; the stage replaces a stand-in Diskman.app
//! in a scratch folder (never this app, never /Applications) and does not relaunch.
//!
//! Checked: "Diskman is up to date" for a feed that offers nothing newer; the title bar's notice
//! after a background check (and no sheet); the notice opens the sheet; Later closes it and
//! hides the notice; a manual check brings the sheet back; Install & Relaunch (↩) shows the
//! download's progress, and a DMG whose signature does not match is refused with Retry, the
//! app untouched; Retry (after the feed is fixed) downloads, verifies and swaps in the new
//! bundle: its version, a valid signature, no quarantine, nothing left beside it. PASS/FAIL
//! lines, exit code 1 on a failure.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels, Point};
use diskman_core::update::{Feed, ThrowawayKey};

use crate::app::Workspace;
use crate::updates::{self, State, Step};

const NEW: &str = "99.0.0";
const BUNDLE_ID: &str = "dev.diskman.update-check";

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
        window.dispatch_keystroke(gpui::Keystroke::parse(k).unwrap(), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

/// Polls `done` every 50 ms for up to `secs`, drawing in between.
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

fn state(cx: &mut AsyncWindowContext) -> State {
    cx.update(|_, cx| updates::state(cx)).unwrap_or(State::Idle)
}

/// A stand-in app bundle at `app`: an Info.plist with `version`, `/usr/bin/true` as its
/// executable, signed ad hoc.
fn stand_in(app: &Path, version: &str) -> anyhow::Result<()> {
    let macos = app.join("Contents/MacOS");
    std::fs::create_dir_all(&macos)?;
    std::fs::copy("/usr/bin/true", macos.join("diskman"))?;
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\
         <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>\
         <key>CFBundleExecutable</key><string>diskman</string>\
         <key>CFBundlePackageType</key><string>APPL</string>\
         <key>CFBundleShortVersionString</key><string>{version}</string>\
         <key>CFBundleVersion</key><string>{version}</string>\
         </dict></plist>\n"
    );
    std::fs::write(app.join("Contents/Info.plist"), plist)?;
    sh(Command::new("/usr/bin/codesign").args(["--force", "--sign", "-"]).arg(app))
}

fn sh(cmd: &mut Command) -> anyhow::Result<()> {
    let out = cmd.output()?;
    if !out.status.success() {
        anyhow::bail!("{:?}: {}", cmd, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// What the server serves (path → body), and the requests it saw.
#[derive(Clone, Default)]
struct Site {
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    log: Arc<Mutex<Vec<String>>>,
}

impl Site {
    fn put(&self, path: &str, body: Vec<u8>) {
        self.files.lock().unwrap().insert(path.to_string(), body);
    }

    /// Serves on a free loopback port; the body goes out in 12 slices 80 ms apart, so the
    /// download's progress has time to show. Returns the port.
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
                    // The rest of the request head.
                    let mut h = String::new();
                    while reader.read_line(&mut h).is_ok_and(|n| n > 2) {
                        h.clear();
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    site.log.lock().unwrap().push(path.clone());
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
    let feed = Feed {
        version: version.into(),
        notes: "A stand-in release for the update check.\nIts DMG holds a small signed bundle.".into(),
        url: url.into(),
        size: dmg.len() as u64,
        ed25519: sig,
        minimum_macos: None,
        published: None,
    };
    serde_json::to_vec(&feed).unwrap_or_default()
}

fn plist_version(app: &Path) -> String {
    diskman_core::update::plist_value(app, "CFBundleShortVersionString").unwrap_or_default()
}

/// Runs the stage; `false` when a check failed.
pub async fn run(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let work = std::env::temp_dir().join(format!("diskman-update-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let apps = work.join("Applications");
    let target: PathBuf = apps.join("Diskman.app");
    let stage_dir = work.join("new");
    let dmg_path = work.join(format!("Diskman-{NEW}.dmg"));
    let built = (|| -> anyhow::Result<Vec<u8>> {
        stand_in(&target, "0.0.1")?;
        stand_in(&stage_dir.join("Diskman.app"), NEW)?;
        sh(Command::new("/usr/bin/hdiutil").args(["create", "-quiet", "-volname", "Diskman", "-fs", "HFS+", "-format", "UDZO", "-ov", "-srcfolder"]).arg(&stage_dir).arg(&dmg_path))?;
        Ok(std::fs::read(&dmg_path)?)
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
    let dmg_url = format!("{base}/Diskman-{NEW}.dmg");
    site.put(&format!("/Diskman-{NEW}.dmg"), dmg.clone());
    let _ = cx.update(|window, cx| {
        updates::configure_for_test(&format!("{base}/feed.json"), &key.public(), target.clone(), cx);
        updates::set_ws_for_test(ws.downgrade(), window.window_handle(), cx);
    });

    // Nothing newer: the manual check says so.
    site.put("/feed.json", feed_json(crate::VERSION, &dmg_url, &dmg, key.sign(&dmg)));
    let _ = cx.update(|_, cx| updates::check(true, cx));
    let said = wait(cx, 10, |cx| cx.update(|_, cx| ws.read(cx).flash_notice.as_ref().is_some_and(|(m, _)| m == "Diskman is up to date")).unwrap_or(false)).await;
    report("a feed with nothing newer: \"Diskman is up to date\"", said && state(cx) == State::Idle, format!("{:?}", state(cx)));

    // 99.0.0 on offer, its DMG signed by another key: the background check raises the notice.
    let wrong = ThrowawayKey::new().map(|k| k.sign(&dmg)).unwrap_or_default();
    site.put("/feed.json", feed_json(NEW, &dmg_url, &dmg, wrong));
    let _ = cx.update(|_, cx| updates::check(false, cx));
    let offered = wait(cx, 10, |cx| matches!(state(cx), State::Available { .. })).await;
    let pill = cx.update(|_, cx| ws.read(cx).update_pill_bounds.borrow().first().copied()).ok().flatten();
    let sheet = cx.update(|_, cx| ws.read(cx).update_sheet).unwrap_or(true);
    report("a background check finds 99.0.0: the title bar's notice, no sheet", offered && pill.is_some() && !sheet, format!("{:?}, pill {pill:?}, sheet {sheet}", state(cx)));
    let label = cx.update(|_, cx| updates::notice_shown(cx) && updates::offered(cx).is_some_and(|f| f.version == NEW)).unwrap_or(false);
    report("the notice is for 99.0.0", label, String::new());

    // The notice opens the sheet; Later closes it and hides the notice.
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

    // A manual check brings it back.
    let _ = cx.update(|_, cx| updates::check(true, cx));
    let back = wait(cx, 10, |cx| cx.update(|_, cx| ws.read(cx).update_sheet && updates::notice_shown(cx)).unwrap_or(false)).await;
    report("Check for Updates opens the sheet again", back, format!("{:?}", state(cx)));

    // ↩ installs: the progress shows, then the signature is refused, with Retry.
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

    // The feed fixed: Retry reads it again, downloads, verifies and swaps.
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
    let signed = Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(&target).status().is_ok_and(|s| s.success());
    report("its signature is valid", signed, String::new());
    let quarantined = Command::new("/usr/bin/xattr").args(["-p", "com.apple.quarantine"]).arg(&target).output().is_ok_and(|o| o.status.success());
    report("no quarantine on the installed bundle", !quarantined, String::new());
    let left: Vec<String> = std::fs::read_dir(&apps).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    report("nothing left beside it (staging copy, old app)", left == ["Diskman.app"], format!("{left:?}"));
    let (sheet, notice) = cx.update(|_, cx| (ws.read(cx).update_sheet, updates::notice_shown(cx))).unwrap_or((true, true));
    report("after the install: no sheet, no notice", !sheet && !notice, format!("sheet {sheet}, notice {notice}"));
    let log = site.log.lock().unwrap().clone();
    let dmg_gets = log.iter().filter(|p| p.ends_with(".dmg")).count();
    report("the requests: the feed and the DMG (twice: refused, then installed)", log.iter().any(|p| p == "/feed.json") && dmg_gets == 2, format!("{log:?}"));
    let _ = std::fs::remove_dir_all(&work);
    ok
}
