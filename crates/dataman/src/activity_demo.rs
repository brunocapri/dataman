//! Demo stages of the activity monitor (`--demo`), on the demo database:
//!
//! - `activity-sessions`, `activity-locks`, `activity-tables`, `activity-statements`: a scratch
//!   schema with a table full of dead tuples, and outside connections (their own application
//!   names) that hold a row lock in an open transaction, wait on it, run a long query and sit
//!   idle in a transaction; the monitor on each section. The thresholds are lowered to seconds
//!   (the sessions are seconds old), and the statements are sample rows when the server has no
//!   `pg_stat_statements` loaded; `activity-statements-state` shows the extension's state line
//!   as the server reports it; `activity-terminate` the confirmation sheet over the blocker.
//! - `activity-check`: the rules with real key events (the tab, the default filter, this app's
//!   backends, the blocking tree, Cancel and Terminate through the sheet, the refresh stopping
//!   when the tab is hidden, the window inactive or the interval off, and redrawing nothing
//!   while nothing changes), printed as PASS/FAIL lines; exit code 1 on a failure.

use std::sync::Arc;
use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Keystroke};
use dataman_core::activity::{Signal, State, StatementRow, Statements, Thresholds};
use dataman_core::result::Phase;
use dataman_core::{Connection, TxStatus};
use tokio_postgres::NoTls;

use crate::activity::{ActivityView, Every, Section};
use crate::app::Workspace;

pub const SCHEMA: &str = "dataman_activity_demo";

pub fn is_stage(s: &str) -> bool {
    s.starts_with("activity-")
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

async fn until(cx: &mut AsyncWindowContext, ms: u64, ready: impl Fn(&mut gpui::App) -> bool) -> bool {
    for _ in 0..ms / 50 {
        if cx.update(|_, cx| ready(cx)).unwrap_or(false) {
            return true;
        }
        pause(cx, 50).await;
    }
    false
}

fn key(cx: &mut AsyncWindowContext, k: &str) {
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(Keystroke::parse(k).unwrap(), cx);
        window.refresh();
    });
}

/// Runs statements in a fresh session of `conn`, waiting for each.
async fn exec(conn: &Arc<Connection>, sqls: &[&str], cx: &mut AsyncWindowContext) {
    let s = conn.session(None);
    for sql in sqls {
        let rs = s.execute(*sql);
        let rs2 = rs.clone();
        until(cx, 60_000, move |_| rs2.phase().is_finished()).await;
        if rs.phase() != Phase::Done {
            eprintln!("activity demo: {sql}: {:?}", rs.status().error);
        }
    }
}

/// A connection of another program (its own application name, not this app's): it runs
/// `sqls` and then holds its session open until the process ends.
fn outside(conn: &Arc<Connection>, password: &str, app: &str, sqls: &[&str]) {
    let p = &conn.profile;
    let mut cfg = tokio_postgres::Config::new();
    cfg.host(&p.host).port(p.port).user(&p.user).password(password).dbname(conn.database()).application_name(app);
    let sqls: Vec<String> = sqls.iter().map(|s| s.to_string()).collect();
    conn.handle().spawn(async move {
        let Ok((client, connection)) = cfg.connect(NoTls).await else { return };
        tokio::spawn(connection);
        for s in &sqls {
            if let Err(e) = client.simple_query(s).await {
                eprintln!("activity demo ({s}): {e}");
            }
        }
        std::future::pending::<()>().await;
        drop(client);
    });
}

