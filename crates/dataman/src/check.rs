//! Shared pieces of the E2E check stages (`DATAMAN_SHOT_STAGE=<stage> dataman --demo`, all run
//! by `xtask e2e`): the PASS/FAIL report, real input (keys, clicks on probed controls), waits,
//! the test servers' profiles, and direct reads through a connection of the check's own.

use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;

use gpui::{AsyncWindowContext, Bounds, Entity, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels, Point, point};
use dataman_core::config::{ConnectionProfile, Engine, SslMode};
use dataman_core::nosql::{Output, Page};
use dataman_core::result::Phase;
use dataman_core::Core;

use crate::app::Workspace;
use crate::conns::ConnState;
use crate::tab::QueryTab;

/// The stages that start with no connection and drive everything themselves.
pub fn is_stage(s: &str) -> bool {
    matches!(s, "connections-check" | "tabs-check" | "edit-check" | "explorer-check" | "export-check" | "import-check" | "settings-check" | "welcome-check" | "palette-check" | "nosql-engines-check" | "mcp-tools-check" | "dropdowns-check" | "history-filter-check" | "json-check" | "dates-check" | "agents-check")
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut r = Report::default();
    pause(cx, 300).await;
    match stage {
        "connections-check" => crate::conns_check::run(ws, &mut r, cx).await,
        "tabs-check" => crate::tabs_check::run(ws, &mut r, cx).await,
        "edit-check" => crate::edit_check::run(ws, &mut r, cx).await,
        "explorer-check" => crate::explorer_check::run(ws, &mut r, cx).await,
        "export-check" => crate::export_check::run_export(ws, &mut r, cx).await,
        "import-check" => crate::export_check::run_import(ws, &mut r, cx).await,
        "settings-check" => crate::settings_check::run(ws, &mut r, cx).await,
        "welcome-check" => crate::welcome_check::run_welcome(ws, &mut r, cx).await,
        "palette-check" => crate::welcome_check::run_palette(ws, &mut r, cx).await,
        "nosql-engines-check" => crate::engines_check::run(ws, &mut r, cx).await,
        "mcp-tools-check" => crate::mcp_tools_check::run(ws, &mut r, cx).await,
        "dropdowns-check" => crate::menus_check::run_dropdowns(ws, &mut r, cx).await,
        "history-filter-check" => crate::menus_check::run_history_filter(ws, &mut r, cx).await,
        "json-check" => crate::values_check::run_json(ws, &mut r, cx).await,
        "dates-check" => crate::values_check::run_dates(ws, &mut r, cx).await,
        "agents-check" => crate::agents_check::run(ws, &mut r, cx).await,
        _ => {
            r.check("a known stage", false, stage);
        }
    }
    r.finish(stage)
}

#[derive(Default)]
pub struct Report {
    pass: usize,
    fail: usize,
}

impl Report {
    pub fn check(&mut self, name: &str, pass: bool, detail: impl Display) -> bool {
        if pass {
            self.pass += 1;
            println!("PASS {name}");
        } else {
            self.fail += 1;
            println!("FAIL {name} — {detail}");
        }
        pass
    }

    pub fn finish(&self, stage: &str) -> bool {
        println!("{stage}: {}/{} passed", self.pass, self.pass + self.fail);
        self.fail == 0
    }
}

pub async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

pub fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

/// Polls (drawing between tries) until `ok` or `secs` run out; the last answer.
pub async fn until(cx: &mut AsyncWindowContext, secs: u64, mut ok: impl FnMut(&mut AsyncWindowContext) -> bool) -> bool {
    for _ in 0..secs * 20 {
        draw(cx);
        if ok(cx) {
            return true;
        }
        pause(cx, 50).await;
    }
    draw(cx);
    ok(cx)
}

pub fn key(cx: &mut AsyncWindowContext, k: &str) {
    draw(cx);
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(crate::keymap::keystroke(k), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

pub fn typ(cx: &mut AsyncWindowContext, s: &str) {
    for c in s.chars() {
        let _ = cx.update(|window, cx| {
            let key = if c == ' ' { "space".to_string() } else { c.to_lowercase().to_string() };
            let k = Keystroke { key, key_char: Some(c.to_string()), modifiers: Modifiers { shift: c.is_uppercase(), ..Default::default() } };
            window.dispatch_keystroke(k, cx);
        });
    }
    draw(cx);
}

pub fn input(cx: &mut AsyncWindowContext, e: PlatformInput) {
    let _ = cx.update(|window, cx| {
        window.dispatch_event(e, cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

pub fn move_to(cx: &mut AsyncWindowContext, p: Point<Pixels>) {
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: None, modifiers: Modifiers::default() }));
}

pub fn click_with(cx: &mut AsyncWindowContext, p: Point<Pixels>, button: MouseButton, modifiers: Modifiers, count: usize) {
    move_to(cx, p);
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button, position: p, modifiers, click_count: count, first_mouse: false }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button, position: p, modifiers, click_count: count }));
}

