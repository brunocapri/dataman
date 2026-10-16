//! Demo stages of a table's data view (`--demo`), against the demo database: `public.orders`
//! (a million rows) and `dataman_demo.tickets` (enum, boolean, reference, timestamptz, date,
//! jsonb, long text; created from `seeds/table.sql` at the start).
//!
//! - `table`: `orders` filtered (`status = paid`), sorted by `total` descending (screenshot);
//! - `table-filter`: the filter popover on `total` (screenshot);
//! - `table-menu`: `created_at` hidden, the header menu open (screenshot);
//! - `json` / `datetime` / `enum`: the grid's value editors on `meta`, `opened_at`, `state`
//!   (screenshots);
//! - `table-check`: the rules with real mouse and key events (the editor's SQL, server-side sort
//!   by a header click, pages with ⌥⌘→, filters, the free-form WHERE, the count against the
//!   server, hide from the header menu, a JSON edit through the change set, the SQL
//!   edited by hand), printed as PASS/FAIL lines; exit code 1 on a failure.

use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Focusable, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput, Pixels, Point, point, px};
use dataman_core::dataview::{Filter, Op};
use dataman_core::result::{Phase, SortDir};
use dataman_core::value::Kind;

use crate::app::Workspace;
use crate::tab::QueryTab;

pub const SEED: &str = include_str!("../../../seeds/table.sql");

/// The stages this module drives.
pub fn is_stage(s: &str) -> bool {
    s.starts_with("table") || matches!(s, "json" | "datetime" | "enum")
}

fn input(cx: &mut AsyncWindowContext, e: PlatformInput) {
    let _ = cx.update(|window, cx| {
        window.dispatch_event(e, cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn key(cx: &mut AsyncWindowContext, k: &str) {
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(Keystroke::parse(k).unwrap(), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn click(cx: &mut AsyncWindowContext, p: Point<Pixels>) {
    let m = Modifiers::default();
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: p, modifiers: m, click_count: 1, first_mouse: false }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: p, modifiers: m, click_count: 1 }));
}

/// The `n`-th click of a sequence at `p` (double, triple…): the clicks before it already landed.
fn click_n(cx: &mut AsyncWindowContext, p: Point<Pixels>, n: usize) {
    let m = Modifiers::default();
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: p, modifiers: m, click_count: n, first_mouse: false }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: p, modifiers: m, click_count: n }));
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

/// Runs the seed on the active tab's connection (statement by statement).
async fn seed(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) {
    run_seed(ws, SEED, cx).await;
}

/// Runs a seed script on the active tab's connection (statement by statement).
pub(crate) async fn run_seed(ws: &Entity<Workspace>, script: &str, cx: &mut AsyncWindowContext) {
    let Some(conn) = cx.update(|_, cx| ws.read(cx).active_tab().and_then(|t| t.read(cx).connection().cloned())).ok().flatten() else { return };
    let session = conn.session(None);
    for s in dataman_core::sql::split(script) {
        let sql = script[s.range].trim().to_string();
        if sql.is_empty() {
            continue;
        }
        let rs = session.execute(sql);
        for _ in 0..200 {
            if rs.phase().is_finished() {
                break;
            }
            pause(cx, 25).await;
        }
        if rs.phase() != Phase::Done {
            eprintln!("seed: {:?}", rs.status().error);
        }
    }
}

/// Opens `schema.table` as data (as the explorer's ↩ does) and waits for its first page.
async fn open(ws: &Entity<Workspace>, conn: &str, db: &str, schema: &str, table: &str, cx: &mut AsyncWindowContext) -> Entity<QueryTab> {
    let _ = cx.update(|window, cx| {
        ws.update(cx, |w, cx| {
            w.open_relation(conn, db, schema, table, window, cx);
            // The empty console the window opened with.
            if w.tabs.len() > 1 && w.tabs[0].read(cx).result().is_none() {
                w.close_tab(0, window, cx);
                let last = w.tabs.len() - 1;
                w.activate(last, window, cx);
            }
        })
    });
    let tab = cx.update(|_, cx| ws.read(cx).active_tab().unwrap().clone()).unwrap();
    page(&tab, cx).await;
    tab
}

/// Waits until the tab shows a finished page of its table.
async fn page(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
    pause(cx, 50).await;
    for _ in 0..400 {
        let done = cx.update(|_, cx| {
            let t = tab.read(cx);
            t.shows_table() && t.result().is_some_and(|r| r.phase().is_finished()) && !t.is_running()
        });
        if done.unwrap_or(true) {
            break;
        }
        pause(cx, 25).await;
    }
    draw(cx);
}

