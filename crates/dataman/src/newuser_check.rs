use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Focusable};
use dataman_core::config::Engine;
use dataman_core::result::Phase;

use crate::app::Workspace;
use crate::conn_dialog::ConnDialog;
use crate::conns::ConnState;

pub fn is_stage(s: &str) -> bool {
    s == "newuser-check"
}

fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn key(cx: &mut AsyncWindowContext, k: &str) {
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(crate::keymap::keystroke(k), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn typ(cx: &mut AsyncWindowContext, s: &str) {
    for c in s.chars() {
        let k = match c {
            ' ' => "space".to_string(),
            '.' => ".".to_string(),
            c => c.to_string(),
        };
        key(cx, &k);
    }
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn dialog(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Entity<ConnDialog>> {
    cx.update(|_, cx| ws.read(cx).dialog.clone()).ok().flatten()
}

fn status(d: &Entity<ConnDialog>, cx: &mut AsyncWindowContext) -> (String, bool) {
    cx.update(|_, cx| d.read(cx).demo_status(cx)).unwrap_or_default()
}

async fn tested(d: &Entity<ConnDialog>, secs: u64, cx: &mut AsyncWindowContext) -> (String, bool) {
    for _ in 0..secs * 20 {
        pause(cx, 50).await;
        let s = status(d, cx);
        if s.0 != "testing" {
            return s;
        }
    }
    status(d, cx)
}

async fn connected(ws: &Entity<Workspace>, id: &str, cx: &mut AsyncWindowContext) -> Result<(), String> {
    for _ in 0..400 {
        pause(cx, 50).await;
        match cx.update(|_, cx| ws.read(cx).connection_state(id, cx)).ok().flatten() {
            Some(ConnState::Connected(_)) => return Ok(()),
            Some(ConnState::Failed(e)) => return Err(e),
            _ => {}
        }
    }
    Err("timed out".into())
}

async fn finished(ws: &Entity<Workspace>, secs: u64, cx: &mut AsyncWindowContext) -> (Option<Phase>, usize, String) {
    let read = |cx: &mut AsyncWindowContext| {
        cx.update(|_, cx| {
            let t = ws.read(cx).active_tab()?.read(cx);
            let rs = t.result().cloned();
            Some((t.run_phase(), t.is_running(), rs.as_ref().map(|r| r.rows()).unwrap_or(0), rs.and_then(|r| r.status().error.map(|e| e.message)).unwrap_or_default()))
        })
        .ok()
        .flatten()
    };
    for _ in 0..secs * 20 {
        pause(cx, 50).await;
        if let Some((phase, running, rows, err)) = read(cx)
            && !running
            && phase.is_some_and(|p| p.is_finished())
        {
            return (phase, rows, err);
        }
    }
    read(cx).map(|(p, _, r, e)| (p, r, e)).unwrap_or((None, 0, String::new()))
}

fn snap(name: &str, cx: &mut AsyncWindowContext) {
    let Ok(dir) = std::env::var("DATAMAN_SHOT_DIR") else { return };
    let path = format!("{dir}/newuser-{name}.png");
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
    });
}

fn focus_grid(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        let grid = ws.read(cx).active_tab().unwrap().read(cx).grid.clone();
        grid.update(cx, |g, cx| g.select_cell(0, 0, cx));
        window.focus(&grid.focus_handle(cx), cx);
    });
    draw(cx);
}

fn editor_text(ws: &Entity<Workspace>, sql: &str, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        let tab = ws.read(cx).active_tab().unwrap().clone();
        let editor = tab.read(cx).editor.clone();
        editor.update(cx, |e, cx| e.set_text(sql, cx));
        window.focus(&gpui::Focusable::focus_handle(editor.read(cx), cx), cx);
    });
    draw(cx);
}

async fn add(ws: &Entity<Workspace>, engine: Engine, name: &str, port: u16, user: &str, pw: &str, db: &str, cx: &mut AsyncWindowContext) -> Option<String> {
    key(cx, "cmd-n");
    pause(cx, 100).await;
    let d = dialog(ws, cx)?;
    let _ = cx.update(|_, cx| {
        d.update(cx, |d, cx| {
            d.demo_engine(engine, cx);
            for (i, v) in [name, "127.0.0.1", &port.to_string(), user, pw, db].into_iter().enumerate() {
                d.demo_set_field(i, v, cx);
            }
        })
    });
    let id = cx.update(|_, cx| d.read(cx).profile_id().to_string()).ok()?;
    key(cx, "cmd-enter");
    pause(cx, 100).await;
    Some(id)
}

