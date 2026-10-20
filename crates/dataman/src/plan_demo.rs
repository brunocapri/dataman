use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Focusable, Keystroke};
use dataman_core::TxStatus;
use dataman_core::explain::{Format, Rule};
use dataman_core::result::Phase;

use crate::app::Workspace;
use crate::plan::Mode;
use crate::tab::QueryTab;

pub fn is_stage(s: &str) -> bool {
    matches!(s, "plan" | "plan-flame" | "plan-details" | "plan-check")
}

pub const SQL: &str = "-- revenue by country, from orders whose total is a multiple of 97
SELECT c.country, c.email, count(*) AS orders, sum(p.total) AS revenue
FROM orders p
JOIN customer c ON c.id = p.customer
WHERE p.total % 97 = 0
GROUP BY c.country, c.email
ORDER BY c.email;";

fn key(cx: &mut AsyncWindowContext, k: &str) {
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(Keystroke::parse(k).unwrap(), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn typ(cx: &mut AsyncWindowContext, s: &str) {
    for c in s.chars() {
        key(cx, &c.to_string());
    }
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

fn set(tab: &Entity<QueryTab>, text: &str, at: usize, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        let editor = tab.read(cx).editor.clone();
        editor.update(cx, |e, cx| {
            e.set_text(text, cx);
            e.set_cursor(at, cx);
        });
        window.focus(&editor.focus_handle(cx), cx);
    });
}

async fn run_sql(tab: &Entity<QueryTab>, sql: &str, cx: &mut AsyncWindowContext) -> Option<String> {
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.run(vec![sql.to_string()], cx)));
    let (t2, want) = (tab.clone(), sql.to_string());
    until(cx, 15_000, move |cx| {
        let t = t2.read(cx);
        !t.is_running() && t.result().is_some_and(|r| r.sql == want && r.phase().is_finished())
    })
    .await;
    cx.update(|_, cx| {
        let snap = tab.read(cx).result()?.snapshot();
        (!snap.columns.is_empty() && snap.rows() > 0).then(|| snap.text(0, 0).map(|s| s.into_owned())).flatten()
    })
    .unwrap_or(None)
}

async fn plan_up(tab: &Entity<QueryTab>, analyzed: bool, cx: &mut AsyncWindowContext) -> bool {
    let t2 = tab.clone();
    until(cx, 20_000, move |cx| {
        let t = t2.read(cx);
        t.shows_plan(cx) && t.plan.read(cx).plan().is_some_and(|p| p.analyzed == analyzed)
    })
    .await
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, cx: &mut AsyncWindowContext) -> bool {
    let tab = cx.update(|_, cx| ws.read(cx).active_tab().unwrap().clone()).unwrap();
    if stage == "plan-check" {
        return check(&tab, cx).await;
    }
    let _ = cx.update(|_, cx| {
        ws.update(cx, |w, cx| {
            w.editor_frac = Some(0.22);
            cx.notify();
        })
    });
    run_sql(&tab, "SET work_mem = '64kB'", cx).await;
    set(&tab, SQL, SQL.find("SELECT").unwrap(), cx);
    key(cx, "cmd-alt-l");
    plan_up(&tab, true, cx).await;
    let _ = cx.update(|window, cx| {
        let plan = tab.read(cx).plan.clone();
        match stage {
            "plan-flame" => plan.update(cx, |p, cx| p.set_mode(Mode::Flame, cx)),
            "plan-details" => {
                let node = plan.read(cx).plan().and_then(|p| p.hints.iter().find(|h| h.rule == Rule::Spill).or(p.hints.first()).map(|h| h.node));
                if let Some(n) = node {
                    plan.update(cx, |p, cx| p.select(n, cx));
                }
            }
            _ => {}
        }
        window.focus(&plan.focus_handle(cx), cx);
    });
    true
}

