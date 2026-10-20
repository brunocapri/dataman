use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AsyncWindowContext, Entity, Focusable, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels,
    Point, point, px,
};
use dataman_core::batch::{Key, State};
use dataman_core::result::{PREFETCH, Phase, SortDir};

use crate::app::Workspace;
use crate::tab::QueryTab;

pub const SQL: &str = "-- weekly review
SELECT p.id, p.customer, p.status, p.total FROM orders p ORDER BY p.id;
CREATE TEMP TABLE IF NOT EXISTS recent AS SELECT id, status, total FROM orders WHERE id <= 500;
SET work_mem = '64MB';
UPDATE recent SET status = 'reviewed' WHERE id <= 3;
SELECT c.id, c.name, c.city, c.country FROM customer c ORDER BY c.id LIMIT 200;
SELECT count(*) FROM ordrs;
SELECT 1;";

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

fn tab(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Entity<QueryTab> {
    cx.update(|_, cx| ws.read(cx).active_tab().unwrap().clone()).unwrap()
}

async fn finished(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
    for _ in 0..300 {
        let done = cx
            .update(|_, cx| {
                let t = tab.read(cx);
                !t.is_running() && t.run_phase().is_some_and(|p| p.is_finished())
            })
            .unwrap_or(true);
        if done {
            return;
        }
        pause(cx, 100).await;
    }
}

fn names(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> Vec<String> {
    cx.update(|_, cx| {
        tab.read(cx)
            .result_tabs()
            .iter()
            .map(|t| format!("{}{}", t.number.map(|n| format!("{n} · ")).unwrap_or_default(), t.label))
            .collect()
    })
    .unwrap()
}

fn shown(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> Option<Key> {
    cx.update(|_, cx| tab.read(cx).shown_key()).unwrap()
}

fn select(tab: &Entity<QueryTab>, k: Key, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        tab.update(cx, |t, cx| t.select_result(k, cx));
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, cx: &mut AsyncWindowContext) -> bool {
    let tab = tab(ws, cx);
    finished(&tab, cx).await;
    match stage {
        "results" => true,
        "results-grid" => {
            select(&tab, Key::Stmt(0), cx);
            let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
            let _ = cx.update(|window, cx| {
                grid.update(cx, |g, cx| g.select_cells(3..=6, 2, cx));
                window.focus(&grid.focus_handle(cx), cx);
            });
            true
        }
        "results-messages" => {
            select(&tab, Key::Messages, cx);
            true
        }
        _ => check(&tab, cx).await & gutters(ws, cx).await,
    }
}

async fn check(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
    let editor = cx.update(|_, cx| tab.read(cx).editor.clone()).unwrap();

    let n = names(tab, cx);
    report(
        "one tab per result, count and error; SET and CREATE TABLE AS fold into messages",
        n == ["1 · orders", "4 · UPDATE 3", "5 · customer", "6 · ordrs", "messages"],
        format!("{n:?}"),
    );
    let folded = cx.update(|_, cx| tab.read(cx).result_tabs().last().map_or(0, |t| t.folded)).unwrap();
    report("the messages tab counts the folded statements", folded == 2, format!("{folded}"));
    let (k, phase) = cx.update(|_, cx| (tab.read(cx).shown_key(), tab.read(cx).result().map(|r| r.phase()))).unwrap();
    report("the run stops at the error and shows it", k == Some(Key::Stmt(5)) && phase == Some(Phase::Failed), format!("{k:?} {phase:?}"));
    let (not_run, first) = cx
        .update(|_, cx| {
            let t = tab.read(cx);
            let first = t.result_tabs().first().map(|t| t.state);
            (t.batch_items()[6].rs.is_none(), (first, t.result_of(Key::Stmt(0)).map(|r| r.rows())))
        })
        .unwrap();
    report("the statement after the error did not run", not_run, String::new());
    let rows = first.1.unwrap_or(0);
    report(
        "the first result paused before the next statement: it keeps one prefetch window",
        first.0 == Some(State::Partial) && rows >= PREFETCH && rows < 1_000_000,
        format!("{:?} {rows}", first.0),
    );

    let _ = cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    key(cx, "cmd-alt-[");
    let a = shown(tab, cx);
    key(cx, "cmd-alt-]");
    key(cx, "cmd-alt-]");
    let b = shown(tab, cx);
    key(cx, "cmd-alt-]");
    let c = shown(tab, cx);
    report(
        "⌥⌘[ / ⌥⌘] step through the tabs, around the ends",
        (a, b, c) == (Some(Key::Stmt(4)), Some(Key::Messages), Some(Key::Stmt(0))),
        format!("{a:?} {b:?} {c:?}"),
    );
    let messages_grid = {
        select(tab, Key::Messages, cx);
        cx.update(|_, cx| grid.read(cx).result().is_none() && tab.read(cx).result().is_none()).unwrap()
    };
    report("the messages tab leaves no result under copy, export and find", messages_grid, String::new());

    select(tab, Key::Stmt(4), cx);
    let _ = cx.update(|_, cx| grid.update(cx, |g, cx| g.toggle_sort(0, cx)));
    for _ in 0..50 {
        pause(cx, 50).await;
        if cx.update(|_, cx| !grid.read(cx).is_sorting()).unwrap_or(true) {
            break;
        }
    }
    let _ = cx.update(|_, cx| grid.update(cx, |g, cx| g.select_cell(7, 1, cx)));
    select(tab, Key::Stmt(0), cx);
    let other = cx.update(|_, cx| grid.read(cx).demo_selection()).unwrap();
    select(tab, Key::Stmt(4), cx);
    let (sel, sort, same) = cx
        .update(|_, cx| {
            let g = grid.read(cx);
            let rs = g.result().cloned();
            let same = match (&rs, tab.read(cx).result_of(Key::Stmt(4))) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, &b),
                _ => false,
            };
            (g.demo_selection(), rs.and_then(|r| r.sort_state()), same)
        })
        .unwrap();
    report("switching shows that result in the grid", same, String::new());
    report("another result does not share the selection", other != Some(((7, 1), (7, 1))), format!("{other:?}"));
    report(
        "back on a result, its selection and sort are as they were",
        sel == Some(((7, 1), (7, 1))) && sort == Some((0, SortDir::Asc)),
        format!("{sel:?} {sort:?}"),
    );

    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
    key(cx, "cmd-f");
    typ(cx, "a");
    pause(cx, 300).await;
    select(tab, Key::Stmt(0), cx);
    pause(cx, 600).await;
    let (open, hits_ok) = cx
        .update(|_, cx| {
            let g = grid.read(cx);
            let Some((open, _, hits, ..)) = g.demo_find(cx) else { return (false, false) };
            let snap = g.result().unwrap().snapshot();
            let ok = !hits.is_empty()
                && hits.iter().take(200).all(|&(r, c)| {
                    snap.text(r as usize, c as usize).is_some_and(|v| v.to_lowercase().contains('a'))
                });
            (open, ok)
        })
        .unwrap();
    report("find searches the active result after a switch", open && hits_ok, format!("open {open} hits {hits_ok}"));
    let _ = cx.update(|_, cx| grid.update(cx, |g, cx| g.close_find(cx)));

    select(tab, Key::Stmt(4), cx);
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.close_result(Key::Stmt(4), cx)));
    let (n, k) = (names(tab, cx), shown(tab, cx));
    report(
        "closing the result on screen shows the next tab",
        n == ["1 · orders", "4 · UPDATE 3", "6 · ordrs", "messages"] && k == Some(Key::Stmt(5)),
        format!("{n:?} {k:?}"),
    );
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.close_result(Key::Stmt(0), cx)));
    let (n, k) = (names(tab, cx), shown(tab, cx));
    report("closing another tab leaves the one on screen", n.len() == 3 && k == Some(Key::Stmt(5)), format!("{n:?} {k:?}"));
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.close_result(Key::Messages, cx)));
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.close_result(Key::Stmt(3), cx)));
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.close_result(Key::Stmt(5), cx)));
    let (n, k) = (names(tab, cx), shown(tab, cx));
    report("the last tab does not close", n == ["6 · ordrs"] && k == Some(Key::Stmt(5)), format!("{n:?} {k:?}"));
    let _ = cx.update(|_, cx| editor.update(cx, |e, cx| e.run_script(cx)));
    pause(cx, 300).await;
    finished(tab, cx).await;
    let n = names(tab, cx);
    report("the next run has every tab again", n.len() == 5, format!("{n:?}"));

    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.run(vec!["SELECT 1 AS one".into()], cx)));
    pause(cx, 200).await;
    finished(tab, cx).await;
    let n = names(tab, cx);
    report("a single statement has a single tab (the strip is hidden)", n == ["1 · SELECT"], format!("{n:?}"));

    let sql = "SELECT g AS n, CASE WHEN g % 10 = 0 THEN NULL ELSE 'Item' || g END AS t FROM generate_series(1, 300000) g";
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.run(vec![sql.into()], cx)));
    let paused = |cx: &mut AsyncWindowContext| cx.update(|_, cx| tab.read(cx).result().map(|r| r.phase())).ok().flatten() == Some(Phase::Paused);
    for _ in 0..200 {
        if paused(cx) {
            break;
        }
        pause(cx, 50).await;
    }
    let rs0 = cx.update(|_, cx| grid.read(cx).result().cloned()).ok().flatten();
    let rows0 = rs0.as_ref().map_or(0, |r| r.rows());
    let header = |col: usize, cx: &mut AsyncWindowContext| cx.update(|_, cx| grid.read(cx).demo_header_point(col)).unwrap();
    async fn settle(grid: &Entity<crate::grid::ResultGrid>, cx: &mut AsyncWindowContext) {
        for _ in 0..200 {
            pause(cx, 30).await;
            if cx.update(|_, cx| !grid.read(cx).is_sorting()).unwrap_or(true) {
                break;
            }
        }
        draw(cx);
    }
    let col_of = |c: usize, cx: &mut AsyncWindowContext| -> Vec<Option<String>> {
        cx.update(|_, cx| {
            let s = grid.read(cx).result().unwrap().snapshot();
            (0..s.rows()).map(|r| s.text(r, c).map(|v| v.into_owned())).collect()
        })
        .unwrap_or_default()
    };
    let same = |cx: &mut AsyncWindowContext| {
        cx.update(|_, cx| match (grid.read(cx).result(), &rs0) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        })
        .unwrap_or(false)
    };
    let p = header(0, cx);
    click(cx, p);
    settle(&grid, cx).await;
    click(cx, p);
    for _ in 0..400 {
        pause(cx, 50).await;
        let whole = cx
            .update(|_, cx| {
                let g = grid.read(cx);
                g.result().is_some_and(|r| r.phase() == Phase::Done && r.sorted_rows() == Some(r.rows())) && !g.is_sorting()
            })
            .unwrap_or(false);
        if whole {
            break;
        }
    }
    draw(cx);
    let n = col_of(0, cx);
    report(
        "two header clicks on a paused result read the rest and sort it all descending, without a new run",
        rows0 > 0 && rows0 < 300_000 && same(cx) && n.len() == 300_000 && n.first().cloned().flatten().as_deref() == Some("300000"),
        format!("{rows0} rows paused, {} sorted, first {:?}", n.len(), n.first()),
    );
    click(cx, p);
    settle(&grid, cx).await;
    let n = col_of(0, cx);
    let state = cx.update(|_, cx| grid.read(cx).result().and_then(|r| r.sort_state())).ok().flatten();
    report("a third click brings back the server's order", state.is_none() && n.first().cloned().flatten().as_deref() == Some("1") && same(cx), format!("{:?}", n.first()));
    for (label, desc) in [("Sort ascending", false), ("Sort descending", true)] {
        let _ = cx.update(|window, cx| {
            grid.update(cx, |g, cx| {
                g.demo_header_menu(1, window, cx);
                g.demo_menu_run(label, window, cx);
            })
        });
        settle(&grid, cx).await;
        let t = col_of(1, cx);
        let nulls = t.iter().filter(|v| v.is_none()).count();
        let vals: Vec<&str> = t.iter().flatten().map(|s| s.as_str()).collect();
        let ordered = vals.windows(2).all(|w| {
            let o = dataman_core::value::cmp_natural(w[0], w[1]);
            if desc { o.is_ge() } else { o.is_le() }
        });
        let nulls_last = nulls > 0 && t[t.len() - nulls..].iter().all(|v| v.is_none());
        let natural = !desc && vals.iter().take(3).copied().eq(["Item1", "Item2", "Item3"]);
        report(
            &format!("the header menu's {label} sorts the text naturally, NULLs last"),
            ordered && nulls_last && (desc || natural) && same(cx),
            format!("ordered {ordered}, nulls last {nulls_last}, first {:?}", vals.first()),
        );
    }
    let before = cx.update(|_, cx| (grid.read(cx).display_order(), grid.read(cx).result().and_then(|r| r.sort_state()))).unwrap();
    let m = Modifiers::default();
    let p = header(0, cx);
    let to = point(p.x + px(160.), p.y);
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: p, modifiers: m, click_count: 1, first_mouse: false }));
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: point(p.x + px(40.), p.y), pressed_button: Some(MouseButton::Left), modifiers: m }));
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: to, pressed_button: Some(MouseButton::Left), modifiers: m }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: to, modifiers: m, click_count: 1 }));
    settle(&grid, cx).await;
    let after = cx.update(|_, cx| (grid.read(cx).display_order(), grid.read(cx).result().and_then(|r| r.sort_state()))).unwrap();
    report("a header dragged sideways does not move its column (nor sort)", before == after && after.0 == [0, 1], format!("{before:?} → {after:?}"));

    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.run(vec!["SELECT * FROM orders".into()], cx)));
    let mut label = String::new();
    for _ in 0..200 {
        pause(cx, 50).await;
        let got = cx.update(|_, cx| tab.read(cx).result().filter(|r| r.phase() == Phase::Paused && r.estimate_known()).map(|r| crate::chrome::fetch_all_label(Some(&r)).to_string())).ok().flatten();
        if let Some(l) = got {
            label = l;
            break;
        }
    }
    report("a paused result's Fetch all shows the estimate of the rows left", label.starts_with("Fetch all ~"), label.clone());
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.fetch_all(cx)));
    let mut phase = None;
    for _ in 0..400 {
        pause(cx, 50).await;
        phase = cx.update(|_, cx| tab.read(cx).result().map(|r| r.phase())).ok().flatten();
        if phase.is_some_and(|p| p.is_finished()) {
            break;
        }
    }
    report("Fetch all reads the rest", phase == Some(Phase::Done), format!("{phase:?}"));
    ok
}