pub async fn run(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut check = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass || detail.is_empty() { String::new() } else { format!(": {detail}") });
        ok &= pass;
    };
    pause(cx, 300).await;
    let first_run = cx.update(|_, cx| ws.read(cx).first_run(cx)).unwrap_or(false);
    check("nothing saved: the welcome screen", first_run, String::new());
    snap("welcome", cx);

    key(cx, "cmd-n");
    pause(cx, 100).await;
    let Some(d) = dialog(ws, cx) else {
        check("⌘N opens the connection sheet", false, String::new());
        return false;
    };
    let focused = |cx: &mut AsyncWindowContext| cx.update(|window, cx| d.read(cx).demo_focused(window, cx)).ok().flatten();
    let field = |i: usize, cx: &mut AsyncWindowContext| cx.update(|_, cx| d.read(cx).demo_field(i, cx)).unwrap_or_default();
    let step = |cx: &mut AsyncWindowContext| cx.update(|_, cx| d.read(cx).step()).ok();
    check("⌘N opens the connection sheet on the engines", step(cx) == Some(crate::conn_dialog::Step::Engine), format!("{:?}", step(cx)));
    typ(cx, "postg");
    key(cx, "enter");
    pause(cx, 50).await;
    draw(cx);
    let picked = cx.update(|_, cx| d.read(cx).current(cx).0.engine).ok();
    check("typing a name and ↩ picks PostgreSQL: its form, on the name", step(cx) == Some(crate::conn_dialog::Step::Form) && picked == Some(Engine::Postgres) && focused(cx) == Some(0), format!("{:?} {picked:?} {:?}", step(cx), focused(cx)));

    typ(cx, "first");
    key(cx, "tab");
    let after_tab = focused(cx);
    typ(cx, "localhost");
    check(
        "Tab moves from the name to the host",
        after_tab == Some(1) && field(0, cx) == "first" && field(1, cx) == "localhost",
        format!("focus {after_tab:?} name {:?} host {:?}", field(0, cx), field(1, cx)),
    );
    let mut order = vec![];
    for _ in 0..5 {
        key(cx, "tab");
        order.push(focused(cx));
    }
    check("Tab goes host → port → user → database → password → name", order == [Some(2), Some(3), Some(5), Some(4), Some(0)], format!("{order:?}"));
    key(cx, "shift-tab");
    check("⇧Tab goes back", focused(cx) == Some(4), format!("{:?}", focused(cx)));

    let mut seen = vec![];
    for e in [Engine::MySql, Engine::SqlServer, Engine::Postgres] {
        let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_engine(e, cx)));
        seen.push((field(2, cx), field(3, cx)));
    }
    let want: Vec<(String, String)> = [("3306", "root"), ("1433", "sa"), ("5432", "postgres")].iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    check("each engine brings its default port and user", seen == want, format!("{seen:?}"));

    let _ = cx.update(|_, cx| {
        d.update(cx, |d, cx| {
            d.demo_set_field(1, "", cx);
            d.demo_save(cx);
        })
    });
    let s = status(&d, cx).0;
    check("no host: Save says so", s.contains("host"), s);
    let (pg, pg_pw) = crate::bench::bench_profile(None);
    let _ = cx.update(|_, cx| {
        d.update(cx, |d, cx| {
            d.demo_set_field(1, &pg.host, cx);
            d.demo_set_field(2, "", cx);
            d.demo_set_field(3, &pg.user, cx);
            d.demo_set_field(5, &pg.database, cx);
            d.demo_set_field(4, "wrong", cx);
        })
    });
    let port = cx.update(|_, cx| d.read(cx).current(cx).0.port).unwrap_or(0);
    let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_test(cx)));
    let s = status(&d, cx).0;
    check("an empty port is the engine's default", port == 5432 && !s.starts_with("invalid"), format!("port {port} {s}"));
    let _ = tested(&d, 10, cx).await;

    let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_set_field(2, &pg.port.to_string(), cx)));
    let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_test(cx)));
    let (s, red) = tested(&d, 10, cx).await;
    snap("wrong-password", cx);
    check("a wrong password fails the test, the field red", s.starts_with("failed") && red, format!("{s} red={red}"));
    let _ = cx.update(|window, cx| { let i = d.read(cx).demo_input(4); window.focus(&i.read(cx).focus_handle(cx), cx) });
    key(cx, "cmd-a");
    typ(cx, "x");
    let (s, red) = status(&d, cx);
    check("typing a new password clears the failure", s == "untested" && !red, format!("{s} red={red}"));

    let _ = cx.update(|_, cx| {
        d.update(cx, |d, cx| {
            d.demo_set_field(2, "9", cx);
            d.demo_test(cx);
        })
    });
    let t0 = std::time::Instant::now();
    let (s, _) = tested(&d, 15, cx).await;
    snap("closed-port", cx);
    check("a closed port fails the test", s.starts_with("failed"), format!("{s} after {:?}", t0.elapsed()));

    let _ = cx.update(|_, cx| {
        d.update(cx, |d, cx| {
            d.demo_set_field(2, &pg.port.to_string(), cx);
            d.demo_set_field(4, &pg_pw, cx);
        })
    });
    let id = cx.update(|_, cx| d.read(cx).profile_id().to_string()).unwrap_or_default();
    let _ = cx.update(|window, cx| { let i = d.read(cx).demo_input(0); window.focus(&i.read(cx).focus_handle(cx), cx) });
    key(cx, "cmd-enter");
    pause(cx, 100).await;
    let closed = dialog(ws, cx).is_none();
    let r = connected(ws, &id, cx).await;
    check("⌘↩ saves and connects", closed && r.is_ok(), format!("closed={closed} {r:?}"));
    draw(cx);
    let welcome = cx.update(|_, cx| ws.read(cx).first_run(cx)).unwrap_or(true);
    let tab_conn = cx.update(|_, cx| ws.read(cx).active_tab().and_then(|t| t.read(cx).connection().map(|c| c.profile.id.clone()))).ok().flatten();
    check("the welcome screen gives way, the tab on the new connection", !welcome && tab_conn.as_deref() == Some(id.as_str()), format!("{tab_conn:?}"));

    editor_text(ws, "", cx);
    key(cx, "cmd-enter");
    pause(cx, 200).await;
    let running = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.read(cx).is_running())).ok().flatten();
    check("an empty run does nothing", running == Some(false), format!("{running:?}"));
    editor_text(ws, "   \n-- just a comment\n  ;  ", cx);
    key(cx, "cmd-enter");
    pause(cx, 300).await;
    let st = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| (t.read(cx).is_running(), t.read(cx).run_phase()))).ok().flatten();
    check("a comment-only run does not hang", st.is_none_or(|(r, _)| !r), format!("{st:?}"));

    editor_text(ws, "SELECT 1 AS one WHERE false", cx);
    key(cx, "cmd-enter");
    let (phase, rows, err) = finished(ws, 10, cx).await;
    check("a query with no rows is done", phase == Some(Phase::Done) && rows == 0, format!("{phase:?} {rows} {err}"));

    editor_text(ws, "SELECT * FROM no_such_table_here", cx);
    key(cx, "cmd-enter");
    let (phase, _, err) = finished(ws, 10, cx).await;
    snap("error", cx);
    check("an error shows the server's message", phase == Some(Phase::Failed) && err.contains("no_such_table_here"), format!("{phase:?} {err}"));

    editor_text(ws, "SELECT pg_sleep(30)", cx);
    key(cx, "cmd-enter");
    pause(cx, 500).await;
    key(cx, "cmd-.");
    let t0 = std::time::Instant::now();
    let (phase, _, err) = finished(ws, 8, cx).await;
    check("⌘. cancels a long query", matches!(phase, Some(Phase::Cancelled | Phase::Failed)) && t0.elapsed() < Duration::from_secs(5), format!("{phase:?} {err} {:?}", t0.elapsed()));

    editor_text(ws, "SELECT FROM generate_series(1, 3)", cx);
    key(cx, "cmd-enter");
    let (phase, rows, err) = finished(ws, 10, cx).await;
    focus_grid(ws, cx);
    key(cx, "cmd-a");
    key(cx, "cmd-c");
    key(cx, "cmd-shift-c");
    draw(cx);
    snap("no-columns", cx);
    check("a result with no columns", phase == Some(Phase::Done) && rows == 3, format!("{phase:?} {rows} {err}"));

    editor_text(ws, "SELECT 'café 😀👍🏽 日本' AS \"名前 🙂\", NULL AS n, '' AS e", cx);
    key(cx, "cmd-enter");
    let (phase, _, err) = finished(ws, 10, cx).await;
    focus_grid(ws, cx);
    key(cx, "cmd-a");
    key(cx, "cmd-shift-c");
    let clip = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text())).ok().flatten().unwrap_or_default();
    snap("unicode", cx);
    check("unicode names and values copy as they are, NULL apart from ''", phase == Some(Phase::Done) && clip.trim_end_matches('\n') == "名前 🙂\tn\te\ncafé 😀👍🏽 日本\t\\N\t", format!("{phase:?} {err} {clip:?}"));

    let wide = format!("SELECT {}", (1..=600).map(|i| format!("{i} AS column_number_{i}")).collect::<Vec<_>>().join(", "));
    editor_text(ws, &wide, cx);
    key(cx, "cmd-enter");
    let (phase, rows, err) = finished(ws, 10, cx).await;
    draw(cx);
    let cols = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.read(cx).grid.read(cx).demo_widths().0.len())).ok().flatten();
    snap("wide", cx);
    check("600 columns", phase == Some(Phase::Done) && rows == 1 && cols == Some(600), format!("{phase:?} {rows} {cols:?} {err}"));

    let long = format!("SELECT length('{}') AS n", "x".repeat(300_000));
    editor_text(ws, &long, cx);
    key(cx, "cmd-enter");
    let (phase, rows, err) = finished(ws, 15, cx).await;
    check("a 300 KB statement runs", phase == Some(Phase::Done) && rows == 1, format!("{phase:?} {rows} {err}"));

    editor_text(ws, "SELECT g, md5(g::text) FROM generate_series(1, 100000) g", cx);
    key(cx, "cmd-enter");
    let _ = finished(ws, 15, cx).await;
    focus_grid(ws, cx);
    key(cx, "cmd-shift-down");
    let mut rows = 0;
    for _ in 0..300 {
        pause(cx, 50).await;
        rows = cx.update(|_, cx| ws.read(cx).active_tab().and_then(|t| t.read(cx).result().map(|r| r.rows()))).ok().flatten().unwrap_or(0);
        if rows == 100_000 {
            break;
        }
    }
    draw(cx);
    check("100k rows, fetched all", rows == 100_000, format!("{rows}"));

    let _ = cx.update(|window, _| window.resize(gpui::size(gpui::px(420.), gpui::px(300.))));
    pause(cx, 200).await;
    draw(cx);
    snap("tiny", cx);
    let _ = cx.update(|window, _| window.resize(gpui::size(gpui::px(1180.), gpui::px(760.))));
    pause(cx, 200).await;
    draw(cx);
    check("a tiny window draws", true, String::new());

    editor_text(ws, "SELECT pg_terminate_backend(pg_backend_pid())", cx);
    key(cx, "cmd-enter");
    let (phase, _, err) = finished(ws, 10, cx).await;
    println!("  (terminated: {phase:?} {err})");
    editor_text(ws, "SELECT 42 AS answer", cx);
    key(cx, "cmd-enter");
    let (phase, rows, err) = finished(ws, 10, cx).await;
    check("after the session is killed, the next run reconnects", phase == Some(Phase::Done) && rows == 1, format!("{phase:?} {rows} {err}"));

    editor_text(ws, "SELECT pg_sleep(2)", cx);
    key(cx, "cmd-enter");
    pause(cx, 300).await;
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dialog(Some(id.clone()), window, cx)));
    if let Some(d) = dialog(ws, cx) {
        let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_set_field(0, "renamed", cx)));
        key(cx, "cmd-enter");
    }
    pause(cx, 100).await;
    let r = connected(ws, &id, cx).await;
    let (phase, _, err) = finished(ws, 10, cx).await;
    println!("  (the run under the edit: {phase:?} {err})");
    editor_text(ws, "SELECT 2 AS two", cx);
    key(cx, "cmd-enter");
    let (phase, rows, err) = finished(ws, 10, cx).await;
    let name = cx.update(|_, cx| ws.read(cx).store.read(cx).profile(&id).map(|p| p.name.clone())).ok().flatten();
    check("an edit saved mid-query: renamed, reconnected, the tab runs again", r.is_ok() && name.as_deref() == Some("renamed") && phase == Some(Phase::Done) && rows == 1, format!("{r:?} {name:?} {phase:?} {rows} {err}"));

    editor_text(ws, "CREATE TABLE IF NOT EXISTS public.newuser_t (id int PRIMARY KEY, name text);\nINSERT INTO public.newuser_t VALUES (1, 'a'), (2, 'b') ON CONFLICT (id) DO UPDATE SET name = excluded.name;\nSELECT count(*) FROM public.newuser_t;", cx);
    key(cx, "cmd-enter");
    let (phase, rows, err) = finished(ws, 10, cx).await;
    let items = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.read(cx).batch_items().len())).ok().flatten();
    check("a three-statement script runs", phase == Some(Phase::Done) && rows == 1 && items == Some(3), format!("{phase:?} {rows} {items:?} {err}"));
    editor_text(ws, "SELECT * FROM public.newuser_t ORDER BY id", cx);
    key(cx, "cmd-enter");
    let _ = finished(ws, 10, cx).await;
    let tab = cx.update(|_, cx| ws.read(cx).active_tab().unwrap().clone()).unwrap();
    let edit = cx.update(|_, cx| tab.read(cx).edit.clone()).unwrap();
    focus_grid(ws, cx);
    let _ = cx.update(|window, cx| edit.update(cx, |e, cx| e.demo_edit(0, 1, window, cx)));
    pause(cx, 800).await;
    draw(cx);
    key(cx, "cmd-a");
    typ(cx, "zz");
    key(cx, "enter");
    draw(cx);
    let pending = cx.update(|_, cx| edit.read(cx).pending_count()).unwrap_or(0);
    let n = cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap_or(0);
    key(cx, "cmd-w");
    pause(cx, 100).await;
    let still = cx.update(|_, cx| ws.read(cx).tabs.contains(&tab)).unwrap_or(false);
    let pending2 = cx.update(|_, cx| edit.read(cx).pending_count()).unwrap_or(0);
    check("⌘W on a tab with a pending edit asks first", pending == 1 && still && pending2 == 1, format!("pending {pending} → {pending2}, tab kept {still} ({n} tabs)"));
    key(cx, "escape");
    pause(cx, 100).await;
    let kept = cx.update(|_, cx| ws.read(cx).tabs.contains(&tab) && edit.read(cx).pending_count() == 1).unwrap_or(false);
    let quits = cx.update(|window, cx| ws.update(cx, |w, cx| w.request_quit(window, cx))).unwrap_or(true);
    let asked = cx.update(|_, cx| tab.read(cx).pending_run.is_some()).unwrap_or(false);
    key(cx, "escape");
    check("a quit with a pending edit asks first", !quits && asked, format!("quits {quits} asked {asked}"));
    key(cx, "cmd-w");
    pause(cx, 100).await;
    key(cx, "cmd-backspace");
    pause(cx, 200).await;
    let gone = cx.update(|_, cx| !ws.read(cx).tabs.contains(&tab)).unwrap_or(false);
    check("esc keeps the edits; Discard & close (⌘⌫) closes", kept && gone, format!("kept {kept} gone {gone}"));

    let n = cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap_or(0);
    for _ in 0..n {
        key(cx, "cmd-w");
        pause(cx, 50).await;
    }
    draw(cx);
    let (n, has_active) = cx.update(|_, cx| (ws.read(cx).tabs.len(), ws.read(cx).active_tab().is_some())).unwrap_or_default();
    check("⌘W on the last tab leaves the window usable", n == 0 || has_active, format!("tabs {n}"));
    key(cx, "cmd-t");
    pause(cx, 100).await;
    let conn = cx.update(|_, cx| ws.read(cx).active_tab().and_then(|t| t.read(cx).connection().map(|c| c.profile.id.clone()))).ok().flatten();
    check("⌘T then opens a tab on the connection", conn.as_deref() == Some(id.as_str()), format!("{conn:?}"));

    let mut my_ids = vec![];
    for (engine, port, user, pw, db) in [
        (Engine::MySql, 33306u16, "root", std::env::var("DATAMAN_MYSQL_PASSWORD").unwrap_or_else(|_| "dataman-root".into()), "dataman_test"),
        (Engine::SqlServer, 14330, "sa", std::env::var("DATAMAN_MSSQL_PASSWORD").unwrap_or_else(|_| "Dataman-sa-1!".into()), ""),
    ] {
        let name = format!("{engine:?}");
        let Some(id) = add(ws, engine, &name, port, user, &pw, db, cx).await else {
            check(&format!("{name}: the sheet opens"), false, String::new());
            continue;
        };
        let left = dialog(ws, cx).map(|d| status(&d, cx).0);
        let r = connected(ws, &id, cx).await;
        check(&format!("{name}: saved through the sheet, connects"), left.is_none() && r.is_ok(), format!("{left:?} {r:?}"));
        let mut row = None;
        for _ in 0..100 {
            pause(cx, 50).await;
            draw(cx);
            row = cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_conn_row(&id)).ok().flatten();
            if row.is_some() {
                break;
            }
        }
        check(&format!("{name}: in the explorer"), row.is_some(), String::new());
        my_ids.push(id);
    }

    if let Some(other) = my_ids.first().cloned() {
        editor_text(ws, "SELECT pg_sleep(20)", cx);
        key(cx, "cmd-enter");
        pause(cx, 500).await;
        let _ = cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                let tab = w.active_tab().unwrap().clone();
                if let Some(c) = w.store.read(cx).connection(&other) {
                    w.retarget(&tab, c, None, None, window, cx);
                }
            })
        });
        let t0 = std::time::Instant::now();
        let (phase, _, err) = finished(ws, 5, cx).await;
        let on = cx.update(|_, cx| ws.read(cx).active_tab().and_then(|t| t.read(cx).connection().map(|c| c.profile.id.clone()))).ok().flatten();
        let running = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.read(cx).is_running())).ok().flatten();
        check("switching connections mid-query stops the run", on.as_deref() == Some(other.as_str()) && running == Some(false) && t0.elapsed() < Duration::from_secs(4), format!("{on:?} {phase:?} {err} running={running:?} {:?}", t0.elapsed()));
        editor_text(ws, "SELECT 1 AS one", cx);
        key(cx, "cmd-enter");
        let (phase, rows, err) = finished(ws, 10, cx).await;
        check("then runs on the new connection", phase == Some(Phase::Done) && rows == 1, format!("{phase:?} {rows} {err}"));
    }

    let _ = cx.update(|window, cx| {
        ws.update(cx, |w, cx| {
            let target = crate::tree_menu::Target::Conn { id: id.clone(), name: String::new(), state: crate::tree_menu::ConnPhase::Connected };
            w.on_row_action(crate::tree_menu::Act::DuplicateConnection, target, false, window, cx);
        })
    });
    pause(cx, 100).await;
    match dialog(ws, cx) {
        Some(d) => {
            let (dup, (p, pw)) = cx.update(|_, cx| (d.read(cx).profile_id().to_string(), d.read(cx).current(cx))).unwrap();
            key(cx, "cmd-enter");
            let r = connected(ws, &dup, cx).await;
            check("Duplicate: prefilled, saved, connects", dup != id && p.name == "copy of renamed" && !pw.is_empty() && r.is_ok(), format!("{} {} {r:?}", p.name, pw.len()));
        }
        None => check("Duplicate opens the sheet", false, String::new()),
    }

    let ids: Vec<String> = cx.update(|_, cx| ws.read(cx).store.read(cx).profiles.iter().map(|p| p.id.clone()).collect()).unwrap_or_default();
    for i in &ids {
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.request_delete_connection(i.clone(), true, window, cx)));
        pause(cx, 50).await;
    }
    draw(cx);
    let (welcome, tabs_on) = cx.update(|_, cx| (ws.read(cx).first_run(cx), ws.read(cx).tabs.iter().filter(|t| t.read(cx).connection().is_some()).count())).unwrap_or_default();
    check("all connections deleted: the welcome screen, no tab left on one", welcome && tabs_on == 0, format!("welcome {welcome} tabs on a connection {tabs_on}"));
    let _ = cx.update(|window, cx| {
        let tree = ws.read(cx).tree.clone();
        window.focus(&tree.focus_handle(cx), cx);
    });
    key(cx, "cmd-z");
    pause(cx, 100).await;
    draw(cx);
    let back = cx.update(|_, cx| ws.read(cx).store.read(cx).profiles.len()).unwrap_or(0);
    check("⌘Z restores the last deleted connection", back == 1, format!("{back}"));
    ok
}