pub fn click(cx: &mut AsyncWindowContext, p: Point<Pixels>) {
    click_with(cx, p, MouseButton::Left, Modifiers::default(), 1);
}

pub fn centre(b: Bounds<Pixels>) -> Point<Pixels> {
    point(b.left() + b.size.width / 2., b.top() + b.size.height / 2.)
}

/// Where a probed control (`ui::probe`) was laid out in the last frame.
pub fn probed(id: &str, cx: &mut AsyncWindowContext) -> Option<Bounds<Pixels>> {
    draw(cx);
    crate::ui::probed(id)
}

/// A real click in the middle of a probed control; false when it is not on screen.
pub fn click_probe(id: &str, cx: &mut AsyncWindowContext) -> bool {
    match probed(id, cx) {
        Some(b) => {
            click(cx, centre(b));
            true
        }
        None => {
            eprintln!("check: no control {id:?} on screen");
            false
        }
    }
}

pub fn spec(var: &str, default: &str) -> Vec<String> {
    std::env::var(var).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.into()).split(':').map(str::to_string).collect()
}

/// A test server's profile (from the `DATAMAN_BENCH_*` the runner sets) and its password.
pub fn profile(engine: Engine) -> (ConnectionProfile, String) {
    match engine {
        Engine::Postgres => crate::bench::bench_profile(None),
        Engine::MySql => crate::bench::mysql_profile(),
        Engine::SqlServer => crate::bench::mssql_profile(),
        Engine::MongoDb => crate::nosql_check::mongo_profile(),
        Engine::Elasticsearch => crate::nosql_check::es_profile(),
        Engine::Redis => crate::nosql_check::redis_profile(),
        Engine::OpenSearch => {
            let p = spec("DATAMAN_BENCH_OPENSEARCH", "127.0.0.1:9200");
            let mut prof = ConnectionProfile::new("opensearch-local");
            prof.engine = Engine::OpenSearch;
            prof.host = p[0].clone();
            prof.port = p[1].parse().unwrap();
            prof.user = p.get(2).cloned().unwrap_or_default();
            prof.database = String::new();
            prof.sslmode = SslMode::Disable;
            prof.save_password = true;
            (prof, p.get(3).cloned().unwrap_or_default())
        }
    }
}

