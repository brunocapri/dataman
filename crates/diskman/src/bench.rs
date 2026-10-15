//! Built-in benchmarks: they drive the real UI frame by frame and print measurements.
//!
//! - `--bench-scroll`: 1 M rows in the grid; steady fling and random jumps, while fetching and
//!   after everything is loaded.
//! - `--bench-tree`: a schema with 1,000 tables (database `diskman_bench`, schema `bench`).
//! - `--bench-typing`: types a query one key per frame, with autocomplete on a real schema.
//!
//! Connection: `DISKMAN_BENCH_PG=host:port:user:password:database`
//! (default `127.0.0.1:55432:postgres:diskman:diskman`).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{App, Entity, Focusable, Window};
use diskman_core::result::Phase;
use diskman_core::{ConnectionProfile, SslMode};

use crate::app::Workspace;
use crate::conns::ConnState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Startup,
    Scroll,
    Tree,
    Typing,
    /// Not a benchmark: connects, opens the tree and runs a query, then stays open
    /// (screenshots).
    Demo,
}

impl Mode {
    pub fn from_args(args: &[String]) -> Option<Mode> {
        args.iter().find_map(|a| match a.as_str() {
            "--bench-startup" => Some(Mode::Startup),
            "--bench-scroll" => Some(Mode::Scroll),
            "--bench-tree" => Some(Mode::Tree),
            "--bench-typing" => Some(Mode::Typing),
            "--demo" => Some(Mode::Demo),
            _ => None,
        })
    }
}

pub fn bench_profile(database_override: Option<&str>) -> (ConnectionProfile, String) {
    let spec = std::env::var("DISKMAN_BENCH_PG").unwrap_or_else(|_| "127.0.0.1:55432:postgres:diskman:diskman".into());
    let p: Vec<&str> = spec.split(':').collect();
    let mut prof = ConnectionProfile::new("bench");
    prof.host = p[0].into();
    prof.port = p[1].parse().unwrap();
    prof.user = p[2].into();
    prof.database = database_override.unwrap_or(p[4]).into();
    prof.sslmode = SslMode::Disable;
    prof.save_password = true;
    (prof, p[3].into())
}

/// The MySQL server of the `mysql*` stages: `DISKMAN_BENCH_MYSQL` (`host:port:user:db`, by
/// default the `diskman-mysql` test container) and its password in `DISKMAN_MYSQL_PASSWORD`.
pub fn mysql_profile() -> (ConnectionProfile, String) {
    let spec = std::env::var("DISKMAN_BENCH_MYSQL").unwrap_or_else(|_| "127.0.0.1:33306:root:diskman_test".into());
    let p: Vec<&str> = spec.split(':').collect();
    let mut prof = ConnectionProfile::new("mysql-local");
    prof.engine = diskman_core::config::Engine::MySql;
    prof.host = p[0].into();
    prof.port = p[1].parse().unwrap();
    prof.user = p[2].into();
    prof.database = p[3].into();
    prof.sslmode = SslMode::Disable;
    prof.save_password = true;
    (prof, std::env::var("DISKMAN_MYSQL_PASSWORD").unwrap_or_default())
}

/// The SQL Server of the `mssql*` stages: `DISKMAN_BENCH_MSSQL` (`host:port:user:db`, by
/// default the `diskman-mssql` test container) and its password in `DISKMAN_MSSQL_PASSWORD`.
pub fn mssql_profile() -> (ConnectionProfile, String) {
    let spec = std::env::var("DISKMAN_BENCH_MSSQL").unwrap_or_else(|_| "127.0.0.1:14330:sa:diskman_test".into());
    let p: Vec<&str> = spec.split(':').collect();
    let mut prof = ConnectionProfile::new("mssql-local");
    prof.name = "diskman_test@mssql".into();
    prof.engine = diskman_core::config::Engine::SqlServer;
    prof.host = p[0].into();
    prof.port = p[1].parse().unwrap();
    prof.user = p[2].into();
    prof.database = p[3].into();
    // The container's certificate is its own: encrypted, not verified.
    prof.sslmode = SslMode::Require;
    prof.save_password = true;
    (prof, std::env::var("DISKMAN_MSSQL_PASSWORD").unwrap_or_default())
}

pub struct Stats(Vec<Duration>);

impl Stats {
    fn ms(&self, p: f64) -> f64 {
        let mut v = self.0.clone();
        v.sort();
        if v.is_empty() {
            return 0.;
        }
        v[((v.len() - 1) as f64 * p).round() as usize].as_secs_f64() * 1e3
    }

    fn over(&self, ms: f64) -> usize {
        self.0.iter().filter(|d| d.as_secs_f64() * 1e3 > ms).count()
    }

    pub fn line(&self, name: &str) -> String {
        format!(
            "{name}: n={} p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms >16.7ms={} >33ms={}",
            self.0.len(),
            self.ms(0.5),
            self.ms(0.95),
            self.ms(0.99),
            self.ms(1.0),
            self.over(16.7),
            self.over(33.4)
        )
    }
}

/// Calls `step` once per frame (after each frame is presented) until it returns false.
fn every_frame(window: &mut Window, step: impl FnMut(&mut Window, &mut App) -> bool + 'static) {
    let step = Rc::new(RefCell::new(step));
    fn go(step: Rc<RefCell<dyn FnMut(&mut Window, &mut App) -> bool>>, window: &mut Window) {
        window.on_next_frame(move |window, cx| {
            if (step.borrow_mut())(window, cx) {
                window.refresh();
                go(step, window);
            }
        });
    }
    go(step, window);
}

