//! Demo stages of the saved session (`--demo`), in a session directory of their own (never the
//! user's):
//!
//! - `session`: two tabs saved and the window's workspace rebuilt from them (screenshot);
//! - `session-check`: the same, checked — a tab with rows, a script that stopped at an error,
//!   the tab in front, the SQL, selection, rows, the error and its tab come back without
//!   connecting, a tab's link to its favorite too; the restored result refuses grid edits; the
//!   tab connects on its first run and runs there; a damaged session file opens a fresh
//!   workspace. PASS/FAIL lines, exit code 1 on a failure.

use std::sync::Arc;
use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Focusable};
use diskman_core::result::Phase;
use diskman_core::session::SessionStore;

use crate::app::Workspace;
use crate::restore::Sessions;
use crate::tab::QueryTab;

pub fn is_stage(s: &str) -> bool {
    s == "session" || s == "session-check"
}

fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

async fn wait(cx: &mut AsyncWindowContext, mut done: impl FnMut(&mut AsyncWindowContext) -> bool) -> bool {
    for _ in 0..300 {
        if done(cx) {
            return true;
        }
        pause(cx, 50).await;
    }
    false
}

/// Runs `sql` in `tab` (`script`: the whole script, else the statement) and waits for its end.
async fn run(ws: &Entity<Workspace>, tab: &Entity<QueryTab>, sql: &str, script: bool, cx: &mut AsyncWindowContext) {
    let ix = cx.update(|_, cx| ws.read(cx).tabs.iter().position(|t| t == tab)).ok().flatten().unwrap_or(0);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.activate(ix, window, cx)));
    let editor = cx.update(|_, cx| tab.read(cx).editor.clone()).unwrap();
    let _ = cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.set_text(sql, cx));
        window.focus(&editor.focus_handle(cx), cx);
    });
    draw(cx);
    crate::editor::dispatch(cx, if script { Box::new(crate::editor::RunScript) } else { Box::new(crate::editor::RunStatement) });
    wait(cx, |cx| cx.update(|_, cx| !tab.read(cx).is_running() && tab.read(cx).run_phase().is_some_and(|p| p.is_finished())).unwrap_or(true)).await;
    draw(cx);
}

