use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{App, Entity, Focusable, Window};
use dataman_core::result::Phase;
use dataman_core::{ConnectionProfile, SslMode};

use crate::app::Workspace;
use crate::conns::ConnState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Startup,
    Scroll,
    Tree,
    Typing,
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
    let spec = std::env::var("DATAMAN_BENCH_PG").unwrap_or_else(|_| "127.0.0.1:55432:postgres:dataman:dataman".into());
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

pub fn mysql_profile() -> (ConnectionProfile, String) {
    let spec = std::env::var("DATAMAN_BENCH_MYSQL").unwrap_or_else(|_| "127.0.0.1:33306:root:dataman_test".into());
    let p: Vec<&str> = spec.split(':').collect();
    let mut prof = ConnectionProfile::new("mysql-local");
    prof.engine = dataman_core::config::Engine::MySql;
    prof.host = p[0].into();
    prof.port = p[1].parse().unwrap();
    prof.user = p[2].into();
    prof.database = p[3].into();
    prof.sslmode = SslMode::Disable;
    prof.save_password = true;
    (prof, std::env::var("DATAMAN_MYSQL_PASSWORD").unwrap_or_else(|_| "dataman-root".into()))
}

pub fn mssql_profile() -> (ConnectionProfile, String) {
    let spec = std::env::var("DATAMAN_BENCH_MSSQL").unwrap_or_else(|_| "127.0.0.1:14330:sa:dataman_test".into());
    let p: Vec<&str> = spec.split(':').collect();
    let mut prof = ConnectionProfile::new("mssql-local");
    prof.name = "dataman_test@mssql".into();
    prof.engine = dataman_core::config::Engine::SqlServer;
    prof.host = p[0].into();
    prof.port = p[1].parse().unwrap();
    prof.user = p[2].into();
    prof.database = p[3].into();
    prof.sslmode = SslMode::Require;
    prof.save_password = true;
    (prof, std::env::var("DATAMAN_MSSQL_PASSWORD").unwrap_or_else(|_| "Dataman-sa-1!".into()))
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

fn when(window: &mut Window, timeout: Duration, mut ready: impl FnMut(&mut App) -> bool + 'static, then: impl FnOnce(&mut Window, &mut App) + 'static) {
    let start = Instant::now();
    let mut then = Some(then);
    let mut polls = 0u64;
    every_frame(window, move |window, cx| {
        polls += 1;
        if std::env::var("DATAMAN_DEBUG").is_ok() && polls % 60 == 0 {
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

fn demo_importable() -> Vec<dataman_core::Importable> {
    let mk = |name: &str, host: &str, port: u16, user: &str, db: &str, source, exists| {
        let mut p = dataman_core::ConnectionProfile::new(name);
        (p.host, p.port, p.user, p.database) = (host.into(), port, user.into(), db.into());
        dataman_core::Importable { source, profile: p, password: None, exists }
    };
    use dataman_core::ImportSource::{Pgpass, Service};
    vec![
        mk("local", "127.0.0.1", 55432, "postgres", "dataman", Service, true),
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
    ws.update(cx, |w, cx| w.close_dialog(cx));
    match mode {
        Mode::Startup => {}
        Mode::Scroll => scroll(ws, window, cx),
        Mode::Tree => tree(ws, window, cx),
        Mode::Typing => typing(ws, window, cx),
        Mode::Demo => demo(ws, window, cx),
    }
}

fn demo(ws: Entity<Workspace>, window: &mut Window, cx: &mut App) {
    crate::ui::enable_probes();
    let raw = std::env::var("DATAMAN_SHOT_STAGE").unwrap_or_else(|_| "main".into());
    let shot_path = std::env::var("DATAMAN_SHOT").ok();
    if raw == "light" || raw.ends_with("-light") {
        crate::theme::set_pref(dataman_core::ThemePref::Light, cx);
    }
    if std::env::var_os("DATAMAN_SHOT_NO_EXPLORER").is_some() {
        ws.update(cx, |w, cx| {
            w.explorer_open = false;
            cx.notify();
        });
    }
    let stage = raw.trim_end_matches("-light").to_string();
    if crate::newuser_check::is_stage(&stage) {
        window
            .spawn(cx, async move |cx| {
                let ok = crate::newuser_check::run(&ws, cx).await;
                let _ = cx.update(|_, cx| cx.quit());
                if !ok {
                    std::process::exit(1);
                }
            })
            .detach();
        return;
    }
    if crate::check::is_stage(&stage) {
        window
            .spawn(cx, async move |cx| {
                let ok = crate::check::run(&ws, &stage, cx).await;
                let _ = cx.update(|_, cx| cx.quit());
                if !ok {
                    std::process::exit(1);
                }
            })
            .detach();
        return;
    }
    if crate::nosql_check::is_stage(&stage) {
        window
            .spawn(cx, async move |cx| {
                if crate::nosql_check::is_demo(&stage) {
                    crate::nosql_check::demo(&ws, &stage, cx).await;
                    return;
                }
                let ok = crate::nosql_check::run(&ws, cx).await;
                let _ = cx.update(|_, cx| cx.quit());
                if !ok {
                    std::process::exit(1);
                }
            })
            .detach();
        return;
    }
    if stage == "conns-check" {
        window
            .spawn(cx, async move |cx| {
                let ok = crate::mcp_check::run_conns(&ws, cx).await;
                let _ = cx.update(|_, cx| cx.quit());
                if !ok {
                    std::process::exit(1);
                }
            })
            .detach();
        return;
    }
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
    let default_sql = crate::lar::demo::sql(&stage).unwrap_or_else(|| match stage.as_str() {
        "error" | "error-pop" => "-- shipping backfill (3 statements)\nSELECT count(*) FROM orders;\n\nUPDATE orders p\n   SET status = s.delivered_at\n  FROM orders s\n WHERE s.id = p.id;\n\nSELECT count(*) FROM customer;".to_string(),
        "edit" | "edit-review" | "edit-commit" | "edit-conflict" => "SELECT * FROM dataman_scratch.products ORDER BY id;".to_string(),
        "export-done" => "SELECT * FROM dataman_scratch.products ORDER BY id;".to_string(),
        "editor-scroll" => {
            let mut s = String::from("-- monthly report, every region\n");
            for i in 1..=80 {
                s.push_str(&format!("SELECT {i} AS region, count(*) AS orders, sum(p.total) AS total, avg(p.total) AS average, min(p.id) AS first_id, max(p.id) AS last_id FROM orders p WHERE p.customer % 80 = {i};\n"));
            }
            s
        }
        s if s.starts_with("fk-") => crate::fk_check::SQL.to_string(),
        s if s.starts_with("results") => crate::results_check::SQL.to_string(),
        s if s.starts_with("value") => crate::value_check::SQL.to_string(),
        "edit-readonly" => "SELECT p.id, p.status, c.name FROM orders p JOIN customer c ON c.id = p.customer LIMIT 50;".to_string(),
        "cancelling" => "-- full scan, then a long wait\nSELECT p.customer, count(*), sum(p.total)\nFROM   orders p\nGROUP  BY p.customer\nORDER  BY sum(p.total) DESC;".to_string(),
        _ => "-- orders with their customer, newest first\nSELECT p.id, p.status, p.total, p.address, c.name, NULL AS nothing, '' AS empty\nFROM   orders p\nJOIN   customer c ON c.id = p.customer\nORDER  BY p.id;".to_string(),
    });
    let sql = std::env::var("DATAMAN_DEMO_SQL").unwrap_or(default_sql);
    window
        .spawn(cx, async move |cx| {
            let sleep = |cx: &mut gpui::AsyncWindowContext, ms| cx.background_executor().timer(Duration::from_millis(ms));
            for _ in 0..600 {
                sleep(cx, 50).await;
                if cx.update(|_, cx| connected(&ws, &id, cx)).unwrap_or(false) {
                    break;
                }
            }
            let run = !matches!(
                stage.as_str(),
                "export-empty"
                    | "empty"
                    | "dialog"
                    | "dialog-advanced"
                    | "dialog-advanced-prod"
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
                && !crate::insert_check::is_stage(&stage)
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
                    "dialog-advanced" | "dialog-advanced-prod" => ws.update(cx, |w, cx| {
                        let core = w.store.read(cx).core.clone();
                        crate::mcp::demo_seed(&core, false, cx);
                        w.store.update(cx, |s, cx| s.reload(cx));
                        let prod = stage == "dialog-advanced-prod";
                        let name = if prod { "shop-live" } else { "shop" };
                        let id = core.config.connections().into_iter().find(|p| p.name == name).map(|p| p.id);
                        w.open_dialog(id, window, cx);
                        if let Some(d) = w.dialog.clone() {
                            d.update(cx, |d, cx| d.demo_advanced(prod.then_some("DELETE FROM sessions WHERE expires_at < ?"), true, cx));
                        }
                    }),
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
                    "dialog-ssh" | "dialog-hostkey" => ws.update(cx, |w, cx| {
                        let (mut p, _) = bench_profile(None);
                        (p.name, p.host, p.port, p.database) = ("prod".into(), "db.internal".into(), 5432, "app".into());
                        p.ssh = Some(dataman_core::SshSettings {
                            host: "bastion".into(),
                            port: None,
                            user: "deploy".into(),
                            auth: dataman_core::SshAuth::Key,
                            key_file: "~/.ssh/id_ed25519".into(),
                        });
                        w.open_dialog_with(Some(p), true, None, window, cx);
                        if stage == "dialog-hostkey"
                            && let Some(d) = w.dialog.clone()
                            && let Some(k) = dataman_core::HostKeyPrompt::from_openssh(
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
                        let mut p = dataman_core::ConnectionProfile::new("prod via bastion");
                        (p.host, p.database, p.color) = ("db.internal".into(), "app".into(), Some("violet".into()));
                        p.ssh = Some(dataman_core::SshSettings { host: "bastion".into(), ..Default::default() });
                        let core = w.store.read(cx).core.clone();
                        core.save_connection(p, None, None).unwrap();
                        w.store.update(cx, |s, cx| s.reload(cx));
                    }),
                    "sort" => grid.update(cx, |g, cx| g.toggle_sort(2, cx)),
                    "history" => {
                        ws.update(cx, |w, cx| w.show_nav(dataman_core::layout::NavView::History, window, cx));
                        let tree = ws.read(cx).tree.clone();
                        tree.update(cx, |t, cx| t.set_filter("ord", cx));
                    }
                    "marks" | "marks-history" | "marks-search" => {
                        if stage != "marks" {
                            ws.update(cx, |w, cx| w.set_nav(dataman_core::layout::NavView::History, cx));
                        }
                        let tree = ws.read(cx).tree.clone();
                        tree.update(cx, |t, cx| t.set_filter("ord", cx));
                        let history = ws.read(cx).history.clone();
                        let favorites = stage == "marks-history";
                        history.update(cx, |h, cx| h.demo_marks(favorites, window, cx));
                        match stage.as_str() {
                            "marks" => tree.update(cx, |t, cx| t.demo_focus_filter(window, cx)),
                            "marks-search" => history.update(cx, |h, cx| h.demo_search("orders", window, cx)),
                            _ => {}
                        }
                    }
                    "copied" => {
                        grid.update(cx, |g, cx| g.select_cells(2..=6, 2, cx));
                        window.focus(&grid.focus_handle(cx), cx);
                        grid.update(cx, |g, cx| g.copy(dataman_core::result::CopyFormat::Tsv, false, cx));
                    }
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
                    "value" => {
                        grid.update(cx, |g, cx| g.select_cells(4..=4, 5, cx));
                        window.focus(&grid.focus_handle(cx), cx);
                        if let Some(cell) = grid.read(cx).value_at(4, 5) {
                            ws.update(cx, |w, cx| w.open_value(cell, window, cx));
                        }
                    }
                    "focus" => ws.update(cx, |w, cx| w.focus_region(crate::app::Region::Grid, window, cx)),
                    "selection" => {
                        editor.update(cx, |e, cx| e.demo_select(30, 150, cx));
                        window.focus(&editor.focus_handle(cx), cx);
                    }
                    "shortcuts" => ws.update(cx, |w, cx| {
                        w.shortcuts_open = true;
                        cx.notify();
                    }),
                    "saved" => ws.update(cx, |w, cx| w.save_favorite(cx)),
                    "palette-favorites" | "fav-menu" => ws.update(cx, |w, cx| {
                        let core = w.store.read(cx).core.clone();
                        let words = ["Orders by day", "Top customers", "Revenue by month", "Churned accounts", "Slow queries", "Open carts"];
                        for i in 0..36 {
                            let mut f = dataman_core::Favorite::new(format!("{} {}", words[i % words.len()], i / words.len() + 1), format!("SELECT {i} AS n, count(*) FROM orders WHERE customer % 36 = {i};"));
                            f.connection_id = Some(id.clone());
                            let _ = core.favorites.add(f);
                        }
                        if stage == "fav-menu" {
                            w.set_nav(dataman_core::layout::NavView::Favorites, cx);
                            let history = w.history.clone();
                            history.update(cx, |h, cx| h.demo_show_favorites(cx));
                        } else {
                            w.open_palette(crate::palette::Scope::All, window, cx);
                            if let Some(p) = w.palette.clone() {
                                p.update(cx, |p, cx| p.set_query("*", cx));
                            }
                        }
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
                    "update" | "update-sheet" | "update-progress" | "update-palette" => {
                        let step = (stage == "update-progress").then_some(crate::updates::Step::Downloading { got: 12_400_000, total: 34_603_008 });
                        crate::updates::demo_offer(step, cx);
                        if stage == "update-sheet" || stage == "update-progress" {
                            ws.update(cx, |w, cx| w.open_update_sheet(window, cx));
                        }
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
                        let dir = dataman_core::crash::dir();
                        w.crash = Some(dataman_core::crash::Pending {
                            report: Some(dir.join("crash-20261013-141203.txt")),
                            system: Some(dataman_core::crash::system_dir().join("dataman-2026-10-13-141204.ips")),
                            at: std::time::SystemTime::now(),
                        });
                        cx.notify();
                    }),
                    "settings" | "settings-keys" | "settings-grid" | "settings-mcp" | "settings-about" => ws.update(cx, |w, cx| {
                        if stage == "settings-about" {
                            crate::updates::demo_offer(None, cx);
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
                                if let Ok(q) = std::env::var("DATAMAN_SHOT_SEARCH") {
                                    v.demo_search(&q, cx);
                                }
                                if let Ok(k) = std::env::var("DATAMAN_SHOT_KEY_SEARCH") {
                                    v.demo_key_search_with(Some(k.as_str()).filter(|k| !k.is_empty()), window, cx);
                                }
                                if let Some(y) = std::env::var("DATAMAN_SHOT_SCROLL").ok().and_then(|y| y.parse::<f32>().ok()) {
                                    v.demo_scroll(y);
                                }
                            });
                        }
                    }),
                    s if crate::agents_check::is_demo(s) => crate::agents_check::demo(&ws, s, window, cx),
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
                            grid.update(cx, |g, cx| g.select_cells(2..=6, 2, cx));
                        }
                        ws.update(cx, |w, cx| {
                            w.open_export(window, cx);
                            if let Some(e) = w.export.clone() {
                                match stage.as_str() {
                                    "export-run" => e.update(cx, |e, cx| e.demo_start(dataman_core::export::ExportFormat::Parquet, cx)),
                                    "export-done" => e.update(cx, |e, cx| e.demo_start(dataman_core::export::ExportFormat::Csv, cx)),
                                    _ => {}
                                }
                            }
                        });
                    }
                    "main" | "light" => {
                        grid.update(cx, |g, cx| g.select_cells(2..=6, 2, cx));
                        window.focus(&grid.focus_handle(cx), cx);
                    }
                    "tree-menu" | "tree-menu-sub" | "tree-menu-conn" | "tree-menu-mouse" => {
                        let tree = ws.read(cx).tree.clone();
                        let row = match stage.as_str() {
                            "tree-menu-conn" => format!("c:{id}"),
                            _ => format!("c:{id}/d:{db}/s:public/r:orders"),
                        };
                        tree.update(cx, |t, cx| t.demo_menu(&row, stage == "tree-menu-sub", stage == "tree-menu-mouse", window, cx));
                    }
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
                        let target = crate::tree_menu::Target::Rel { conn: id.clone(), db: db.clone(), schema: "public".into(), name: "orders".into(), kind: dataman_core::schema::RelKind::Table, lar: false, ro: false };
                        w.on_row_action(crate::tree_menu::Act::Generate(crate::tree_menu::Gen::CreateTable), target, false, window, cx);
                    }),
                    s if s.starts_with("fk-")
                        || s.starts_with("find")
                        || s.starts_with("results")
                        || crate::mysql_check::is_stage(s)
                        || crate::mssql_check::is_stage(s)
                        || crate::table_check::is_stage(s)
                        || crate::tx_check::is_stage(s)
                        || crate::insert_check::is_stage(s)
                        || crate::import_demo::is_stage(s)
                        || crate::intel_demo::is_stage(s)
                        || crate::plan_demo::is_stage(s)
                        || crate::activity_demo::is_stage(s)
                        || crate::ui_check::is_stage(s)
                        || crate::dock_check::is_stage(s)
                        || crate::split_check::is_stage(s)
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
                    sleep(cx, 4000).await;
                }
                if stage != "edit" {
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
            if crate::insert_check::is_stage(&stage) {
                let ok = crate::insert_check::run(&ws, cx).await;
                let _ = cx.update(|_, cx| cx.quit());
                if !ok {
                    std::process::exit(1);
                }
                return;
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
            if crate::dock_check::is_stage(&stage) {
                let ok = crate::dock_check::run(&ws, &stage, cx).await;
                if stage == "dock-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if crate::value_check::is_stage(&stage) {
                let ok = crate::value_check::run(&ws, cx).await;
                let _ = cx.update(|_, cx| cx.quit());
                if !ok {
                    std::process::exit(1);
                }
                return;
            }
            if crate::split_check::is_stage(&stage) {
                let ok = crate::split_check::run(&ws, &stage, cx).await;
                if stage == "split-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if crate::update_check::is_stage(&stage) {
                let ok = crate::update_check::run(&ws, cx).await;
                let _ = cx.update(|_, cx| cx.quit());
                if !ok {
                    std::process::exit(1);
                }
                return;
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
            if crate::input_check::is_stage(&stage) {
                let ok = crate::input_check::run(&ws, &stage, cx).await;
                if stage == "input-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if crate::undo_check::is_stage(&stage) {
                let ok = crate::undo_check::run(&ws, &id, &db, cx).await;
                let _ = cx.update(|_, cx| cx.quit());
                if !ok {
                    std::process::exit(1);
                }
                return;
            }
            if crate::pin_check::is_stage(&stage) {
                let ok = crate::pin_check::run(&ws, &stage, &id, &db, cx).await;
                if stage == "pin-check" {
                    let _ = cx.update(|_, cx| cx.quit());
                    if !ok {
                        std::process::exit(1);
                    }
                    return;
                }
            }
            if stage == "fav-menu" {
                sleep(cx, 600).await;
                let _ = cx.update(|_, cx| {
                    let history = ws.read(cx).history.clone();
                    history.update(cx, |h, cx| h.demo_open_fav_menu(1, cx));
                });
            }
            if stage == "mcp-log-menu" {
                sleep(cx, 600).await;
                let _ = cx.update(|_, cx| {
                    if let Some(log) = ws.read(cx).mcp_log() {
                        log.update(cx, |v, cx| v.demo_open_menu(2, cx));
                    }
                });
            }
            if stage == "tree-menu-sub" {
                sleep(cx, 300).await;
                let _ = cx.update(|_, cx| {
                    let tree = ws.read(cx).tree.clone();
                    tree.update(cx, |t, cx| t.demo_menu_highlight(true, false, cx));
                });
            }
            sleep(cx, if stage.starts_with("dialog-") { 1500 } else { 500 } + crate::lar::demo::wait_ms(&stage)).await;
            if let Some((x, y)) = std::env::var("DATAMAN_SHOT_HOVER").ok().and_then(|v| v.split_once(',').and_then(|(x, y)| Some((x.parse::<f32>().ok()?, y.parse::<f32>().ok()?)))) {
                for wait in [900, 300] {
                    let _ = cx.update(|window, cx| {
                        let ev = gpui::MouseMoveEvent { position: gpui::point(gpui::px(x), gpui::px(y)), pressed_button: None, modifiers: Default::default() };
                        window.dispatch_event(gpui::PlatformInput::MouseMove(ev), cx);
                        window.refresh();
                        window.draw(cx).clear(cx);
                    });
                    sleep(cx, wait).await;
                }
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

/// GPUI's target holds premultiplied color with additive alpha, which is what the window
/// server composites: flatten as color + desk·(1 − alpha).
#[cfg(feature = "screenshots")]
pub(crate) fn shot(window: &Window, path: &str, dark: bool) {
    let desk = std::env::var("DATAMAN_SHOT_DESK").ok().and_then(|h| u32::from_str_radix(h.trim_start_matches('#'), 16).ok());
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
        if std::env::var_os("DATAMAN_SHOT_LIGHTS").is_some() {
            use crate::theme::metrics;
            let k = img.width() as f32 / window.viewport_size().width.as_f32();
            let colors = [[255u8, 95, 87], [254, 188, 46], [40, 200, 64]];
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
pub(crate) fn shot(_: &Window, path: &str, _: bool) {
    eprintln!("shot {path}: build with --features screenshots");
}

fn scroll(ws: Entity<Workspace>, window: &mut Window, cx: &mut App) {
    let (prof, pw) = bench_profile(None);
    let id = prof.id.clone();
    connect(&ws, prof, pw, cx);
    let sql = std::env::var("DATAMAN_BENCH_SQL").unwrap_or_else(|_| "SELECT * FROM orders".into());
    let ws2 = ws.clone();
    when(window, Duration::from_secs(30), move |cx| connected(&ws2, &id, cx), move |window, cx| {
        let tab = ws.read(cx).active_tab().unwrap().clone();
        let grid = tab.read(cx).grid.clone();
        let t0 = Instant::now();
        tab.update(cx, |t, cx| t.run(vec![sql.clone()], cx));
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
    let (prof, pw) = bench_profile(Some("dataman_bench"));
    let id = prof.id.clone();
    connect(&ws, prof, pw, cx);
    let tree = ws.read(cx).tree.clone();
    let c = format!("c:{id}");
    let d = format!("{c}/d:dataman_bench");
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
            let t = Instant::now();
            tree.update(cx, |tr, cx| {
                tr.expand_id(&d, cx);
                tr.expand_id(&s, cx);
            });
            let tree3 = tree.clone();
            let g2 = g.clone();
            when(window, Duration::from_secs(30), move |cx| tree3.read(cx).has_row(&g2), move |window, cx| {
                let loaded = t.elapsed();
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

pub(crate) fn capture(window: &Window, path: &str, cx: &App) {
    shot(window, path, crate::theme::ActiveTheme::theme(cx).is_dark());
}