/// Waits (polling each frame) until `ready` holds, then runs `then`.
fn when(window: &mut Window, timeout: Duration, mut ready: impl FnMut(&mut App) -> bool + 'static, then: impl FnOnce(&mut Window, &mut App) + 'static) {
    let start = Instant::now();
    let mut then = Some(then);
    let mut polls = 0u64;
    every_frame(window, move |window, cx| {
        polls += 1;
        if std::env::var("DISKMAN_DEBUG").is_ok() && polls % 60 == 0 {
            eprintln!("bench: still waiting after {polls} frames");
        }
        if ready(cx) {
            if let Some(f) = then.take() {
                f(window, cx);
            }
            return false;
        }
        if start.elapsed() > timeout {
            eprintln!("bench: timed out waiting");
            cx.quit();
            return false;
        }
        true
    });
}

pub(crate) fn connect(ws: &Entity<Workspace>, prof: ConnectionProfile, password: String, cx: &mut App) {
    let core = ws.read(cx).store.read(cx).core.clone();
    core.save_connection(prof.clone(), Some(&password), None).unwrap();
    let store = ws.read(cx).store.clone();
    store.update(cx, |s, cx| {
        s.reload(cx);
        s.connect(&prof.id, Some(password), cx);
    });
}

/// The import sheet's demo list: two services, two pgpass lines, one already saved.
fn demo_importable() -> Vec<diskman_core::Importable> {
    let mk = |name: &str, host: &str, port: u16, user: &str, db: &str, source, exists| {
        let mut p = diskman_core::ConnectionProfile::new(name);
        (p.host, p.port, p.user, p.database) = (host.into(), port, user.into(), db.into());
        diskman_core::Importable { source, profile: p, password: None, exists }
    };
    use diskman_core::ImportSource::{Pgpass, Service};
    vec![
        mk("local", "127.0.0.1", 55432, "postgres", "diskman", Service, true),
        mk("prod", "db.prod.internal", 6543, "app", "app", Service, false),
        mk("analytics", "warehouse.internal", 5432, "bruno", "events", Service, false),
        mk("app@staging.internal", "staging.internal", 5432, "app", "app", Pgpass, false),
        mk("bruno@localhost", "localhost", 5432, "bruno", "postgres", Pgpass, false),
    ]
}

fn connected(ws: &Entity<Workspace>, id: &str, cx: &App) -> bool {
    match ws.read(cx).connection_state(id, cx) {
        Some(ConnState::Connected(_)) => true,
        Some(ConnState::Failed(e)) => panic!("bench: connection failed: {e}"),
        _ => false,
    }
}

pub fn start(mode: Mode, ws: Entity<Workspace>, window: &mut Window, cx: &mut App) {
    // The throwaway config starts empty, which opens the first-run dialog: not wanted here.
    ws.update(cx, |w, cx| w.close_dialog(cx));
    match mode {
        Mode::Startup => {}
        Mode::Scroll => scroll(ws, window, cx),
        Mode::Tree => tree(ws, window, cx),
        Mode::Typing => typing(ws, window, cx),
        Mode::Demo => demo(ws, window, cx),
    }
}