fn input(cx: &mut AsyncWindowContext, e: PlatformInput) {
    let _ = cx.update(|window, cx| {
        window.dispatch_event(e, cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn click(cx: &mut AsyncWindowContext, p: Point<Pixels>) {
    let m = Modifiers::default();
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: p, modifiers: m, click_count: 1, first_mouse: false }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: p, modifiers: m, click_count: 1 }));
}

fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn divider_xs(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> (f32, f32) {
    cx.update(|_, cx| {
        let t = tab.read(cx);
        (t.editor.read(cx).demo_gutter_divider().unwrap_or(f32::NAN), t.grid.read(cx).demo_gutter_divider())
    })
    .unwrap()
}

/// After two frames: a need the grid learns while laying out reaches the editor a frame later.
fn dividers(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> (f32, f32) {
    draw(cx);
    draw(cx);
    divider_xs(tab, cx)
}

fn aligned((e, g): (f32, f32)) -> bool {
    (e - g).abs() <= 0.5
}

async fn run_in(tab: &Entity<QueryTab>, script: &str, sql: &str, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|_, cx| {
        tab.update(cx, |t, cx| {
            t.editor.update(cx, |e, cx| e.set_text(script, cx));
            t.run(vec![sql.to_string()], cx);
        })
    });
    finished(tab, cx).await;
}

async fn fetch_rest(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> Vec<(f32, f32)> {
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.fetch_all(cx)));
    let mut frames = Vec::new();
    for _ in 0..1000 {
        draw(cx);
        frames.push(divider_xs(tab, cx));
        if cx.update(|_, cx| tab.read(cx).result().is_some_and(|r| r.phase().is_finished())).unwrap() {
            break;
        }
        pause(cx, 10).await;
    }
    frames
}