fn col(tab: &Entity<QueryTab>, name: &str, cx: &mut AsyncWindowContext) -> usize {
    cx.update(|_, cx| tab.read(cx).result().and_then(|r| r.columns()).and_then(|c| c.iter().position(|c| c.name == name)))
        .ok()
        .flatten()
        .unwrap_or(0)
}

fn sql(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> String {
    cx.update(|_, cx| tab.read(cx).editor.read(cx).text().to_string()).unwrap_or_default()
}

/// Runs a stage; `false` when a check failed.
pub async fn run(ws: &Entity<Workspace>, stage: &str, conn: &str, db: &str, cx: &mut AsyncWindowContext) -> bool {
    let tickets = matches!(stage, "json" | "datetime" | "enum" | "table-check");
    if tickets {
        seed(ws, cx).await;
    }
    match stage {
        "table" | "table-filter" | "table-menu" => {
            let tab = open(ws, conn, db, "public", "orders", cx).await;
            let total = col(&tab, "total", cx);
            let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.add_filter(Filter::new("status", Kind::Text, Op::Eq, "paid"), None, cx)));
            page(&tab, cx).await;
            let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.table_sort(total, Some(SortDir::Desc), cx)));
            page(&tab, cx).await;
            // The count of the filtered rows.
            for _ in 0..200 {
                if cx.update(|_, cx| tab.read(cx).table.as_ref().is_some_and(|d| d.counting.is_none())).unwrap_or(true) {
                    break;
                }
                pause(cx, 50).await;
            }
            let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
            match stage {
                "table" => {
                    let _ = cx.update(|window, cx| {
                        grid.update(cx, |g, cx| g.select_cells(3..=3, total, cx));
                        window.focus(&grid.focus_handle(cx), cx);
                    });
                }
                "table-filter" => {
                    let at = cx.update(|_, cx| grid.read(cx).cell_bounds(0, total)).ok().flatten().unwrap_or_default();
                    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_filter(total, None, (at.0, at.1 - 2.), window, cx)));
                    let pop = cx.update(|_, cx| ws.read(cx).filter_pop.clone()).ok().flatten();
                    if let Some((p, _)) = pop {
                        let _ = cx.update(|window, cx| {
                            p.update(cx, |p, cx| {
                                p.set_op(Op::Gt, window, cx);
                                p.set_value("5000", cx);
                            })
                        });
                    }
                }
                _ => {
                    let created_at = col(&tab, "created_at", cx);
                    let address = col(&tab, "address", cx);
                    let _ = cx.update(|window, cx| {
                        grid.update(cx, |g, cx| {
                            g.hide_column(created_at, cx);
                            g.select_cells(2..=2, address, cx);
                        });
                        window.focus(&grid.focus_handle(cx), cx);
                    });
                    draw(cx);
                    let _ = cx.update(|window, cx| grid.update(cx, |g, cx| g.demo_header_menu(address, window, cx)));
                }
            }
            true
        }
        "json" | "datetime" | "enum" => {
            let tab = open(ws, conn, db, "dataman_demo", "tickets", cx).await;
            let c = col(&tab, match stage { "json" => "meta", "datetime" => "opened_at", _ => "state" }, cx);
            let (grid, edit) = cx.update(|_, cx| (tab.read(cx).grid.clone(), tab.read(cx).edit.clone())).unwrap();
            let _ = cx.update(|window, cx| {
                grid.update(cx, |g, cx| g.select_cells(1..=1, c, cx));
                window.focus(&grid.focus_handle(cx), cx);
                edit.update(cx, |e, cx| e.demo_edit(1, c, window, cx));
            });
            pause(cx, 800).await;
            if stage == "json"
                && let Some(v) = cx.update(|_, cx| edit.read(cx).demo_value_editor()).ok().flatten()
            {
                let _ = cx.update(|window, cx| v.update(cx, |v, cx| v.demo_edit_first_leaf(window, cx)));
            }
            true
        }
        _ => check(ws, conn, db, cx).await,
    }
}