fn monitor_of(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Entity<ActivityView>> {
    cx.update(|_, cx| ws.read(cx).active_tab().and_then(|t| t.read(cx).monitor.clone())).ok().flatten()
}

fn sample_statements() -> Vec<StatementRow> {
    let s = |q: &str, calls: i64, total: f64, rows: i64, hit: i64, read: i64, temp: i64| StatementRow {
        queryid: Some(format!("{}", q.len() as i64 * 7_919 + calls)),
        query: q.into(),
        user: Some("app".into()),
        db: Some("dataman".into()),
        calls,
        total_ms: total,
        mean_ms: total / calls as f64,
        min_ms: Some(total / calls as f64 * 0.2),
        max_ms: Some(total / calls as f64 * 6.),
        stddev_ms: Some(total / calls as f64 * 0.8),
        rows,
        shared_hit: hit,
        shared_read: read,
        temp_blocks: temp,
        plans: Some(calls),
        plan_ms: Some(calls as f64 * 0.04),
    };
    vec![
        s("SELECT p.id, p.total, c.name FROM orders p JOIN customer c ON c.id = p.customer WHERE p.status = $1 ORDER BY p.id DESC LIMIT $2", 48_210, 913_402.5, 2_410_500, 9_120_330, 412_008, 0),
        s("UPDATE dataman_activity_demo.accounts SET balance = balance - $1 WHERE id = $2", 211_377, 402_118.9, 211_377, 1_980_440, 2_118, 0),
        s("SELECT c.country, count(*), sum(p.total) FROM orders p JOIN customer c ON c.id = p.customer GROUP BY c.country ORDER BY 3 DESC", 96, 288_034.7, 1_920, 310_922, 801_455, 61_440),
        s("INSERT INTO dataman_activity_demo.orders (customer, total) VALUES ($1, $2) RETURNING id", 390_552, 118_930.2, 390_552, 2_760_110, 9_802, 0),
        s("SELECT * FROM customer WHERE email = $1", 1_204_877, 96_321.4, 1_204_877, 4_819_508, 33, 0),
        s("DELETE FROM dataman_activity_demo.orders WHERE id % $1 = $2", 12, 42_877.0, 1_200_000, 99_402, 18_220, 0),
        s("SELECT pg_sleep($1), count(*) FROM dataman_activity_demo.orders", 4, 40_011.3, 4, 1_802, 0, 0),
        s("WITH t AS (SELECT customer, sum(total) s FROM orders GROUP BY 1) SELECT * FROM t WHERE s > $1", 300, 21_660.8, 90_300, 120_410, 60_221, 12_800),
        s("BEGIN", 412_870, 1_204.1, 0, 0, 0, 0),
        s("COMMIT", 412_865, 3_996.0, 0, 0, 0, 0),
    ]
}

/// Builds the scenario and opens the monitor on the stage's section.
pub async fn run(ws: &Entity<Workspace>, stage: &str, id: &str, password: &str, cx: &mut AsyncWindowContext) -> bool {
    let conn = cx.update(|_, cx| ws.read(cx).store.read(cx).connection(id)).ok().flatten().expect("connected");
    if stage == "activity-check" {
        return check(ws, &conn, id, cx).await;
    }
    exec(
        &conn,
        &[
            // An earlier run's outside sessions (a long pg_sleep outlives its client).
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name IN ('billing-worker', 'api-server', 'nightly-report', 'metabase', 'psql') AND pid <> pg_backend_pid()",
            &format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"),
            &format!("CREATE SCHEMA {SCHEMA}"),
            &format!("CREATE TABLE {SCHEMA}.accounts (id int PRIMARY KEY, owner text, balance numeric(12,2))"),
            &format!("INSERT INTO {SCHEMA}.accounts SELECT g, 'owner ' || g, 1000 FROM generate_series(1, 2000) g"),
            &format!("CREATE TABLE {SCHEMA}.orders (id serial PRIMARY KEY, customer int, total numeric(12,2), note text) WITH (autovacuum_enabled = off)"),
            &format!("INSERT INTO {SCHEMA}.orders (customer, total, note) SELECT g % 500, g % 997, repeat('x', 60) FROM generate_series(1, 200000) g"),
            &format!("ANALYZE {SCHEMA}.orders"),
            &format!("DELETE FROM {SCHEMA}.orders WHERE id % 3 = 0"),
            &format!("UPDATE {SCHEMA}.orders SET note = 'y' WHERE id % 7 = 0"),
            &format!("ANALYZE {SCHEMA}.accounts"),
        ],
        cx,
    )
    .await;
    let acct = |sql: &str| sql.replace("{S}", SCHEMA);
    outside(&conn, password, "billing-worker", &["BEGIN", &acct("UPDATE {S}.accounts SET balance = balance - 25 WHERE id = 42")]);
    pause(cx, 300).await;
    outside(&conn, password, "api-server", &[&acct("UPDATE {S}.accounts SET balance = balance + 5 WHERE id = 42")]);
    pause(cx, 200).await;
    outside(&conn, password, "api-server", &[&acct("UPDATE {S}.accounts SET owner = 'renamed' WHERE id = 42")]);
    outside(&conn, password, "nightly-report", &[&acct("SELECT pg_sleep(600), count(*) FROM {S}.orders")]);
    outside(&conn, password, "metabase", &["SELECT 1"]);
    pause(cx, 2500).await;
    outside(&conn, password, "psql", &["BEGIN", &acct("SELECT sum(balance) FROM {S}.accounts")]);

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_activity(id, window, cx)));
    let Some(m) = monitor_of(ws, cx) else { return false };
    let _ = cx.update(|_, cx| m.update(cx, |m, cx| m.set_thresholds(Thresholds { query_warn: 3, query_err: 600, idle_tx_warn: 2, idle_tx_err: 6 }, cx)));
    let m2 = m.clone();
    until(cx, 10_000, move |cx| m2.read(cx).avail[0] == crate::activity::Avail::Yes && m2.read(cx).shown_sessions().len() >= 5).await;
    pause(cx, 4000).await;
    let section = match stage {
        "activity-locks" | "activity-terminate" => Section::Locks,
        "activity-tables" => Section::Tables,
        "activity-statements" | "activity-statements-state" => Section::Statements,
        _ => Section::Sessions,
    };
    let _ = cx.update(|window, cx| m.update(cx, |m, cx| m.set_section(section, window, cx)));
    if stage == "activity-statements-state" {
        pause(cx, 1200).await;
        return true;
    }
    if section == Section::Statements && !matches!(cx.update(|_, cx| m.read(cx).statements_state().cloned()).ok().flatten(), Some(Statements::Rows(r)) if !r.is_empty()) {
        let _ = cx.update(|_, cx| m.update(cx, |m, cx| m.demo_statements(sample_statements(), cx)));
    }
    pause(cx, 1200).await;
    let _ = cx.update(|window, cx| {
        m.update(cx, |m, cx| match section {
            Section::Sessions => {
                let pid = m.shown_sessions().iter().find(|s| s.state_kind() == State::Active && !s.blocked_by.is_empty()).map(|s| s.pid);
                m.select_pid(pid, cx);
                cx.emit(crate::activity::ActivityEvent::Details { toggle: false });
            }
            Section::Locks => {
                let pid = m.tree().first().map(|r| r.pid);
                m.select_pid(pid, cx);
                if stage == "activity-terminate" {
                    m.request_signal(Signal::Terminate, cx);
                }
            }
            Section::Tables => {
                let grid = m.tables_grid.clone();
                let row = m.tables().iter().position(|t| t.name == "orders");
                if let Some(r) = row {
                    grid.update(cx, |g, cx| g.select_cell(r, 1, cx));
                }
            }
            Section::Statements => {
                let grid = m.statements_grid.clone();
                grid.update(cx, |g, cx| g.select_cell(2, 0, cx));
                cx.emit(crate::activity::ActivityEvent::Details { toggle: false });
            }
        });
        let f = m.read(cx).focus_target(cx);
        window.focus(&f, cx);
    });
    pause(cx, 600).await;
    true
}

