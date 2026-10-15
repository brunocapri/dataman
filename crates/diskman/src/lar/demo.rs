//! Demo stages of the live queries for `--demo` screenshots (`DISKMAN_SHOT_STAGE`), run against
//! the LAR in `DISKMAN_BENCH_PG` with the `demo_orders` table (seeded by
//! `seeds/lar-live-demo.py`). Each stage drives the real UI: runs the editor's SUBSCRIBE, opens
//! the Live panel and writes to the server so the grid moves.
//!
//! `lar-live`, `lar-live-ended`, `lar-live-empty`, `lar-live-burst`; with `-light` in the light
//! theme.

use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AsyncWindowContext, Entity, Window};
use diskman_core::Session;

use crate::app::Workspace;

const SUBSCRIBE: &str = "-- a delta every time the result changes: ts, diff, columns\nSUBSCRIBE SELECT id, demo_customer, status, total, updated_at\n          FROM demo_orders\n          WHERE id >= 49190;";

/// A plain query: the empty Live panel offers to SUBSCRIBE to its table.
const ORDERS: &str = "-- newest orders\nSELECT id, demo_customer, status, total, updated_at\nFROM   demo_orders\nWHERE  id >= 49190\nORDER  BY id DESC;";

/// The editor text of a stage (`None`: not a LAR stage).
pub fn sql(stage: &str) -> Option<String> {
    Some(match stage {
        "lar-live-empty" => ORDERS.to_string(),
        "lar-live" | "lar-live-ended" | "lar-live-burst" => SUBSCRIBE.to_string(),
        _ => return None,
    })
}

/// How long the stage needs before the capture.
pub fn wait_ms(stage: &str) -> u64 {
    match stage {
        "lar-live" => 2500,
        "lar-live-burst" => 7000,
        "lar-live-ended" => 3000,
        s if s.starts_with("lar-") => 1500,
        _ => 0,
    }
}

/// Runs a LAR stage (after the editor's statement ran). Returns false for other stages.
pub fn stage(stage: &str, ws: &Entity<Workspace>, window: &mut Window, cx: &mut App) -> bool {
    if !stage.starts_with("lar-") {
        return false;
    }
    // The "LAR detected" toast would cover the panel.
    ws.update(cx, |w, _| w.lar_toast = None);
    let ws = ws.clone();
    let stage = stage.to_string();
    window
        .spawn(cx, async move |cx| {
            let r = run(&stage, ws, cx).await;
            if let Err(e) = r {
                eprintln!("demo {stage}: {e}");
            }
        })
        .detach();
    true
}

async fn sleep(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

/// Runs one statement to completion in the stage's own session (the writes that make the grid
/// move), reporting failures.
async fn exec(session: &Arc<Session>, sql: &str) {
    let rs = session.execute(sql);
    rs.wait_finished().await;
    if let Some(e) = rs.status().error {
        eprintln!("demo: {sql}: {}", e.message);
    }
}

async fn run(stage: &str, ws: Entity<Workspace>, cx: &mut AsyncWindowContext) -> Result<(), String> {
    let e = |e: anyhow::Error| e.to_string();
    let session = cx
        .update(|_, cx| ws.read(cx).active_tab().and_then(|t| t.read(cx).connection().cloned()))
        .map_err(e)?
        .ok_or("no connection")?
        .session(None);
    match stage {
        "lar-live-burst" => {
            // ~200 deltas/s for 3 s: 60 commits of 10 rows, one every 50 ms.
            exec(&session, "DELETE FROM demo_orders WHERE id >= 50000").await;
            sleep(cx, 500).await;
            for c in 0..60u64 {
                let rows: Vec<String> = (0..10u64).map(|r| format!("({}, 1001, 'new', {}, now(), now())", 50000 + c * 10 + r, r)).collect();
                exec(&session, &format!("INSERT INTO demo_orders VALUES {}", rows.join(", "))).await;
                sleep(cx, 50).await;
            }
            sleep(cx, 1500).await;
            exec(&session, "DELETE FROM demo_orders WHERE id >= 50000").await;
        }
        "lar-live" | "lar-live-ended" => {
            exec(&session, "DELETE FROM demo_orders WHERE id >= 49300").await;
            exec(&session, "UPDATE demo_orders SET status = 'new', total = 84.9 WHERE id = 49200").await;
            cx.update(|window, cx| ws.update(cx, |w, cx| w.toggle_server_panel("live", window, cx))).map_err(e)?;
            if stage == "lar-live" {
                // A second tab listening (JSON notifications), then back to the SUBSCRIBE tab.
                cx.update(|window, cx| {
                    ws.update(cx, |w, cx| {
                        let first = w.active;
                        let tab = w.new_tab(window, cx);
                        let sql = "LISTEN big_orders AS\n  SELECT id, total FROM demo_orders WHERE total > 1000 AND id >= 49190;";
                        tab.update(cx, |t, cx| {
                            t.editor.update(cx, |e, cx| e.set_text(sql, cx));
                            t.run(vec![sql.trim_end_matches(';').to_string()], cx);
                        });
                        w.activate(first, window, cx);
                    })
                })
                .map_err(e)?;
            }
            sleep(cx, 900).await;
            for i in 0..14u64 {
                exec(&session, &format!("INSERT INTO demo_orders VALUES ({}, {}, 'new', {}, now(), now())", 49300 + i, 1000 + i * 7, 40 + i * 97)).await;
                sleep(cx, 120).await;
            }
            exec(&session, "UPDATE demo_orders SET status = 'paid', updated_at = now() WHERE id = 49200").await;
            sleep(cx, 150).await;
            exec(&session, "DELETE FROM demo_orders WHERE id = 49302").await;
            sleep(cx, 150).await;
            exec(&session, "UPDATE demo_orders SET status = 'paid', total = 1290, updated_at = now() WHERE id = 49306").await;
            exec(&session, "INSERT INTO demo_orders VALUES (49399, 1042, 'new', 2210, now(), now())").await;
            if stage == "lar-live-ended" {
                sleep(cx, 300).await;
                cx.update(|_, cx| {
                    if let Some(l) = ws.read(cx).active_tab().and_then(|t| t.read(cx).lar.live().cloned()) {
                        l.update(cx, |l, cx| l.stop(cx));
                    }
                })
                .map_err(e)?;
            }
        }
        "lar-live-empty" => {
            cx.update(|window, cx| ws.update(cx, |w, cx| w.toggle_server_panel("live", window, cx))).map_err(e)?;
        }
        _ => {}
    }
    Ok(())
}