/// Driven by a timer, not by frames: it works with the window hidden, and draws explicitly
/// before a capture.
///
/// `DISKMAN_SHOT_STAGE` picks the screen: `welcome` (first run, nothing connected), `main`,
/// `light` (main in the light theme), `completion`, `dialog`, `dialog-url` (and
/// `dialog-url-bad`: the connection string mode), `dialog-fail` (a real
/// password failure), `sort`, `history` (with the explorer filter), `error` (a batch that
/// stops at statement 2), `cancelling`, `viewer`, `empty` (connected, nothing run),
/// `shortcuts`, `focus` (the grid with the keyboard focus ring), `dialog-password`,
/// `error-pop` and `tree-pop` (the find indicator; `DISKMAN_POP_FREEZE_MS=n` holds every pop
/// n ms in, for a capture mid-flight; `fk-pop` and `fk-land` are in `fk_check`; the find bars'
/// `find`, `find-grid` and `find-check` in `find_check`; a script's result tabs `results`,
/// `results-grid`, `results-messages` and `results-check` in `results_check`; a favorite's
/// context menu, inline rename and the merged status dropdown's `history-check` in
/// `history_check`). Any
/// stage + `-light` renders in the light theme; `DISKMAN_SHOT_NO_EXPLORER=1` collapses the
/// explorer (⌘B).
fn demo(ws: Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let raw = std::env::var("DISKMAN_SHOT_STAGE").unwrap_or_else(|_| "main".into());
    let shot_path = std::env::var("DISKMAN_SHOT").ok();
    // `<stage>-light` is any stage in the light theme.
    if raw == "light" || raw.ends_with("-light") {
        crate::theme::set_pref(diskman_core::ThemePref::Light, cx);
    }
    if std::env::var_os("DISKMAN_SHOT_NO_EXPLORER").is_some() {
        ws.update(cx, |w, cx| {
            w.explorer_open = false;
            cx.notify();
        });
    }
    let stage = raw.trim_end_matches("-light").to_string();
    if stage.starts_with("welcome") {
        window
            .spawn(cx, async move |cx| {
                cx.background_executor().timer(Duration::from_millis(1500)).await;
                if let Some(path) = shot_path {
                    let _ = cx.update(|window, cx| {
                        window.refresh();
                        window.draw(cx).clear(cx);
                        shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
                        cx.quit();
                    });
                }
            })
            .detach();
        return;
    }
    let mysql = crate::mysql_check::is_stage(&stage);
    let mssql = crate::mssql_check::is_stage(&stage);
    let (mut prof, pw) = if mysql {
        mysql_profile()
    } else if mssql {
        mssql_profile()
    } else {
        bench_profile(None)
    };
    crate::tx_check::prepare(&stage, &mut prof);
    let id = prof.id.clone();
    let db = prof.database.clone();
    let bad_pw = stage == "dialog-fail";
    let password = pw.clone();
    connect(&ws, prof, pw, cx);
    // Live query stages bring their own SQL (`lar::demo`).
    let default_sql = crate::lar::demo::sql(&stage).unwrap_or_else(|| match stage.as_str() {
        "error" | "error-pop" => "-- shipping backfill (3 statements)\nSELECT count(*) FROM orders;\n\nUPDATE orders p\n   SET status = s.delivered_at\n  FROM orders s\n WHERE s.id = p.id;\n\nSELECT count(*) FROM customer;".to_string(),
        "edit" | "edit-review" | "edit-commit" | "edit-conflict" => "SELECT * FROM diskman_scratch.products ORDER BY id;".to_string(),
        "export-done" => "SELECT * FROM diskman_scratch.products ORDER BY id;".to_string(),
        // Taller and wider than the editor: both scrollbars.
        "editor-scroll" => {
            let mut s = String::from("-- monthly report, every region\n");
            for i in 1..=80 {
                s.push_str(&format!("SELECT {i} AS region, count(*) AS orders, sum(p.total) AS total, avg(p.total) AS average, min(p.id) AS first_id, max(p.id) AS last_id FROM orders p WHERE p.customer % 80 = {i};\n"));
            }
            s
        }
        s if s.starts_with("fk-") => crate::fk_check::SQL.to_string(),
        s if s.starts_with("results") => crate::results_check::SQL.to_string(),
        "edit-readonly" => "SELECT p.id, p.status, c.name FROM orders p JOIN customer c ON c.id = p.customer LIMIT 50;".to_string(),
        "cancelling" => "-- full scan, then a long wait\nSELECT p.customer, count(*), sum(p.total)\nFROM   orders p\nGROUP  BY p.customer\nORDER  BY sum(p.total) DESC;".to_string(),
        _ => "-- orders with their customer, newest first\nSELECT p.id, p.status, p.total, p.address, c.name, NULL AS nothing, '' AS empty\nFROM   orders p\nJOIN   customer c ON c.id = p.customer\nORDER  BY p.id;".to_string(),
    });
    let sql = std::env::var("DISKMAN_DEMO_SQL").unwrap_or(default_sql);
    window
        .spawn(cx, async move |cx| {
            let sleep = |cx: &mut gpui::AsyncWindowContext, ms| cx.background_executor().timer(Duration::from_millis(ms));
            for _ in 0..600 {
                sleep(cx, 50).await;
                if cx.update(|_, cx| connected(&ws, &id, cx)).unwrap_or(false) {
                    break;
                }
            }
            // A table's data view opens its own tab (`table_check`).
            let run = !matches!(
                stage.as_str(),
                "export-empty"
                    | "empty"
                    | "dialog"
                    | "dialog-url"
                    | "dialog-url-bad"
                    | "dialog-fail"
                    | "dialog-password"
                    | "dialog-test"
                    | "dialog-ssh"
                    | "dialog-hostkey"
                    | "import"
                    | "shortcuts"
                    | "editor-scroll"
            ) && !crate::table_check::is_stage(&stage)
                && !mysql
                && !mssql
                && !crate::tx_check::is_stage(&stage)
                && !crate::intel_demo::is_stage(&stage)
                && !crate::plan_demo::is_stage(&stage)
                && !crate::activity_demo::is_stage(&stage)
                && !crate::mcp_check::is_stage(&stage);
            let _ = cx.update(|_, cx| {
                let tab = ws.read(cx).active_tab().unwrap().clone();
                let editor = tab.read(cx).editor.clone();
                editor.update(cx, |e, cx| e.set_text(&sql, cx));
                if run {
                    if stage == "error" || stage == "error-pop" || stage.starts_with("results") {
                        editor.update(cx, |e, cx| e.run_script(cx));
                    } else {
                        let stmt = sql.trim_end_matches(';').to_string();
                        tab.update(cx, |t, cx| t.run(vec![stmt], cx));
                    }
                }
            });
            let c = format!("c:{id}");
            let s = format!("{c}/d:{db}/s:public");
            // The explorer's object stages open their own schema (`explorer_demo`).
            let nodes = if crate::explorer_demo::is_stage(&stage) || mysql || mssql { vec![] } else { vec![s.clone(), format!("{s}/tables"), format!("{s}/r:orders")] };
            for node in nodes {
                sleep(cx, 400).await;
                let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
                let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&node, cx)));
            }
            sleep(cx, 600).await;
            let _ = cx.update(|window, cx| {
                let tab = ws.read(cx).active_tab().unwrap().clone();
                let editor = tab.read(cx).editor.clone();
                let grid = tab.read(cx).grid.clone();
                match stage.as_str() {
                    "completion" => {
                        editor.update(cx, |e, cx| {
                            let end = e.text().len();
                            e.set_cursor(end, cx);
                            e.type_text("\n\nselect * from orders p where p.st", window, cx);
                        });
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                    "dialog" => ws.update(cx, |w, cx| w.open_dialog(None, window, cx)),
                    // A new connection by its connection string; `-bad`: one that is not, left.
                    "dialog-url" | "dialog-url-bad" => ws.update(cx, |w, cx| {
                        w.open_dialog(None, window, cx);
                        if let Some(d) = w.dialog.clone() {
                            d.update(cx, |d, cx| {
                                d.set_entry(crate::conn_dialog::Entry::Url, window, cx);
                                if stage == "dialog-url" {
                                    d.set_conn_str("Server=tcp:db.example.com,14330;Database=shop;User Id=app;Encrypt=True;TrustServerCertificate=True", cx);
                                } else {
                                    d.set_conn_str("postgres//app@db.example.com/shop", cx);
                                    d.focus_name(window, cx);
                                }
                            });
                        }
                    }),
                    "dialog-test" => ws.update(cx, |w, cx| {
                        // A new connection prefilled with the bench server's fields, tested.
                        let (mut p, pw) = bench_profile(None);
                        p.name = "local".into();
                        w.open_dialog_with(Some(p), true, Some(pw), window, cx);
                        if let Some(d) = w.dialog.clone() {
                            d.update(cx, |d, cx| d.run_test(cx));
                        }
                    }),
                    "dialog-password" => ws.update(cx, |w, cx| {
                        w.open_dialog(Some(id.clone()), window, cx);
                        if let Some(d) = w.dialog.clone() {
                            d.update(cx, |d, cx| d.set_password_needed(cx));
                            let f = d.read(cx).password_focus(cx);
                            window.focus(&f, cx);
                        }
                    }),
                    "dialog-fail" => ws.update(cx, |w, cx| {
                        w.open_dialog(Some(id.clone()), window, cx);
                        if let Some(d) = w.dialog.clone() {
                            d.update(cx, |d, cx| {
                                d.set_password(if bad_pw { "not-the-password" } else { "" }, cx);
                                d.run_test(cx);
                            });
                        }
                    }),
                    // SSH: the section open (key auth), an unknown host key to trust, the
                    // import sheet, and the explorer's tunnel glyph.
                    "dialog-ssh" | "dialog-hostkey" => ws.update(cx, |w, cx| {
                        let (mut p, _) = bench_profile(None);
                        (p.name, p.host, p.port, p.database) = ("prod".into(), "db.internal".into(), 5432, "app".into());
                        p.ssh = Some(diskman_core::SshSettings {
                            host: "bastion".into(),
                            port: None,
                            user: "deploy".into(),
                            auth: diskman_core::SshAuth::Key,
                            key_file: "~/.ssh/id_ed25519".into(),
                        });
                        w.open_dialog_with(Some(p), true, None, window, cx);
                        if stage == "dialog-hostkey"
                            && let Some(d) = w.dialog.clone()
                            && let Some(k) = diskman_core::HostKeyPrompt::from_openssh(
                                "bastion",
                                22,
                                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ",
                            )
                        {
                            d.update(cx, |d, cx| d.set_host_key(k, cx));
                        }
                    }),
                    "import" => ws.update(cx, |w, cx| {
                        w.open_import(window, cx);
                        if let Some(v) = w.import.clone() {
                            v.update(cx, |v, cx| {
                                v.set_items(demo_importable());
                                v.pick(1, cx);
                                v.pick(2, cx);
                            });
                        }
                    }),
                    "tree-ssh" => ws.update(cx, |w, cx| {
                        let mut p = diskman_core::ConnectionProfile::new("prod via bastion");
                        (p.host, p.database, p.color) = ("db.internal".into(), "app".into(), Some("violet".into()));
                        p.ssh = Some(diskman_core::SshSettings { host: "bastion".into(), ..Default::default() });
                        let core = w.store.read(cx).core.clone();
                        core.save_connection(p, None, None).unwrap();
                        w.store.update(cx, |s, cx| s.reload(cx));
                    }),
                    // Total, descending: two clicks on the header.
                    "sort" => grid.update(cx, |g, cx| g.toggle_sort(2, cx)),
                    "history" => {
                        ws.update(cx, |w, cx| w.toggle_history_for_demo(cx));
                        let tree = ws.read(cx).tree.clone();
                        tree.update(cx, |t, cx| t.set_filter("ord", cx));
                    }
                    // The find indicator (hold it mid-pop with DISKMAN_POP_FREEZE_MS): ⌘' on the
                    // error, the explorer filter's first match.
                    "error-pop" => {
                        editor.update(cx, |e, cx| e.jump_to_error(cx));
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                    "tree-pop" => {
                        let tree = ws.read(cx).tree.clone();
                        tree.update(cx, |t, cx| {
                            t.set_filter("ord", cx);
                            t.pop_first_match(cx);
                        });
                    }
                    "cancelling" => tab.update(cx, |t, cx| t.demo_pretend_cancel(cx)),
                    "viewer" => {
                        grid.update(cx, |g, cx| g.select_cells(4..=4, 3, cx));
                        window.focus(&grid.focus_handle(cx), cx);
                        ws.update(cx, |w, cx| w.toggle_side(crate::app::Side::Viewer, cx));
                    }
                    "focus" => ws.update(cx, |w, cx| w.focus_region(crate::app::Region::Grid, window, cx)),
                    // A selection over three lines, mid-line to mid-line.
                    "selection" => {
                        editor.update(cx, |e, cx| e.demo_select(30, 150, cx));
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                    "shortcuts" => ws.update(cx, |w, cx| {
                        w.shortcuts_open = true;
                        cx.notify();
                    }),
                    "palette" | "palette-nomatch" | "goto" => ws.update(cx, |w, cx| {
                        let scope = if stage == "goto" { crate::palette::Scope::Goto } else { crate::palette::Scope::All };
                        w.open_palette(scope, window, cx);
                        let q = match stage.as_str() {
                            "palette-nomatch" => "xyzzy",
                            "goto" => "",
                            _ => "ord",
                        };
                        if let Some(p) = w.palette.clone() {
                            p.update(cx, |p, cx| p.set_query(q, cx));
                        }
                    }),
                    // Distribution: an update ready (the dot, the palette), last session's crash.
                    "update" | "update-palette" => {
                        crate::updates::demo_ready(cx);
                        if stage == "update-palette" {
                            ws.update(cx, |w, cx| {
                                w.open_palette(crate::palette::Scope::All, window, cx);
                                if let Some(p) = w.palette.clone() {
                                    p.update(cx, |p, cx| p.set_query("update", cx));
                                }
                            });
                        }
                    }
                    "crash" => ws.update(cx, |w, cx| {
                        let dir = diskman_core::crash::dir();
                        w.crash = Some(diskman_core::crash::Pending {
                            report: Some(dir.join("crash-20261013-141203.txt")),
                            system: Some(diskman_core::crash::system_dir().join("diskman-2026-10-13-141204.ips")),
                            at: std::time::SystemTime::now(),
                        });
                        cx.notify();
                    }),
                    "settings" | "settings-keys" | "settings-grid" | "settings-mcp" | "settings-about" => ws.update(cx, |w, cx| {
                        if stage == "settings-about" {
                            crate::updates::demo_ready(cx);
                        }
                        if stage == "settings-mcp" {
                            let core = w.store.read(cx).core.clone();
                            crate::mcp::demo_seed(&core, false, cx);
                            w.store.update(cx, |s, cx| s.reload(cx));
                        }
                        w.toggle_settings(window, cx);
                        if let Some(v) = w.settings_view.clone() {
                            let section = match stage.as_str() {
                                "settings-keys" => "keyboard",
                                "settings-grid" => "results",
                                "settings-mcp" => "mcp",
                                "settings-about" => "about",
                                _ => "appearance",
                            };
                            v.update(cx, |v, cx| {
                                v.show_section(section, cx);
                                if let Ok(q) = std::env::var("DISKMAN_SHOT_SEARCH") {
                                    v.demo_search(&q, cx);
                                }
                            });
                        }
                    }),
                    // The MCP log in the side panel; a write waiting for Allow.
                    "mcp-log" | "mcp-log-menu" | "mcp-approve" => ws.update(cx, |w, cx| {
                        let core = w.store.read(cx).core.clone();
                        crate::mcp::demo_seed(&core, stage == "mcp-approve", cx);
                        w.store.update(cx, |s, cx| s.reload(cx));
                        if stage != "mcp-approve" {
                            w.toggle_mcp_log(window, cx);
                        } else {
                            w.mcp_sheet_opened(window, cx);
                        }
                    }),
                    "edit" | "edit-review" | "edit-commit" | "edit-conflict" | "edit-readonly" => {
                        let edit = tab.read(cx).edit.clone();
                        window.focus(&grid.focus_handle(cx), cx);
                        edit.update(cx, |e, cx| e.demo_start(window, cx));
                    }
                    "export" | "export-run" | "export-empty" | "export-done" => {
                        if stage == "export-run" {
                            // A big result, paused: "all rows" streams the query again.
                            grid.update(cx, |g, cx| g.select_cells(2..=6, 2, cx));
                        }
                        ws.update(cx, |w, cx| {
                            w.open_export(window, cx);
                            if let Some(e) = w.export.clone() {
                                match stage.as_str() {
                                    "export-run" => e.update(cx, |e, cx| e.demo_start(diskman_core::export::ExportFormat::Parquet, cx)),
                                    "export-done" => e.update(cx, |e, cx| e.demo_start(diskman_core::export::ExportFormat::Csv, cx)),
                                    _ => {}
                                }
                            }
                        });
                    }
                    "main" | "light" => {
                        grid.update(cx, |g, cx| g.select_cells(2..=6, 2, cx));
                        window.focus(&grid.focus_handle(cx), cx);
                    }
                    // The explorer's context menu on `orders`: the menu, its Generate SQL ▸
                    // submenu, a connection's menu, the delete sheet, the generated DDL.
                    "tree-menu" | "tree-menu-sub" | "tree-menu-conn" | "tree-menu-mouse" => {
                        let tree = ws.read(cx).tree.clone();
                        let row = match stage.as_str() {
                            "tree-menu-conn" => format!("c:{id}"),
                            _ => format!("c:{id}/d:{db}/s:public/r:orders"),
                        };
                        tree.update(cx, |t, cx| t.demo_menu(&row, stage == "tree-menu-sub", stage == "tree-menu-mouse", window, cx));
                    }
                    // A database made read-only from its menu: the lock on its row and in the
                    // tab's picker, the menu open again with its check mark.
                    "tree-ro" => {
                        ws.update(cx, |w, cx| w.toggle_read_only(&id, &db, cx));
                        let tree = ws.read(cx).tree.clone();
                        tree.update(cx, |t, cx| t.demo_menu(&format!("c:{id}/d:{db}"), false, true, window, cx));
                    }
                    "editor-scroll" => {
                        editor.update(cx, |e, cx| e.set_cursor(0, cx));
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                    "tree-delete" => ws.update(cx, |w, cx| w.on_row_action(crate::tree_menu::Act::DeleteConnection, crate::tree_menu::Target::Conn { id: id.clone(), name: String::new(), state: crate::tree_menu::ConnPhase::Connected }, false, window, cx)),
                    "tree-ddl" => ws.update(cx, |w, cx| {
                        let target = crate::tree_menu::Target::Rel { conn: id.clone(), db: db.clone(), schema: "public".into(), name: "orders".into(), kind: diskman_core::schema::RelKind::Table, lar: false, ro: false };
                        w.on_row_action(crate::tree_menu::Act::Generate(crate::tree_menu::Gen::CreateTable), target, false, window, cx);
                    }),
                    // The grid's cell menu and the find bars: driven below with real mouse and
                    // key events.
                    s if s.starts_with("fk-")
                        || s.starts_with("find")
                        || s.starts_with("results")
                        || crate::mysql_check::is_stage(s)
                        || crate::mssql_check::is_stage(s)
                        || crate::table_check::is_stage(s)
                        || crate::tx_check::is_stage(s)
                        || crate::import_demo::is_stage(s)
                        || crate::intel_demo::is_stage(s)
                        || crate::plan_demo::is_stage(s)
                        || crate::activity_demo::is_stage(s)
                        || crate::ui_check::is_stage(s)
                        || crate::session_check::is_stage(s)
                        || crate::history_check::is_stage(s)
                        || crate::mcp_check::is_stage(s)
                        || crate::explorer_demo::is_stage(s) => {}
                    other => {
                        crate::lar::demo::stage(other, &ws, window, cx);
                    }
                }
            });
            if stage.starts_with("edit") && stage != "edit-readonly" {
                sleep(cx, 1200).await;
                let edit = cx.update(|_, cx| ws.read(cx).active_tab().unwrap().read(cx).edit.clone()).unwrap();
                let _ = cx.update(|window, cx| edit.update(cx, |e, cx| e.demo_fill(stage == "edit", window, cx)));
                if stage == "edit-conflict" {
                    // Time for a script to change a row underneath (the conflict test).
                    sleep(cx, 4000).await;
                }
                if stage != "edit" {
                    // ⌘S: the review sheet ("edit-review"), or straight to the commit.
                    let _ = cx.update(|window, cx| edit.update(cx, |e, cx| e.review(stage == "edit-review", window, cx)));
                }
                sleep(cx, 1500).await;
            }
            if stage == "sort" {
                sleep(cx, 2500).await;
                let _ = cx.update(|_, cx| {
                    let tab = ws.read(cx).active_tab().unwrap().clone();
                    let grid = tab.read(cx).grid.clone();
                    grid.update(cx, |g, cx| g.toggle_sort(2, cx));
                });
                sleep(cx, 2500).await;
            }
            if stage == "fk-nav-check" {
                let ok = crate::nav_check::run(&ws, cx).await;
                let _ = cx.update(|_, cx| cx.quit());
                if !ok {
                    std::process::exit(1);
                }
                return;
            }
            if stage.starts_with("fk-") {
                let ok = crate::fk_check::run(&ws, &stage, cx).await;
                if stage == "fk-check" || stage == "fk-pop-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if stage.starts_with("find") {
                let ok = crate::find_check::run(&ws, &stage, cx).await;
                if stage == "find-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if stage.starts_with("results") {
                let ok = crate::results_check::run(&ws, &stage, cx).await;
                if stage == "results-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if mysql {
                let ok = crate::mysql_check::run(&ws, &stage, &id, &db, cx).await;
                if stage == "mysql-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if mssql {
                let ok = crate::mssql_check::run(&ws, &stage, &id, &db, cx).await;
                if stage == "mssql-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if crate::table_check::is_stage(&stage) {
                let ok = crate::table_check::run(&ws, &stage, &id, &db, cx).await;
                if stage == "table-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if crate::tx_check::is_stage(&stage) {
                let ok = crate::tx_check::run(&ws, &stage, cx).await;
                if stage == "tx-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if crate::ui_check::is_stage(&stage) {
                let ok = crate::ui_check::run(&ws, &stage, &id, &db, cx).await;
                if stage == "ui-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if crate::session_check::is_stage(&stage) {
                let ok = crate::session_check::run_stage(&ws, &stage, cx).await;
                if stage == "session-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if stage == "sql-panel" {
                let ok = crate::sql_picker::demo_panel(cx).await;
                let _ = cx.update(|_, cx| cx.quit());
                if !ok {
                    std::process::exit(1);
                }
                return;
            }
            if crate::history_check::is_stage(&stage) {
                let ok = crate::history_check::run(&ws, cx).await;
                let _ = cx.update(|_, cx| cx.quit());
                if !ok {
                    std::process::exit(1);
                }
                return;
            }
            if crate::explorer_demo::is_stage(&stage) {
                crate::explorer_demo::run(&ws, &stage, &id, &db, cx).await;
            }
            if crate::intel_demo::is_stage(&stage) {
                let ok = crate::intel_demo::run(&ws, &stage, cx).await;
                if stage == "intel-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if crate::import_demo::is_stage(&stage) {
                crate::import_demo::run(&ws, &stage, &id, &db, cx).await;
            }
            if crate::mcp_check::is_stage(&stage) {
                let ok = crate::mcp_check::run(&ws, &stage, &id, &db, cx).await;
                if stage == "mcp-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if crate::plan_demo::is_stage(&stage) {
                let ok = crate::plan_demo::run(&ws, &stage, cx).await;
                if stage == "plan-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if crate::activity_demo::is_stage(&stage) {
                let ok = crate::activity_demo::run(&ws, &stage, &id, &password, cx).await;
                if stage == "activity-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if stage == "mcp-log-menu" {
                // A row's context menu, once the list is painted (the UPDATE, 3rd from the top).
                sleep(cx, 600).await;
                let _ = cx.update(|_, cx| {
                    if let Some(log) = ws.read(cx).mcp_log() {
                        log.update(cx, |v, cx| v.demo_open_menu(2, cx));
                    }
                });
            }
            if stage == "tree-menu-sub" {
                // A resting pointer over the menu hovers an item when it appears: highlight again.
                sleep(cx, 300).await;
                let _ = cx.update(|_, cx| {
                    let tree = ws.read(cx).tree.clone();
                    tree.update(cx, |t, cx| t.demo_menu_highlight(true, false, cx));
                });
            }
            sleep(cx, if stage.starts_with("dialog-") { 1500 } else { 500 } + crate::lar::demo::wait_ms(&stage)).await;
            // `DISKMAN_SHOT_HOVER=x,y`: the pointer rests there (hover states) for the capture.
            if let Some((x, y)) = std::env::var("DISKMAN_SHOT_HOVER").ok().and_then(|v| v.split_once(',').and_then(|(x, y)| Some((x.parse::<f32>().ok()?, y.parse::<f32>().ok()?)))) {
                let _ = cx.update(|window, cx| {
                    let ev = gpui::MouseMoveEvent { position: gpui::point(gpui::px(x), gpui::px(y)), pressed_button: None, modifiers: Default::default() };
                    window.dispatch_event(gpui::PlatformInput::MouseMove(ev), cx);
                    window.refresh();
                    window.draw(cx).clear(cx);
                });
                sleep(cx, 200).await;
            }
            if let Some(path) = shot_path {
                let _ = cx.update(|window, cx| {
                    window.refresh();
                    window.draw(cx).clear(cx);
                    shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
                    cx.quit();
                });
            }
        })
        .detach();
}

/// Captures the window. The glass is translucent, so the capture is flattened onto the
/// design's desktop color (what shows through the blur in the boards). GPUI's target holds
/// premultiplied color (each fill blends `src·a + dst·(1−a)` over a transparent clear) with
/// additive alpha, which is what the window server composites: color + desk·(1 − alpha).
#[cfg(feature = "screenshots")]
fn shot(window: &Window, path: &str, dark: bool) {
    // `DISKMAN_SHOT_DESK=ffffff`: another desktop color (an adverse one shows seams best).
    let desk = std::env::var("DISKMAN_SHOT_DESK").ok().and_then(|h| u32::from_str_radix(h.trim_start_matches('#'), 16).ok());
    let bg: [f32; 3] = match desk {
        Some(h) => [(h >> 16 & 255) as f32, (h >> 8 & 255) as f32, (h & 255) as f32],
        None if dark => [7., 8., 14.],
        None => [228., 231., 239.],
    };
    match window.render_to_image().map(|mut img| {
        for p in img.pixels_mut() {
            let a = p.0[3] as f32 / 255.;
            for c in 0..3 {
                p.0[c] = (p.0[c] as f32 + bg[c] * (1. - a)).round().min(255.) as u8;
            }
            p.0[3] = 255;
        }
        // `DISKMAN_SHOT_LIGHTS=1`: the native traffic lights are not in the capture; paint
        // where AppKit puts them (`metrics::light_centre`) to check the title bar against them.
        if std::env::var_os("DISKMAN_SHOT_LIGHTS").is_some() {
            use crate::theme::metrics;
            let k = img.width() as f32 / window.viewport_size().width.as_f32();
            let colors = [[255u8, 95, 87], [254, 188, 46], [40, 200, 64]];
            // At a UI zoom AppKit's lights keep their size, centred on the zoomed bar.
            let (lx, ly) = metrics::traffic_light_position_at(crate::theme::zoom());
            let centre = |i: usize| (lx + i as f32 * metrics::LIGHT_STEP + metrics::LIGHT_FRAME.0 / 2., ly + metrics::LIGHT_FRAME.1 / 2.);
            for (i, c) in colors.iter().enumerate() {
                let (cx, cy) = centre(i);
                let r = metrics::LIGHT_D / 2. * k;
                let (cx, cy) = (cx * k, cy * k);
                for y in (cy - r).floor() as u32..(cy + r).ceil() as u32 {
                    for x in (cx - r).floor() as u32..(cx + r).ceil() as u32 {
                        let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                        if dx * dx + dy * dy <= r * r {
                            img.put_pixel(x, y, image::Rgba([c[0], c[1], c[2], 255]));
                        }
                    }
                }
            }
            // The centre line, across the lights and the explorer button.
            let y = (centre(0).1 * k) as u32;
            for x in 0..(140. * k) as u32 {
                if x % 4 < 2 {
                    img.put_pixel(x, y, image::Rgba([255, 0, 255, 255]));
                }
            }
        }
        img
    }) {
        Ok(img) => match img.save(path) {
            Ok(()) => println!("shot: {path}"),
            Err(e) => eprintln!("shot {path}: {e}"),
        },
        Err(e) => eprintln!("shot {path}: {e:#}"),
    }
}

#[cfg(not(feature = "screenshots"))]
fn shot(_: &Window, path: &str, _: bool) {
    eprintln!("shot {path}: build with --features screenshots");
}

fn scroll(ws: Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let (prof, pw) = bench_profile(None);
    let id = prof.id.clone();
    connect(&ws, prof, pw, cx);
    let sql = std::env::var("DISKMAN_BENCH_SQL").unwrap_or_else(|_| "SELECT * FROM orders".into());
    let ws2 = ws.clone();
    when(window, Duration::from_secs(30), move |cx| connected(&ws2, &id, cx), move |window, cx| {
        let tab = ws.read(cx).active_tab().unwrap().clone();
        let grid = tab.read(cx).grid.clone();
        let t0 = Instant::now();
        tab.update(cx, |t, cx| t.run(vec![sql.clone()], cx));
        // Phase A: fling while the result streams in (the grid's demand drives the fetch).
        let mut y = 0f32;
        let mut last = None::<Instant>;
        let mut a = Vec::new();
        let mut b = Vec::new();
        let mut c = Vec::new();
        let mut phase = 0;
        let mut frames_in_phase = 0;
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut first_rows = None;
        every_frame(window, move |_window, cx| {
            let now = Instant::now();
            let rs = tab.read(cx).result().cloned();
            let Some(rs) = rs else { return true };
            if first_rows.is_none() && rs.rows() > 0 {
                first_rows = Some(t0.elapsed());
            }
            if let Some(l) = last {
                match phase {
                    0 => a.push(now - l),
                    1 => b.push(now - l),
                    _ => c.push(now - l),
                }
            }
            last = Some(now);
            frames_in_phase += 1;
            let max = grid.read(cx).max_scroll_y();
            match phase {
                0 => {
                    y += 600.;
                    if rs.phase().is_finished() || frames_in_phase > 600 {
                        let rows = rs.rows();
                        println!("scroll: streaming phase ended after {:?} with {rows} rows loaded ({:?})", t0.elapsed(), rs.phase());
                        rs.fetch_all();
                        phase = 1;
                        frames_in_phase = 0;
                        last = None;
                    }
                }
                1 => {
                    // Wait for everything, then fling across the whole result.
                    if rs.phase() != Phase::Done {
                        last = None;
                        frames_in_phase = 0;
                        return true;
                    }
                    if frames_in_phase == 1 {
                        println!("scroll: all {} rows loaded after {:?}", rs.rows(), t0.elapsed());
                    }
                    y += 600.;
                    if frames_in_phase > 600 {
                        phase = 2;
                        frames_in_phase = 0;
                        last = None;
                    }
                }
                _ => {
                    // Random jumps: every frame shows rows never drawn before.
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    y = (rng % 1_000_000) as f32 / 1_000_000. * max;
                    if frames_in_phase > 600 {
                        println!("scroll: first rows on screen after {:?}", first_rows.unwrap_or_default());
                        println!("{}", Stats(std::mem::take(&mut a)).line("scroll while streaming (600 px/frame)"));
                        println!("{}", Stats(std::mem::take(&mut b)).line("scroll 1M loaded (600 px/frame)"));
                        println!("{}", Stats(std::mem::take(&mut c)).line("random jumps 1M loaded"));
                        cx.quit();
                        return false;
                    }
                }
            }
            if y > max {
                y = 0.;
            }
            grid.update(cx, |g, cx| g.set_scroll_y(y, cx));
            true
        });
    });
}

fn tree(ws: Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let (prof, pw) = bench_profile(Some("diskman_bench"));
    let id = prof.id.clone();
    connect(&ws, prof, pw, cx);
    let tree = ws.read(cx).tree.clone();
    let c = format!("c:{id}");
    let d = format!("{c}/d:diskman_bench");
    let s = format!("{d}/s:bench");
    let g = format!("{s}/tables");
    let ws2 = ws.clone();
    let id2 = id.clone();
    let tree2 = tree.clone();
    let d2 = d.clone();
    when(
        window,
        Duration::from_secs(30),
        move |cx| connected(&ws2, &id2, cx) && tree2.read(cx).has_row(&d2),
        move |window, cx| {
            // 1. Expand the schema: catalog query for 1,000 relations, then the frame.
            let t = Instant::now();
            tree.update(cx, |tr, cx| {
                tr.expand_id(&d, cx);
                tr.expand_id(&s, cx);
            });
            let tree3 = tree.clone();
            let g2 = g.clone();
            when(window, Duration::from_secs(30), move |cx| tree3.read(cx).has_row(&g2), move |window, cx| {
                let loaded = t.elapsed();
                // 2. Expand the "tables" group (cached): 1,000 rows appear.
                let t = Instant::now();
                tree.update(cx, |tr, cx| {
                    tr.expand_id(&g, cx);
                });
                let rebuild = tree.read(cx).last_rebuild;
                let rows = tree.read(cx).row_count();
                let mut first = true;
                let mut last = None::<Instant>;
                let mut frames = Vec::new();
                let mut n = 0;
                every_frame(window, move |_window, cx| {
                    let now = Instant::now();
                    if first {
                        first = false;
                        println!("tree: schema expand (catalog query + frame) {loaded:?}");
                        println!("tree: expand 'tables' (1,000 rows) to next frame {:?}; rebuild {:?}; {rows} rows", t.elapsed(), rebuild);
                    } else if let Some(l) = last {
                        frames.push(now - l);
                    }
                    last = Some(now);
                    // 3. Scroll through the tree, 3 rows per frame.
                    n += 1;
                    let handle = tree.read(cx).scroll_handle().clone();
                    handle.scroll_to_item((n * 3) % rows.max(1), gpui::ScrollStrategy::Top);
                    tree.update(cx, |_, cx| cx.notify());
                    if n > 400 {
                        println!("{}", Stats(std::mem::take(&mut frames)).line("tree scroll (1,000+ rows)"));
                        cx.quit();
                        return false;
                    }
                    true
                });
            });
        },
    );
}

fn typing(ws: Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let (prof, pw) = bench_profile(None);
    let id = prof.id.clone();
    connect(&ws, prof, pw, cx);
    let ws2 = ws.clone();
    when(
        window,
        Duration::from_secs(30),
        move |cx| {
            connected(&ws2, &id, cx)
                && ws2
                    .read(cx)
                    .active_tab()
                    .and_then(|t| t.read(cx).connection().cloned())
                    .is_some_and(|c| {
                        let s = c.schema.snapshot(c.database());
                        s.resolve(None, "orders").is_some_and(|i| !s.columns_of(i).is_empty())
                    })
        },
        move |window, cx| {
            let tab = ws.read(cx).active_tab().unwrap().clone();
            tab.update(cx, |t, cx| t.refresh_schema(cx));
            let editor = tab.read(cx).editor.clone();
            // A big script first: typing must not depend on its size.
            let mut script = String::new();
            for i in 0..2000 {
                script.push_str(&format!("SELECT p.id, p.total FROM orders p WHERE p.customer = {i};\n"));
            }
            script.push('\n');
            editor.update(cx, |e, cx| {
                e.set_text(&script, cx);
                e.set_cursor(script.len(), cx);
            });
            let text: Vec<char> =
                "select p.id, p.status, c.name from orders p join customer c on c.id = p.customer where p.status = 'paid' and p.total > 100 order by p.created_at desc limit 50;"
                    .chars()
                    .collect();
            let mut i = 0;
            let mut pending: Option<Instant> = None;
            let mut lat = Vec::new();
            let mut popup = 0;
            every_frame(window, move |window, cx| {
                if let Some(t) = pending.take() {
                    lat.push(t.elapsed());
                    if editor.read(cx).completion_visible() {
                        popup += 1;
                    }
                }
                if i == text.len() {
                    println!("{}", Stats(std::mem::take(&mut lat)).line("keystroke to frame (2,000-line script, autocomplete on)"));
                    println!("typing: completion popup visible after {popup} of {} keystrokes", text.len());
                    cx.quit();
                    return false;
                }
                let ch = text[i].to_string();
                i += 1;
                let t = Instant::now();
                editor.update(cx, |e, cx| e.type_text(&ch, window, cx));
                pending = Some(t);
                true
            });
        },
    );
}