/// `activity-check`.
async fn check(ws: &Entity<Workspace>, conn: &Arc<Connection>, id: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    use std::sync::atomic::Ordering::Relaxed;
    let renders = || crate::app::RENDERS.load(Relaxed);
    let _ = cx.update(|_, cx| ws.update(cx, |w, _| w.assume_active = Some(true)));

    // Opened from the action (View ▸ Activity Monitor, the palette): a tab on the connection.
    // (The focus first: the action goes up from the focused element.)
    let _ = cx.update(|window, cx| {
        ws.update(cx, |w, cx| w.focus_editor(window, cx));
        window.refresh();
        window.draw(cx).clear(cx);
        window.dispatch_action(Box::new(crate::app::OpenActivity), cx);
    });
    let opened = until(cx, 3000, |cx| ws.read(cx).active_tab().is_some_and(|t| t.read(cx).monitor.is_some())).await;
    let Some(m) = monitor_of(ws, cx) else {
        report("the monitor opens in a tab", false, String::new());
        return false;
    };
    let title = cx.update(|_, cx| ws.read(cx).active_tab().unwrap().read(cx).title(cx).to_string()).unwrap();
    report("OpenActivity opens \"Activity · <connection>\"", opened && title == format!("Activity · {}", conn.profile.name), title);
    let m2 = m.clone();
    let probed = until(cx, 10_000, move |cx| m2.read(cx).avail.iter().all(|a| *a != crate::activity::Avail::Unknown)).await;
    let avail = cx.update(|_, cx| m.read(cx).avail).unwrap();
    report("PostgreSQL reports sessions, locks and tables", probed && avail[..3].iter().all(|a| *a == crate::activity::Avail::Yes), format!("{avail:?}"));
    let st = cx.update(|_, cx| m.read(cx).statements_state().cloned()).unwrap();
    report("pg_stat_statements: its state or its rows", matches!(st, Some(Statements::NotInstalled | Statements::NotLoaded | Statements::Rows(_))), format!("{st:?}"));
    // Opening it again goes to the same tab.
    let tabs = cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap();
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_activity(id, window, cx)));
    let tabs2 = cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap();
    report("a second open goes to the same tab", tabs == tabs2, format!("{tabs} → {tabs2}"));

    // The default filter, and this app's backends.
    let shown = cx.update(|_, cx| m.read(cx).shown_sessions().to_vec()).unwrap();
    report("idle and background sessions are hidden by default", !shown.is_empty() && shown.iter().all(|s| s.shown_by_default()), format!("{} shown", shown.len()));
    let own_mark = cx
        .update(|_, cx| {
            let g = m.read(cx).sessions_grid.read(cx);
            let snap = g.result()?.snapshot();
            let row = (0..snap.rows()).find(|&r| m.read(cx).shown_sessions().get(snap.source_row(r)).is_some_and(|s| s.is_self))?;
            Some(snap.text(row, 0).map(|s| s.into_owned()))
        })
        .ok()
        .flatten();
    report("the monitor's own backend is listed", own_mark.is_some(), String::new());
    let _ = cx.update(|_, cx| m.update(cx, |m, cx| m.set_filters(true, true, cx)));
    let all = cx.update(|_, cx| m.read(cx).shown_sessions().len()).unwrap();
    report("the toggles show idle and background sessions", all > shown.len(), format!("{all} vs {}", shown.len()));
    let _ = cx.update(|_, cx| m.update(cx, |m, cx| m.set_filters(false, false, cx)));

    // A blocking scenario: a row lock held in an open transaction, a session waiting on it.
    exec(conn, &[
        &format!("DROP SCHEMA IF EXISTS {SCHEMA}_check CASCADE"),
        &format!("CREATE SCHEMA {SCHEMA}_check"),
        &format!("CREATE TABLE {SCHEMA}_check.acct (id int PRIMARY KEY, balance int)"),
        &format!("INSERT INTO {SCHEMA}_check.acct VALUES (1, 100)"),
    ], cx)
    .await;
    let holder = conn.session(None);
    for sql in ["BEGIN", &format!("UPDATE {SCHEMA}_check.acct SET balance = 1 WHERE id = 1")] {
        let rs = holder.execute(sql);
        let rs2 = rs.clone();
        until(cx, 5000, move |_| rs2.phase().is_finished()).await;
    }
    let waiter = conn.session(None);
    let warm = waiter.execute("SELECT 1");
    until(cx, 5000, move |_| warm.phase().is_finished()).await;
    let blocked = waiter.execute(format!("UPDATE {SCHEMA}_check.acct SET balance = 2 WHERE id = 1"));
    let (hp, wp) = (holder.backend_pid().unwrap_or(0), waiter.backend_pid().unwrap_or(0));
    let _ = cx.update(|window, cx| m.update(cx, |m, cx| m.set_section(Section::Locks, window, cx)));
    let m2 = m.clone();
    let tree_ok = until(cx, 8000, move |cx| {
        let t = m2.read(cx).tree();
        t.iter().position(|r| r.pid == hp && r.depth == 0).is_some_and(|i| t.get(i + 1).is_some_and(|w| w.pid == wp && w.depth == 1))
    })
    .await;
    report("the blocking tree: the holder on top, the waiter under it", tree_ok, format!("{hp} / {wp}"));

    // Cancel the waiter through the sheet (a ⌘↩ in the first 350 ms does not count).
    let _ = cx.update(|_, cx| m.update(cx, |m, cx| {
        m.select_pid(Some(wp), cx);
        m.request_signal(Signal::Cancel, cx);
    }));
    pause(cx, 100).await;
    let sheet = cx.update(|_, cx| ws.read(cx).active_tab().is_some_and(|t| matches!(t.read(cx).confirm, Some(crate::tab::TabConfirm::Monitor(_))))).unwrap();
    report("Cancel asks first (the sheet)", sheet, String::new());
    pause(cx, 400).await;
    key(cx, "cmd-enter");
    let b2 = blocked.clone();
    let cancelled = until(cx, 5000, move |_| b2.phase().is_finished()).await && blocked.phase() == Phase::Cancelled;
    report("⌘↩ cancels the waiter's statement", cancelled, format!("{:?}", blocked.phase()));

    // Terminate the holder: the sheet marks it as this app's; esc keeps it, ⌘↩ ends it.
    let _ = cx.update(|_, cx| m.update(cx, |m, cx| {
        m.select_pid(Some(hp), cx);
        m.request_signal(Signal::Terminate, cx);
    }));
    pause(cx, 100).await;
    let own = cx
        .update(|_, cx| {
            ws.read(cx).active_tab().and_then(|t| match &t.read(cx).confirm {
                Some(crate::tab::TabConfirm::Monitor(crate::activity::Pending::Signal { own, signal, .. })) => Some((*own, *signal)),
                _ => None,
            })
        })
        .ok()
        .flatten();
    report("Terminate of this app's backend says so on the sheet", own == Some((true, Signal::Terminate)), format!("{own:?}"));
    key(cx, "escape");
    pause(cx, 300).await;
    let kept = holder.tx().status == TxStatus::Open && cx.update(|_, cx| ws.read(cx).active_tab().is_some_and(|t| t.read(cx).confirm.is_none())).unwrap();
    report("esc closes the sheet and sends nothing", kept, String::new());
    let _ = cx.update(|_, cx| m.update(cx, |m, cx| m.request_signal(Signal::Terminate, cx)));
    pause(cx, 450).await;
    key(cx, "cmd-enter");
    let gone = until(cx, 6000, |_| holder.tx().status == TxStatus::Idle).await;
    let m2 = m.clone();
    let gone_from_list = until(cx, 6000, move |cx| m2.read(cx).session(hp).is_none()).await;
    report("⌘↩ terminates the holder (its transaction rolls back)", gone && gone_from_list, String::new());
    drop((holder, waiter));
    exec(conn, &[&format!("DROP SCHEMA {SCHEMA}_check CASCADE")], cx).await;

    // Explain of a session's query: a new tab with the plan.
    let _ = cx.update(|window, cx| m.update(cx, |m, cx| m.set_section(Section::Sessions, window, cx)));
    pause(cx, 300).await;
    let tabs_before = cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap();
    let _ = cx.update(|_, cx| {
        m.update(cx, |_, cx| cx.emit(crate::activity::ActivityEvent::OpenSql { sql: "SELECT count(*) FROM pg_class;".into(), explain: true }))
    });
    let planned = until(cx, 8000, |cx| ws.read(cx).active_tab().is_some_and(|t| t.read(cx).shows_plan(cx))).await;
    let tabs_after = cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap();
    report("Explain opens the query in a new tab with its plan", planned && tabs_after == tabs_before + 1, String::new());

    // Hidden: another tab in front. No reads, no frames.
    let mon_ix = cx.update(|_, cx| ws.read(cx).tabs.iter().position(|t| t.read(cx).monitor.is_some())).unwrap().unwrap();
    // The notices fade and the new tab's cursor stops blinking first (6 s after its text).
    pause(cx, 7000).await;
    let counts = |cx: &mut AsyncWindowContext| cx.update(|_, cx| (m.read(cx).refreshes, m.read(cx).renders)).unwrap();
    let ((r0, v0), f0) = (counts(cx), renders());
    let ticking = cx.update(|_, cx| m.read(cx).ticking()).unwrap();
    pause(cx, 5000).await;
    let ((r1, v1), f1) = (counts(cx), renders());
    report(
        "hidden behind another tab: 0 reads, 0 renders, 0 frames in 5 s",
        !ticking && r1 == r0 && v1 == v0 && f1 == f0,
        format!("{} reads, {} renders, {} frames, timer {ticking}", r1 - r0, v1 - v0, f1 - f0),
    );

    // Back in front: it reads at once.
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.activate(mon_ix, window, cx)));
    let m2 = m.clone();
    let back = until(cx, 2000, move |cx| m2.read(cx).refreshes > r1).await;
    report("back in front, it reads at once", back && cx.update(|_, cx| m.read(cx).ticking()).unwrap(), String::new());

    // On screen with nothing changing: reads every second, no frame.
    let _ = cx.update(|_, cx| m.update(cx, |m, cx| m.set_every(Every::S1, cx)));
    pause(cx, 2500).await;
    let (r0, f0, d0) = (cx.update(|_, cx| m.read(cx).refreshes).unwrap(), renders(), cx.update(|_, cx| m.read(cx).redraws).unwrap());
    pause(cx, 5000).await;
    let (r1, f1, d1) = (cx.update(|_, cx| m.read(cx).refreshes).unwrap(), renders(), cx.update(|_, cx| m.read(cx).redraws).unwrap());
    report(
        "on screen, nothing changing: a read a second, 0 frames",
        r1 >= r0 + 4 && f1 == f0 && d1 == d0,
        format!("{} reads, {} frames, {} redraws", r1 - r0, f1 - f0, d1 - d0),
    );

    // The window inactive: the timer stops.
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| {
        w.assume_active = Some(false);
        w.sync_monitors(window, cx);
    }));
    let r0 = cx.update(|_, cx| m.read(cx).refreshes).unwrap();
    pause(cx, 3000).await;
    let r1 = cx.update(|_, cx| m.read(cx).refreshes).unwrap();
    report("the window inactive: no reads", r1 == r0 && !cx.update(|_, cx| m.read(cx).ticking()).unwrap(), format!("{} reads", r1 - r0));
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| {
        w.assume_active = Some(true);
        w.sync_monitors(window, cx);
    }));

    // Off: no timer.
    let _ = cx.update(|_, cx| m.update(cx, |m, cx| m.set_every(Every::Off, cx)));
    let r0 = cx.update(|_, cx| m.read(cx).refreshes).unwrap();
    pause(cx, 3000).await;
    let r1 = cx.update(|_, cx| m.read(cx).refreshes).unwrap();
    report("refresh off: no reads", r1 == r0 && !cx.update(|_, cx| m.read(cx).ticking()).unwrap(), format!("{} reads", r1 - r0));
    // ⌘R still reads.
    let f = cx.update(|_, cx| m.read(cx).focus_target(cx)).unwrap();
    let _ = cx.update(|window, cx| window.focus(&f, cx));
    key(cx, "cmd-r");
    let m2 = m.clone();
    report("⌘R reads now", until(cx, 2000, move |cx| m2.read(cx).refreshes > r1).await, String::new());

    // Closing the tab ends the monitor (its timer goes with it).
    let weak = m.downgrade();
    drop(m);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.close_tab(mon_ix, window, cx)));
    pause(cx, 300).await;
    report("closing the tab drops the monitor", cx.update(|_, _| weak.upgrade().is_none()).unwrap(), String::new());
    ok
}