async fn check(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let plan = cx.update(|_, cx| tab.read(cx).plan.clone()).unwrap();
    let info = |cx: &mut AsyncWindowContext| {
        cx.update(|_, cx| {
            plan.read(cx).plan().map(|p| (p.format, p.analyzed, p.nodes.len(), p.totals.execution_ms, p.hints.iter().map(|h| h.rule).collect::<Vec<_>>()))
        })
        .unwrap()
    };

    set(tab, SQL, SQL.find("FROM").unwrap(), cx);
    key(cx, "cmd-l");
    let up = plan_up(tab, false, cx).await;
    let i = info(cx);
    report(
        "⌘L shows the plan of the statement under the cursor",
        up && i.as_ref().is_some_and(|i| i.0 == Format::Json && !i.1 && i.2 >= 3 && i.3.is_none()),
        format!("{i:?}"),
    );
    let text = cx.update(|_, cx| tab.read(cx).editor.read(cx).text().to_string()).unwrap();
    report("the editor keeps the statement as typed", text == SQL, text);

    key(cx, "cmd-shift-g");
    let focused = cx.update(|window, cx| plan.focus_handle(cx).is_focused(window)).unwrap();
    report("⌘⇧G focuses the plan where the grid was", focused, String::new());
    let _ = cx.update(|_, cx| plan.update(cx, |p, cx| p.select(0, cx)));
    key(cx, "down");
    let s = cx.update(|_, cx| plan.read(cx).selected()).unwrap();
    report("↓ selects the next node", s == Some(1), format!("{s:?}"));
    key(cx, "up");
    key(cx, "left");
    let collapsed = cx.update(|_, cx| plan.read(cx).visible_rows()).unwrap();
    key(cx, "right");
    let open = cx.update(|_, cx| plan.read(cx).visible_rows()).unwrap();
    let n = i.as_ref().map_or(0, |i| i.2);
    report("← folds the root, → opens it", collapsed == 1 && open == n, format!("{collapsed} / {open} of {n}"));

    run_sql(tab, "SET work_mem = '64kB'", cx).await;
    set(tab, SQL, SQL.find("JOIN").unwrap(), cx);
    key(cx, "cmd-alt-l");
    let up = plan_up(tab, true, cx).await;
    let i = info(cx);
    report(
        "⌥⌘L runs it: actual times, execution time, spill hints",
        up && i.as_ref().is_some_and(|i| i.1 && i.3.is_some() && i.4.iter().any(|r| matches!(r, Rule::Spill | Rule::HashBatches))),
        format!("{i:?}"),
    );
    let shares = cx.update(|_, cx| plan.read(cx).plan().map(|p| p.nodes.iter().map(|n| n.share).sum::<f64>())).unwrap();
    report("the bars' shares add up to the whole", shares.is_some_and(|s| (s - 1.).abs() < 1e-6), format!("{shares:?}"));
    let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::app::TogglePlanFlame), cx));
    let mode = cx.update(|_, cx| (plan.read(cx).mode, plan.read(cx).has_flame())).unwrap();
    report("the flame view toggles", mode == (Mode::Flame, true), format!("{mode:?}"));
    let _ = cx.update(|_, cx| plan.update(cx, |p, cx| p.set_mode(Mode::Tree, cx)));
    run_sql(tab, "RESET work_mem", cx).await;

    run_sql(tab, "CREATE TEMP TABLE IF NOT EXISTS plan_t AS SELECT g AS id FROM generate_series(1, 100) g", cx).await;
    let del = "DELETE FROM plan_t WHERE id <= 50";
    set(tab, del, 0, cx);
    key(cx, "cmd-alt-l");
    let up = plan_up(tab, true, cx).await;
    let (op, rolled) = cx
        .update(|_, cx| {
            let t = tab.read(cx);
            (plan.read(cx).plan().map(|p| p.nodes[0].op.clone()), t.result().is_some_and(|r| r.rolled_back()))
        })
        .unwrap();
    let count = run_sql(tab, "SELECT count(*) FROM plan_t", cx).await;
    let idle = cx.update(|_, cx| tab.read(cx).tx().status == TxStatus::Idle).unwrap();
    report(
        "Explain Analyze of a DELETE runs rolled back: no row gone, no transaction left",
        up && op.as_deref() == Some("Delete") && rolled && count.as_deref() == Some("100") && idle,
        format!("{op:?} rolled={rolled} count={count:?} idle={idle}"),
    );

    run_sql(tab, "BEGIN", cx).await;
    run_sql(tab, "INSERT INTO plan_t VALUES (1000)", cx).await;
    set(tab, "DELETE FROM plan_t WHERE id > 0", 0, cx);
    key(cx, "cmd-alt-l");
    let up = plan_up(tab, true, cx).await;
    let count = run_sql(tab, "SELECT count(*) FROM plan_t", cx).await;
    let open = cx.update(|_, cx| tab.read(cx).tx().status == TxStatus::Open).unwrap();
    report(
        "inside an open transaction it rolls back to a savepoint: the INSERT stays, the transaction too",
        up && count.as_deref() == Some("101") && open,
        format!("count={count:?} open={open}"),
    );
    let end = cx.update(|_, cx| tab.update(cx, |t, cx| t.end_tx(false, cx))).unwrap();
    let _ = end.await;

    let q = "SELECT * FROM orders WHERE id = :id";
    set(tab, q, 0, cx);
    key(cx, "cmd-l");
    let t2 = tab.clone();
    let asked = until(cx, 3_000, move |cx| t2.read(cx).params.is_some()).await;
    typ(cx, "42");
    key(cx, "cmd-enter");
    let up = plan_up(tab, false, cx).await;
    let bound = cx
        .update(|_, cx| {
            let t = tab.read(cx);
            (t.result().map(|r| (r.params().to_vec(), r.sql.contains("$1"))), plan.read(cx).plan().map(|p| p.nodes[0].title.clone()))
        })
        .unwrap();
    report(
        "a :name parameter opens the prompt and is bound to the EXPLAIN",
        asked && up && bound.0 == Some((vec![Some("42".to_string())], true)) && bound.1.as_deref() == Some("Index Scan using orders_pkey on orders"),
        format!("asked={asked} {bound:?}"),
    );

    let saved = cx.update(|_, cx| cx.read_from_clipboard()).unwrap();
    let _ = cx.update(|_, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(String::new())));
    let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::app::CopyPlanText), cx));
    until(cx, 5_000, |cx| cx.read_from_clipboard().and_then(|c| c.text()).is_some_and(|t| !t.is_empty())).await;
    let copied = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text())).unwrap().unwrap_or_default();
    if let Some(s) = saved {
        let _ = cx.update(|_, cx| cx.write_to_clipboard(s));
    }
    report(
        "Copy as text puts PostgreSQL's text plan on the clipboard",
        copied.starts_with("Index Scan using orders_pkey on public.orders") && copied.contains("(cost=") && copied.contains("Index Cond: (orders.id = '42'::bigint)"),
        copied,
    );

    let big: String = (1..=300).map(|i| format!("SELECT {i} AS n, count(*) FROM customer WHERE id < {i}")).collect::<Vec<_>>().join("\nUNION ALL ");
    set(tab, &big, 0, cx);
    key(cx, "cmd-l");
    let up = plan_up(tab, false, cx).await;
    let nodes = info(cx).map_or(0, |i| i.2);
    key(cx, "cmd-shift-g");
    let _ = cx.update(|_, cx| plan.update(cx, |p, cx| p.select(0, cx)));
    let t0 = std::time::Instant::now();
    for _ in 0..60 {
        key(cx, "down");
    }
    let per_key = t0.elapsed() / 60;
    let _ = cx.update(|_, cx| plan.update(cx, |p, cx| p.set_mode(Mode::Flame, cx)));
    let t1 = std::time::Instant::now();
    for _ in 0..10 {
        key(cx, "down");
    }
    let per_flame = t1.elapsed() / 10;
    let _ = cx.update(|_, cx| plan.update(cx, |p, cx| p.set_mode(Mode::Tree, cx)));
    println!("big plan: {nodes} nodes, ↓ + frame {per_key:?} (tree), {per_flame:?} (flame)");
    report(
        "a plan of 500+ nodes: ↓ and its frame under 8 ms in the tree and the flame",
        up && nodes >= 500 && per_key < Duration::from_millis(8) && per_flame < Duration::from_millis(8),
        format!("{nodes} nodes, {per_key:?} / {per_flame:?}"),
    );

    let bad = "SELECT * FROM orders WHERE nope = 1";
    set(tab, bad, 0, cx);
    key(cx, "cmd-l");
    let t2 = tab.clone();
    until(cx, 5_000, move |cx| t2.read(cx).result().is_some_and(|r| r.phase() == Phase::Failed) && t2.read(cx).failed_index().is_some()).await;
    let (plan_shown, at) = cx
        .update(|_, cx| {
            let t = tab.read(cx);
            (t.shows_plan(cx), t.editor.read(cx).diagnostic().and_then(|d| d.at))
        })
        .unwrap();
    report(
        "a failing explain shows the error, at the right place in the editor",
        !plan_shown && at == bad.find("nope"),
        format!("plan={plan_shown} at={at:?}"),
    );
    ok
}