/// The window's workspace rebuilt from the saved session, as at launch.
fn relaunch(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Entity<Workspace> {
    let core = cx.update(|_, cx| ws.read(cx).store.read(cx).core.clone()).unwrap();
    // The old one leaves quietly: it must not save over the new one later (on quit).
    let _ = cx.update(|_, cx| {
        ws.update(cx, |w, _| {
            w.session_off = true;
            w.session_task = None;
        })
    });
    let ws = cx.update(|window, cx| window.replace_root(cx, |window, cx| Workspace::new(core, window, cx))).unwrap();
    draw(cx);
    ws
}

/// Runs a stage; `false` when a check failed.
pub async fn run_stage(ws: &Entity<Workspace>, stage: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        if stage == "session-check" {
            println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        }
        ok &= pass;
    };
    // A session directory of this run's own.
    let dir = std::env::temp_dir().join(format!("diskman-session-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(SessionStore::new(&dir));
    let _ = cx.update(|_, cx| cx.set_global(Sessions(Some(store.clone()))));

    // The demo's tab already ran its query (a paused result past the row cap); a second tab
    // runs a script that stops at its second statement.
    let first = cx.update(|_, cx| ws.read(cx).tabs[0].clone()).unwrap();
    wait(cx, |cx| cx.update(|_, cx| first.read(cx).run_phase().is_some_and(|p| p != Phase::Running)).unwrap_or(true)).await;
    let script = cx.update(|window, cx| ws.update(cx, |w, cx| w.new_tab(window, cx))).unwrap();
    run(ws, &script, "SELECT g AS n, 'line ' || g AS label FROM generate_series(1, 25) g;\nSELECT * FROM no_such_table_here;\nSELECT 3;", true, cx).await;
    // The script's tab is linked to a favorite (⌘S saves into it): the link comes back.
    let fav = cx
        .update(|_, cx| {
            let core = ws.read(cx).store.read(cx).core.clone();
            let f = core.favorites.add(diskman_core::Favorite::new("Session favorite", "SELECT 'linked'")).ok()?;
            script.update(cx, |t, _| t.favorite = Some((f.id.clone(), f.name.clone().into())));
            Some(f.id)
        })
        .ok()
        .flatten();
    // The first tab in front, with a selection in its editor.
    let _ = cx.update(|window, cx| {
        ws.update(cx, |w, cx| {
            w.activate(0, window, cx);
            // A wider explorer and the history open come back too.
            w.explorer_w = 300.;
            w.toggle_history_for_demo(cx);
        });
        first.update(cx, |t, cx| t.editor.update(cx, |e, cx| e.demo_select(3, 9, cx)));
    });
    draw(cx);
    let before = cx
        .update(|_, cx| {
            let w = ws.read(cx);
            let tabs: Vec<(String, (usize, usize), Option<String>, Option<Phase>)> = w
                .tabs
                .iter()
                .map(|t| {
                    let t = t.read(cx);
                    let e = t.editor.read(cx);
                    (e.text().to_string(), e.selection_ends(), t.connection().map(|c| c.profile.id.clone()), t.run_phase())
                })
                .collect();
            let rows = first.read(cx).result().map_or(0, |r| r.rows());
            let cell = first.read(cx).result().and_then(|r| r.snapshot().text(0, 0).map(|s| s.into_owned()));
            (tabs, w.active, rows, cell)
        })
        .unwrap();
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.save_session(true, cx)));
    let written = store.load();
    report("the session is written (tabs and results files)", written.as_ref().is_some_and(|s| s.tabs.len() == before.0.len() && s.tabs.iter().all(|t| t.results.is_some())), format!("{written:?}"));

    // Relaunch: the workspace rebuilt from the files, before any connection.
    let ws2 = relaunch(ws, cx);
    let back = wait(cx, |cx| {
        cx.update(|_, cx| ws2.read(cx).tabs.len() == before.0.len() && ws2.read(cx).tabs.iter().all(|t| t.read(cx).result().is_some())).unwrap_or(false)
    })
    .await;
    draw(cx);
    let after = cx
        .update(|_, cx| {
            let w = ws2.read(cx);
            let tabs: Vec<(String, (usize, usize))> = w
                .tabs
                .iter()
                .map(|t| {
                    let e = t.read(cx).editor.read(cx);
                    (e.text().to_string(), e.selection_ends())
                })
                .collect();
            (tabs, w.active)
        })
        .unwrap();
    report("the tabs come back with their SQL, selection and the tab in front", back && after.0 == before.0.iter().map(|t| (t.0.clone(), t.1)).collect::<Vec<_>>() && after.1 == before.1, format!("{after:?}"));
    let (ew, history) = cx.update(|_, cx| (ws2.read(cx).explorer_w, matches!(ws2.read(cx).side, Some(crate::app::Side::History)))).unwrap_or_default();
    report("the explorer's width and the open history come back", ew == 300. && history, format!("explorer {ew}, history {history}"));
    let (t1, t2) = cx.update(|_, cx| (ws2.read(cx).tabs[0].clone(), ws2.read(cx).tabs[1].clone())).unwrap();
    let link = cx.update(|_, cx| (t2.read(cx).favorite.as_ref().map(|(id, _)| id.clone()), t2.read(cx).title(cx).to_string())).unwrap();
    report("a tab linked to a favorite comes back linked, titled with its name", fav.is_some() && link.0 == fav && link.1 == "Session favorite", format!("{link:?}"));
    let (rows, cell, restored, partial, connected) = cx
        .update(|_, cx| {
            let t = t1.read(cx);
            let r = t.result();
            (
                r.map_or(0, |r| r.rows()),
                r.and_then(|r| r.snapshot().text(0, 0).map(|s| s.into_owned())),
                r.is_some_and(|r| r.is_restored()),
                r.is_some_and(|r| r.status().partial),
                t.connection().is_some(),
            )
        })
        .unwrap();
    let want_rows = before.2.min(diskman_core::session::ROW_CAP);
    report(
        "the first tab's rows are back (capped), without a connection",
        restored && !connected && rows == want_rows && cell == before.3 && (partial || before.2 <= want_rows),
        format!("{rows} rows (had {}), first {cell:?}, restored {restored}, partial {partial}, connected {connected}", before.2),
    );
    let (phase, error, tabs, shown_err) = cx
        .update(|_, cx| {
            let t = t2.read(cx);
            let err = t.batch_items().iter().find_map(|b| b.rs.as_ref().and_then(|r| r.status().error)).map(|e| e.message);
            let tabs = t.result_tabs().len();
            (t.run_phase(), err, tabs, t.result().is_some_and(|r| r.phase() == Phase::Failed))
        })
        .unwrap();
    report(
        "the script's error is back, on screen, with its result tabs",
        phase == Some(Phase::Failed) && error.as_deref().is_some_and(|m| m.contains("no_such_table_here")) && tabs >= 2 && shown_err && before.0[1].3 == Some(Phase::Failed),
        format!("phase {phase:?}, error {error:?}, {tabs} result tabs, error on screen {shown_err}"),
    );
    // A snapshot: grid edits refuse it.
    let edit = cx.update(|_, cx| t1.read(cx).edit.clone()).unwrap();
    let _ = cx.update(|window, cx| {
        ws2.update(cx, |w, cx| w.activate(0, window, cx));
        edit.update(cx, |e, cx| e.demo_edit(0, 1, window, cx));
    });
    draw(cx);
    let refused = cx.update(|_, cx| edit.read(cx).demo_read_only()).ok().flatten();
    let editing = cx.update(|_, cx| t1.read(cx).grid.read(cx).demo_editing()).unwrap_or(true);
    report("a restored result refuses grid edits", refused.is_some() && !editing, format!("{refused:?}"));

    // The first run connects the tab's own connection and runs there.
    run(&ws2, &t1, "SELECT 40 + 2 AS answer", false, cx).await;
    wait(cx, |cx| {
        cx.update(|_, cx| {
            let t = t1.read(cx);
            t.connection().is_some() && t.result().is_some_and(|r| !r.is_restored() && r.phase().is_finished())
        })
        .unwrap_or(true)
    })
    .await;
    draw(cx);
    let (conn, answer, live) = cx
        .update(|_, cx| {
            let t = t1.read(cx);
            (t.connection().map(|c| c.profile.id.clone()), t.result().and_then(|r| r.snapshot().text(0, 0).map(|s| s.into_owned())), t.result().is_some_and(|r| !r.is_restored()))
        })
        .unwrap();
    report("its first run connects its own connection and runs there", conn == before.0[0].2 && answer.as_deref() == Some("42") && live, format!("{conn:?} {answer:?}"));

    if stage == "session" {
        // The screenshot: a fresh relaunch, the script's error in front.
        let ws3 = relaunch(&ws2, cx);
        wait(cx, |cx| cx.update(|_, cx| ws3.read(cx).tabs.iter().all(|t| t.read(cx).result().is_some())).unwrap_or(false)).await;
        let _ = cx.update(|window, cx| ws3.update(cx, |w, cx| w.activate(1, window, cx)));
        draw(cx);
        let _ = cx.update(|_, cx| cx.set_global(Sessions(None)));
        let _ = std::fs::remove_dir_all(&dir);
        return ok;
    }

    // A damaged session file: a fresh workspace, no panic.
    let _ = cx.update(|_, cx| {
        ws2.update(cx, |w, _| {
            w.session_off = true;
            w.session_task = None;
        })
    });
    let _ = std::fs::write(dir.join(diskman_core::session::SESSION_FILE), "{ damaged");
    let ws3 = relaunch(&ws2, cx);
    let fresh = cx.update(|_, cx| ws3.read(cx).tabs.len() == 1 && ws3.read(cx).tabs[0].read(cx).editor.read(cx).text().is_empty()).unwrap_or(false);
    report("a damaged session file opens a fresh workspace", fresh, String::new());
    let _ = cx.update(|_, cx| cx.set_global(Sessions(None)));
    let _ = std::fs::remove_dir_all(&dir);
    ok
}