async fn gutters(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let short = "SELECT g FROM generate_series(1, 9) g;";
    let mut long = String::from("-- a long script\n");
    for i in 0..1500 {
        long.push_str(&format!("SELECT {i};\n"));
    }
    long.push_str(short);
    let nine = "SELECT g FROM generate_series(1, 9) g";
    let many = "SELECT g FROM generate_series(1, 100000) g";

    let tab = cx.update(|window, cx| ws.update(cx, |w, cx| w.new_tab(window, cx))).unwrap();
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.editor.update(cx, |e, cx| e.set_text(short, cx))));
    let before = dividers(&tab, cx).0;
    run_in(&tab, short, nine, cx).await;
    let base = dividers(&tab, cx);
    report("gutters: short script, 9 rows: one divider", aligned(base), format!("{base:?}"));
    report("gutters: the editor's stays put on the first run", (base.0 - before).abs() <= 0.5, format!("{before} → {}", base.0));

    run_in(&tab, &long, nine, cx).await;
    let wide = dividers(&tab, cx);
    report("gutters: 1,500-line script, 9 rows: one divider, wider", aligned(wide) && wide.1 > base.1 + 1., format!("{wide:?} (short {base:?})"));

    run_in(&tab, short, many, cx).await;
    let frames = fetch_rest(&tab, cx).await;
    let rows = cx.update(|_, cx| tab.read(cx).result().map(|r| r.rows())).ok().flatten();
    let all = dividers(&tab, cx);
    report("gutters: short script, 100,000 rows: one divider, wider", rows == Some(100_000) && aligned(all) && all.1 > base.1 + 1., format!("{rows:?} rows, {all:?}"));
    let steady = frames.windows(2).all(|w| w[1].0 >= w[0].0 - 0.01 && w[1].1 >= w[0].1 - 0.01);
    let apart = frames.iter().filter(|f| !aligned(**f)).count();
    report(
        "gutters: while rows stream in, the dividers only grow, together in every frame",
        steady && apart == 0,
        format!("{apart} of {} frames apart: {frames:?}", frames.len()),
    );

    for (code, zoom) in [(16u8, 1.0f32), (12, 1.0), (13, 0.8), (13, 1.5), (16, 2.0)] {
        let _ = cx.update(|window, cx| {
            crate::editor::set_code_size(code);
            crate::theme::set_zoom(zoom, window, cx);
        });
        for (script, label) in [(short, "short"), (long.as_str(), "long")] {
            let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.editor.update(cx, |e, cx| e.set_text(script, cx))));
            let d = dividers(&tab, cx);
            report(&format!("gutters: code {code} pt, zoom {zoom}, {label} script, 100,000 rows: one divider"), aligned(d), format!("{d:?}"));
        }
    }
    let _ = cx.update(|window, cx| {
        crate::editor::set_code_size(crate::theme::text::CODE as u8);
        crate::theme::set_zoom(1.0, window, cx);
    });

    run_in(&tab, &long, nine, cx).await;
    let shown = dividers(&tab, cx);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.toggle_result_zoom(window, cx)));
    pause(cx, 500).await;
    let zoomed = dividers(&tab, cx);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.toggle_result_zoom(window, cx)));
    pause(cx, 500).await;
    let back = dividers(&tab, cx);
    report(
        "gutters: the result zoomed keeps the grid's width; back, one divider",
        aligned(shown) && (zoomed.1 - shown.1).abs() <= 0.5 && aligned(back) && (back.1 - shown.1).abs() <= 0.5,
        format!("{shown:?} → {zoomed:?} → {back:?}"),
    );

    let (conn, db) = cx
        .update(|_, cx| {
            let t = tab.read(cx);
            t.connection().map(|c| (c.profile.id.clone(), t.database().to_string()))
        })
        .unwrap()
        .unwrap();
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_relation(&conn, &db, "public", "orders", window, cx)));
    let data = self::tab(ws, cx);
    finished(&data, cx).await;
    let d = dividers(&data, cx);
    report("gutters: a table's data view: one divider", data != tab && aligned(d), format!("{d:?}"));

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.activate_tab(&tab, window, cx)));
    let _ = cx.update(|window, cx| window.dispatch_keystroke(Keystroke::parse("cmd-\\").unwrap(), cx));
    pause(cx, 500).await;
    let copy = self::tab(ws, cx);
    run_in(&copy, short, many, cx).await;
    fetch_rest(&copy, cx).await;
    let (l, r) = (dividers(&tab, cx), dividers(&copy, cx));
    report(
        "gutters: two editor groups: one divider in each",
        copy != tab && aligned(l) && aligned(r) && r.0 > l.0 + 100.,
        format!("left {l:?}, right {r:?}"),
    );
    ok
}