pub fn core(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Arc<Core> {
    cx.update(|_, cx| ws.read(cx).store.read(cx).core.clone()).unwrap()
}

pub fn state(ws: &Entity<Workspace>, id: &str, cx: &mut AsyncWindowContext) -> Option<ConnState> {
    cx.update(|_, cx| ws.read(cx).connection_state(id, cx)).ok().flatten()
}

pub fn is_connected(ws: &Entity<Workspace>, id: &str, cx: &mut AsyncWindowContext) -> bool {
    matches!(state(ws, id, cx), Some(ConnState::Connected(_)))
}

/// Saves the profile and connects it, as the app does after the sheet's Save; Err on failure.
pub async fn connect(ws: &Entity<Workspace>, prof: &ConnectionProfile, pw: &str, cx: &mut AsyncWindowContext) -> Result<(), String> {
    let _ = cx.update(|_, cx| crate::bench::connect(ws, prof.clone(), pw.to_string(), cx));
    let up = connected(ws, &prof.id, 30, cx).await;
    // Saved through the sheet, the editor would have the focus back: keys go where a person's would.
    let _ = cx.update(|window, cx| {
        if let Some(t) = ws.read(cx).active_tab() {
            let e = t.read(cx).editor.clone();
            window.focus(&gpui::Focusable::focus_handle(e.read(cx), cx), cx);
        }
    });
    draw(cx);
    up
}

pub async fn connected(ws: &Entity<Workspace>, id: &str, secs: u64, cx: &mut AsyncWindowContext) -> Result<(), String> {
    for _ in 0..secs * 20 {
        match state(ws, id, cx) {
            Some(ConnState::Connected(_)) => return Ok(()),
            Some(ConnState::Failed(e)) => return Err(e),
            _ => {}
        }
        pause(cx, 50).await;
    }
    Err("timed out".into())
}

pub fn active(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Entity<QueryTab>> {
    cx.update(|_, cx| ws.read(cx).active_tab().cloned()).ok().flatten()
}

pub fn tabs(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Vec<Entity<QueryTab>> {
    cx.update(|_, cx| ws.read(cx).tabs.clone()).unwrap_or_default()
}

pub fn tab_conn(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> Option<String> {
    cx.update(|_, cx| tab.read(cx).connection().map(|c| c.profile.id.clone())).ok().flatten()
}

pub fn idle(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|_, cx| !tab.read(cx).is_running()).unwrap_or(true)
}

/// The text into the active tab's editor, focused, then `keys` (⌘↩ runs it).
pub async fn run_keys(ws: &Entity<Workspace>, text: &str, keys: &str, cx: &mut AsyncWindowContext) -> Option<Entity<QueryTab>> {
    let tab = active(ws, cx)?;
    let _ = cx.update(|window, cx| {
        let editor = tab.read(cx).editor.clone();
        editor.update(cx, |e, cx| {
            e.set_text(text, cx);
            e.set_cursor(0, cx);
        });
        window.focus(&gpui::Focusable::focus_handle(editor.read(cx), cx), cx);
    });
    key(cx, keys);
    pause(cx, 50).await;
    until(cx, 30, |cx| idle(&tab, cx)).await;
    Some(tab)
}

/// The active tab's (SQL) result: phase, rows, error.
pub fn result(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> (Option<Phase>, usize, String) {
    cx.update(|_, cx| {
        let t = tab.read(cx);
        match t.result() {
            Some(rs) => (Some(rs.phase()), rs.rows(), rs.status().error.map(|e| e.message.clone()).unwrap_or_default()),
            None => (None, 0, String::new()),
        }
    })
    .unwrap_or((None, 0, String::new()))
}

pub fn cell(tab: &Entity<QueryTab>, row: usize, col: usize, cx: &mut AsyncWindowContext) -> Option<String> {
    cx.update(|_, cx| tab.read(cx).result().and_then(|rs| rs.snapshot().text(row, col).map(|s| s.into_owned()))).ok().flatten()
}

/// Rows of a statement run on a connection of the check's own (not the app's), as text.
pub async fn sql(core: &Arc<Core>, prof: &ConnectionProfile, pw: &str, stmt: &str, cx: &mut AsyncWindowContext) -> Result<Vec<Vec<Option<String>>>, String> {
    let (core, prof, pw, stmt) = (core.clone(), prof.clone(), pw.to_string(), stmt.to_string());
    cx.background_executor()
        .spawn(async move {
            let c = core.block_on(core.connect(prof, Some(pw))).map_err(|e| format!("{e:#}"))?;
            let rs = c.session(None).execute(stmt.clone());
            rs.fetch_all();
            for _ in 0..600 {
                if rs.phase().is_finished() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            if rs.phase() != Phase::Done {
                return Err(format!("{stmt}: {:?} {}", rs.phase(), rs.status().error.map(|e| e.message.clone()).unwrap_or_default()));
            }
            let s = rs.snapshot();
            Ok((0..s.rows()).map(|r| (0..s.columns.len()).map(|c| s.text(r, c).map(|v| v.into_owned())).collect()).collect())
        })
        .await
}

/// The first value of the first row (or None).
pub async fn sql1(core: &Arc<Core>, prof: &ConnectionProfile, pw: &str, stmt: &str, cx: &mut AsyncWindowContext) -> Option<String> {
    match sql(core, prof, pw, stmt, cx).await {
        Ok(rows) => rows.first().and_then(|r| r.first().cloned()).flatten(),
        Err(e) => {
            eprintln!("check: {e}");
            None
        }
    }
}

/// A NoSQL statement on a connection of the check's own.
pub async fn nosql(core: &Arc<Core>, prof: &ConnectionProfile, pw: &str, db: &str, stmt: &str, cx: &mut AsyncWindowContext) -> Result<Output, String> {
    let (core, prof, pw, db, stmt) = (core.clone(), prof.clone(), pw.to_string(), db.to_string(), stmt.to_string());
    cx.background_executor()
        .spawn(async move {
            let c = core.block_on(core.connect(prof, Some(pw))).map_err(|e| format!("{e:#}"))?;
            core.block_on(c.run_nosql(&db, &stmt, Page::default())).map(|r| r.output).map_err(|e| format!("{e:#}"))
        })
        .await
}

pub fn clipboard(cx: &mut AsyncWindowContext) -> String {
    cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default()).unwrap_or_default()
}

pub fn set_clipboard(text: &str, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string())));
}

/// Whether a sample taken now can still fall inside an animation `d` long that began at `t0`.
/// On a slow renderer (Xvfb's software GL in CI) one frame can outlast an animation: then there
/// is no mid-way to see, and a check holds the animation to where it lands only.
pub fn mid_possible(t0: std::time::Instant, d: std::time::Duration) -> bool {
    t0.elapsed() < d.mul_f32(0.9)
}

/// A time budget set on macOS (Metal, the machine it was measured on). Linux CI draws with Mesa's
/// software GL under Xvfb, in a VM: 2.5 times the budget there, so a regression still shows.
pub fn budget(mac: std::time::Duration) -> std::time::Duration {
    if cfg!(target_os = "linux") { mac.mul_f32(2.5) } else { mac }
}