async fn check(ws: &Entity<Workspace>, conn: &str, db: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let tab = open(ws, conn, db, "public", "orders", cx).await;
    let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
    let s = sql(&tab, cx);
    report("the editor holds the page's SELECT", s == "SELECT * FROM public.orders\n ORDER BY id\n LIMIT 1000;", s.clone());
    let (title, mode, rows) = cx
        .update(|_, cx| {
            let t = tab.read(cx);
            (t.title(cx).to_string(), t.grid.read(cx).table_mode().cloned(), t.result().map_or(0, |r| r.rows()))
        })
        .unwrap();
    report("the tab is named after the table", title == "orders", title);
    report("the grid is in table mode at offset 0", mode.as_ref().is_some_and(|m| m.offset == 0 && m.sort.is_none()), format!("{mode:?}"));
    report("the page has 1,000 rows", rows == 1000, rows.to_string());
    let est = cx.update(|_, cx| tab.read(cx).table.as_ref().and_then(|d| d.estimate)).ok().flatten();
    report("the estimate comes from reltuples", est.is_some_and(|n| n > 900_000), format!("{est:?}"));

    // A header click sorts on the server (asc, then desc).
    let total = col(&tab, "total", cx);
    let p = cx.update(|_, cx| grid.read(cx).demo_header_point(total)).unwrap();
    click(cx, p);
    page(&tab, cx).await;
    let s = sql(&tab, cx);
    report("a header click sorts on the server", s.contains("\n ORDER BY total\n"), s.clone());
    click(cx, p);
    page(&tab, cx).await;
    let s = sql(&tab, cx);
    let first = cx.update(|_, cx| tab.read(cx).result().and_then(|r| r.snapshot().text(0, total).map(|v| v.into_owned()))).ok().flatten();
    report("a second click sorts descending", s.contains("ORDER BY total DESC"), s.clone());
    let max = cx.update(|_, cx| tab.read(cx).grid.read(cx).table_mode().and_then(|m| m.sort)).ok().flatten();
    report("the header shows the server's sort", max == Some((total, SortDir::Desc)), format!("{max:?}"));
    let _ = first;

    // ⌥⌘→: the next page.
    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
    key(cx, "cmd-alt-right");
    page(&tab, cx).await;
    let s = sql(&tab, cx);
    let off = cx.update(|_, cx| grid.read(cx).table_mode().map(|m| m.offset)).ok().flatten();
    report("⌥⌘→ goes to the next page", s.ends_with("LIMIT 1000 OFFSET 1000;") && off == Some(1000), format!("{s} {off:?}"));
    key(cx, "cmd-alt-left");
    page(&tab, cx).await;
    report("⌥⌘← goes back", sql(&tab, cx).ends_with("LIMIT 1000;"), sql(&tab, cx));

    // Filters: by value; back to the first page.
    let status = col(&tab, "status", cx);
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.table_page(3, cx)));
    page(&tab, cx).await;
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.filter_value(status, Some("paid".into()), cx)));
    page(&tab, cx).await;
    let s = sql(&tab, cx);
    report("filter by value builds the WHERE", s.contains("\n WHERE status = 'paid'\n") && !s.contains("OFFSET"), s.clone());
    let all_paid = cx
        .update(|_, cx| {
            tab.read(cx).result().map(|r| {
                let snap = r.snapshot();
                (0..snap.rows()).all(|i| snap.text(i, status).as_deref() == Some("paid"))
            })
        })
        .ok()
        .flatten();
    report("the server filtered the rows", all_paid == Some(true), format!("{all_paid:?}"));

    // The count against a direct count(*).
    let mut total_rows = None;
    for _ in 0..400 {
        total_rows = cx.update(|_, cx| tab.read(cx).table.as_ref().filter(|d| d.counting.is_none()).and_then(|d| d.total)).ok().flatten();
        if total_rows.is_some() {
            break;
        }
        pause(cx, 50).await;
    }
    let conn_ = cx.update(|_, cx| tab.read(cx).connection().cloned()).ok().flatten();
    let expect = match conn_ {
        Some(c) => {
            let rs = c.session(None).execute("SELECT count(*) FROM public.orders WHERE status = 'paid'");
            for _ in 0..400 {
                if rs.phase().is_finished() {
                    break;
                }
                pause(cx, 25).await;
            }
            rs.snapshot().text(0, 0).and_then(|v| v.parse::<u64>().ok())
        }
        None => None,
    };
    report("the count matches count(*)", total_rows.is_some() && total_rows == expect, format!("{total_rows:?} vs {expect:?}"));

    // ⌘. stops a count.
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.start_count(cx)));
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.cancel(cx)));
    let stopped = cx.update(|_, cx| tab.read(cx).table.as_ref().map(|d| (d.counting.is_none(), d.stopped.is_some()))).ok().flatten();
    report("⌘. stops the count", stopped == Some((true, true)), format!("{stopped:?}"));

    // Hide from the header menu (a right click on the header).
    let created_at = col(&tab, "created_at", cx);
    let p = cx.update(|_, cx| grid.read(cx).demo_header_point(created_at)).unwrap();
    let m = Modifiers::default();
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Right, position: p, modifiers: m, click_count: 1, first_mouse: false }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Right, position: p, modifiers: m, click_count: 1 }));
    let labels = cx.update(|_, cx| grid.read(cx).demo_menu().map(|m| m.1)).ok().flatten().unwrap_or_default();
    report("the header menu has Hide column", labels.iter().any(|l| l == "Hide column") && labels.iter().any(|l| l == "Filter…"), format!("{labels:?}"));
    let _ = cx.update(|window, cx| grid.update(cx, |g, cx| g.demo_menu_run("Hide column", window, cx)));
    draw(cx);
    let order = cx.update(|_, cx| grid.read(cx).display_order()).unwrap();
    report("hiding removes the column", !order.contains(&created_at), format!("{order:?}"));
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.table_step(true, cx)));
    page(&tab, cx).await;
    let order2 = cx.update(|_, cx| grid.read(cx).display_order()).unwrap();
    report("the next page keeps the hidden column", order2 == order, format!("{order2:?}"));

    // The inline cell editor (edit_ui/input.rs): double click selects the whole value, then
    // word, line and (⌘Z/⇧⌘Z) undo rules shared with every text field in the app. `customer` is
    // a plain integer column (not the key, not JSON): the text field, not a value editor.
    let customer = col(&tab, "customer", cx);
    let edit = cx.update(|_, cx| tab.read(cx).edit.clone()).unwrap();
    let p = cx.update(|_, cx| grid.read(cx).demo_cell_point(2, customer)).unwrap();
    click_n(cx, p, 1);
    click_n(cx, p, 2);
    draw(cx);
    // The target (table, key) resolves async on the tab's first edit.
    let mut editor = None;
    for _ in 0..40 {
        editor = cx.update(|_, cx| edit.read(cx).demo_cell_editor()).ok().flatten();
        if editor.is_some() {
            break;
        }
        pause(cx, 50).await;
        draw(cx);
    }
    report("a double click opens the cell editor", editor.is_some(), String::new());
    if let Some(editor) = editor {
        let text = cx.update(|_, cx| editor.read(cx).input.clone()).unwrap();
        let orig_len = cx.update(|_, cx| text.read(cx).text().len()).unwrap();
        let sel = cx.update(|_, cx| text.read(cx).demo_selected()).unwrap();
        report("…with the whole value selected", sel == (0..orig_len), format!("{sel:?} vs 0..{orig_len}"));

        // Typing over the selection replaces it (one undo step covers all of it).
        let _ = cx.update(|window, cx| window.focus(&text.focus_handle(cx), cx));
        for c in ["f", "o", "o", "space", "b", "a", "r"] {
            key(cx, c);
        }
        let got = cx.update(|_, cx| text.read(cx).text().to_string()).unwrap();
        report("typing replaces the selection", got == "foo bar", got.clone());

        key(cx, "alt-left");
        let at = cx.update(|_, cx| text.read(cx).demo_selected()).unwrap();
        report("⌥← stops at the start of the word", at == (4..4), format!("{at:?}"));
        key(cx, "alt-left");
        let at = cx.update(|_, cx| text.read(cx).demo_selected()).unwrap();
        report("⌥← again goes to the start of the text", at == (0..0), format!("{at:?}"));
        key(cx, "alt-shift-right");
        let at = cx.update(|_, cx| text.read(cx).demo_selected()).unwrap();
        report("⌥⇧→ selects to the end of the word", at == (0..3), format!("{at:?}"));
        key(cx, "cmd-right");
        let at = cx.update(|_, cx| text.read(cx).demo_selected()).unwrap();
        report("⌘→ goes to the end of the line", at == (7..7), format!("{at:?}"));
        key(cx, "cmd-shift-left");
        let at = cx.update(|_, cx| text.read(cx).demo_selected()).unwrap();
        report("⌘⇧← selects to the start of the line", at == (0..7), format!("{at:?}"));
        key(cx, "cmd-right");
        key(cx, "alt-backspace");
        let got = cx.update(|_, cx| text.read(cx).text().to_string()).unwrap();
        report("⌥⌫ deletes the word before the cursor", got == "foo ", got.clone());
        key(cx, "cmd-z");
        let got = cx.update(|_, cx| text.read(cx).text().to_string()).unwrap();
        report("⌘Z undoes it", got == "foo bar", got.clone());
        key(cx, "cmd-shift-z");
        let got = cx.update(|_, cx| text.read(cx).text().to_string()).unwrap();
        report("⇧⌘Z redoes it", got == "foo ", got.clone());
        key(cx, "cmd-z");
        key(cx, "cmd-a");
        let at = cx.update(|_, cx| text.read(cx).demo_selected()).unwrap();
        report("⌘A selects everything", at == (0..7), format!("{at:?}"));

        // Double and triple clicks inside the field: a word, then the whole value.
        let b = cx.update(|_, cx| text.read(cx).demo_bounds()).ok().flatten().unwrap_or_default();
        let left = point(px(b.0 + 2.), px(b.1 + b.3 / 2.));
        let right = point(px(b.0 + b.2 - 2.), px(b.1 + b.3 / 2.));
        click_n(cx, left, 1);
        click_n(cx, left, 2);
        let at = cx.update(|_, cx| text.read(cx).demo_selected()).unwrap();
        report("a double click near the start selects the first word", at == (0..3), format!("{at:?}"));
        click_n(cx, right, 1);
        click_n(cx, right, 2);
        let at = cx.update(|_, cx| text.read(cx).demo_selected()).unwrap();
        report("a double click near the end selects the last word", at == (4..7), format!("{at:?}"));
        click_n(cx, left, 3);
        let at = cx.update(|_, cx| text.read(cx).demo_selected()).unwrap();
        report("a triple click selects the whole value", at == (0..7), format!("{at:?}"));

        key(cx, "escape");
        draw(cx);
        let pending = cx.update(|_, cx| edit.read(cx).pending_count()).unwrap_or(0);
        report("escape cancels the edit, nothing pending", pending == 0, pending.to_string());
    }

    // A JSON edit through the change set (the tickets table, editable by its key).
    let t2 = open(ws, conn, db, "dataman_demo", "tickets", cx).await;
    let meta = col(&t2, "meta", cx);
    let (g2, e2) = cx.update(|_, cx| (t2.read(cx).grid.clone(), t2.read(cx).edit.clone())).unwrap();
    let _ = cx.update(|window, cx| {
        window.focus(&g2.focus_handle(cx), cx);
        e2.update(cx, |e, cx| e.demo_edit(0, meta, window, cx));
    });
    pause(cx, 800).await;
    draw(cx);
    let v = cx.update(|_, cx| e2.read(cx).demo_value_editor()).ok().flatten();
    report("a jsonb cell opens the JSON editor", v.is_some(), String::new());
    if let Some(v) = v {
        let _ = cx.update(|window, cx| v.update(cx, |v, cx| v.demo_edit_first_leaf(window, cx)));
        draw(cx);
        // Type over the selected leaf, ↩ keeps it, ⌘↩ applies.
        for c in "sms".chars() {
            key(cx, &c.to_string());
        }
        key(cx, "enter");
        key(cx, "cmd-enter");
        draw(cx);
        let cur = cx.update(|_, cx| e2.read(cx).current_text(0, meta)).ok().flatten().flatten();
        let pending = cx.update(|_, cx| e2.read(cx).pending_count()).unwrap_or(0);
        let valid = cur.as_deref().is_some_and(|c| dataman_core::jsontree::parse(c).is_ok() && c.contains("\"sms\""));
        report("the JSON edit lands in the change set", pending == 1 && valid, format!("{pending} {cur:?}"));
        let plan = cx.update(|_, cx| e2.read(cx).plan()).ok();
        let plan_sql = plan.and_then(|p| p.ok()).map(|p| dataman_core::edit::preview(&p)).unwrap_or_default();
        report("the commit path is the usual UPDATE", plan_sql.contains("UPDATE dataman_demo.tickets SET meta = '") && plan_sql.contains("\"sms\""), plan_sql.clone());
        let _ = cx.update(|_, cx| e2.update(cx, |e, cx| e.discard_all(cx)));
    }

    // Edited by hand: a plain query tab.
    let editor = cx.update(|_, cx| t2.read(cx).editor.clone()).unwrap();
    let _ = cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            let end = e.text().len();
            e.set_cursor(end, cx);
            e.type_text(" ", window, cx);
        })
    });
    draw(cx);
    let (plain, mode) = cx.update(|_, cx| (t2.read(cx).table.is_none(), t2.read(cx).grid.read(cx).table_mode().is_none())).unwrap();
    report("editing the SQL by hand makes a plain query tab", plain && mode, format!("{plain} {mode}"));
    ok
}
