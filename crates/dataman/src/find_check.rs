use std::time::{Duration, Instant};

use gpui::{AsyncWindowContext, Entity, Focusable};
use dataman_core::find::FindOptions;

use crate::app::Workspace;
use crate::editor::CodeEditor;
use crate::find::FindBarEvent;
use crate::grid::ResultGrid;

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
        key(cx, &c.to_string());
    }
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn parts(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> (Entity<CodeEditor>, Entity<ResultGrid>) {
    cx.update(|_, cx| {
        let t = ws.read(cx).active_tab().unwrap().read(cx);
        (t.editor.clone(), t.grid.clone())
    })
    .unwrap()
}

async fn rows_loaded(grid: &Entity<ResultGrid>, cx: &mut AsyncWindowContext) {
    for _ in 0..100 {
        if cx.update(|_, cx| grid.read(cx).result().is_some_and(|r| r.rows() > 0 && r.phase() != dataman_core::Phase::Running)).unwrap_or(false) {
            return;
        }
        pause(cx, 100).await;
    }
}

async fn grid_settled(grid: &Entity<ResultGrid>, cx: &mut AsyncWindowContext) {
    for _ in 0..100 {
        draw(cx);
        if !cx.update(|_, cx| grid.read(cx).demo_find(cx).is_some_and(|f| f.4)).unwrap_or(true) {
            return;
        }
        pause(cx, 50).await;
    }
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, cx: &mut AsyncWindowContext) -> bool {
    let (editor, grid) = parts(ws, cx);
    rows_loaded(&grid, cx).await;
    match stage {
        "find" => {
            let _ = cx.update(|window, cx| {
                window.focus(&editor.focus_handle(cx), cx);
                editor.update(cx, |e, cx| e.set_cursor(0, cx));
            });
            key(cx, "cmd-alt-f");
            typ(cx, "p.");
            key(cx, "enter");
            key(cx, "tab");
            typ(cx, "o.");
            pause(cx, 250).await;
            true
        }
        "find-grid" => {
            let _ = cx.update(|window, cx| {
                window.focus(&grid.focus_handle(cx), cx);
                grid.update(cx, |g, cx| g.select_cell(0, 0, cx));
            });
            key(cx, "cmd-f");
            typ(cx, "shipped");
            grid_settled(&grid, cx).await;
            key(cx, "enter");
            pause(cx, 250).await;
            true
        }
        _ => check(ws, &editor, &grid, cx).await,
    }
}

fn expect(grid: &Entity<ResultGrid>, rows: usize, q: &str, cx: &mut AsyncWindowContext) -> Vec<(u32, u32)> {
    cx.update(|_, cx| {
        let snap = grid.read(cx).result().unwrap().snapshot();
        let n = snap.columns.len();
        (0..rows)
            .flat_map(|r| (0..n).map(move |c| (r, c)))
            .filter(|&(r, c)| match snap.text(r, c) {
                Some(v) => v.to_lowercase().contains(q),
                None => q == "null",
            })
            .map(|(r, c)| (r as u32, c as u32))
            .collect()
    })
    .unwrap()
}

async fn check(ws: &Entity<Workspace>, editor: &Entity<CodeEditor>, grid: &Entity<ResultGrid>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let ed = |cx: &mut AsyncWindowContext| {
        cx.update(|window, cx| {
            let e = editor.read(cx);
            let (open, label) = e.demo_find(cx).map_or((false, String::new()), |f| (f.1, f.2));
            (open, label, e.find_has_focus(window, cx), e.focus_handle(cx).is_focused(window), e.demo_selection(), e.text().to_string())
        })
        .unwrap()
    };
    let original = cx.update(|_, cx| editor.read(cx).text().to_string()).unwrap();
    let first_p = original.find("p.").unwrap();
    let _ = cx.update(|window, cx| {
        window.focus(&editor.focus_handle(cx), cx);
        editor.update(cx, |e, cx| e.set_cursor(0, cx));
    });

    key(cx, "cmd-f");
    let (open, _, field, ..) = ed(cx);
    report("⌘F opens the editor's bar on its field", open && field, format!("open {open} field {field}"));
    typ(cx, "p.");
    let (_, label, _, _, sel, _) = ed(cx);
    report("typing finds every match; the first from the cursor is selected", label == "1/6" && sel == (first_p..first_p + 2), format!("{label} {sel:?}"));
    key(cx, "enter");
    let l1 = ed(cx).1;
    key(cx, "shift-enter");
    let l2 = ed(cx).1;
    key(cx, "shift-enter");
    let l3 = ed(cx).1;
    key(cx, "cmd-g");
    let l4 = ed(cx).1;
    report("↩ / ⇧↩ / ⌘G step through the matches, around the ends", (l1.as_str(), l2.as_str(), l3.as_str(), l4.as_str()) == ("2/6", "1/6", "6/6", "1/6"), format!("{l1} {l2} {l3} {l4}"));
    key(cx, "escape");
    let (open, _, _, focused, ..) = ed(cx);
    report("esc closes the bar and gives the focus back to the editor", !open && focused, format!("open {open} focused {focused}"));
    key(cx, "cmd-shift-g");
    let to_grid = cx.update(|window, cx| grid.focus_handle(cx).is_focused(window)).unwrap();
    report("⇧⌘G with the bar closed still focuses the grid", to_grid, String::new());
    let _ = cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    key(cx, "cmd-g");
    let (open, label, field, focused, ..) = ed(cx);
    report("⌘G with the bar closed reopens it on the last query, the focus stays", open && label == "2/6" && focused && !field, format!("{open} {label} {focused} {field}"));

    key(cx, "cmd-alt-f");
    key(cx, "tab");
    typ(cx, "o.");
    let bar = cx.update(|_, cx| editor.read(cx).demo_find(cx).map(|f| f.0)).unwrap().unwrap();
    let _ = cx.update(|_, cx| bar.update(cx, |_, cx| cx.emit(FindBarEvent::ReplaceAll)));
    draw(cx);
    let (_, label, _, _, _, text) = ed(cx);
    report("Replace All replaces every match", !text.contains("p.") && text.matches("o.").count() == 6 && label == "0/0", format!("{label} {text:?}"));
    key(cx, "escape");
    key(cx, "cmd-z");
    let text = ed(cx).5;
    report("one ⌘Z undoes the whole Replace All", text == original, format!("{text:?}"));

    let _ = cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.open_find(true, window, cx));
        bar.update(cx, |b, cx| {
            b.set_options(FindOptions { regex: true, ..Default::default() }, cx);
            b.set_query(r"p\.(\w+)", cx);
            b.set_replacement("$1_p", cx);
            cx.emit(FindBarEvent::Changed);
        });
    });
    draw(cx);
    let label = ed(cx).1;
    let _ = cx.update(|_, cx| bar.update(cx, |_, cx| cx.emit(FindBarEvent::ReplaceAll)));
    draw(cx);
    let text = ed(cx).5;
    report("a regular expression replaces with $1", label.ends_with("/6") && text.contains("id_p, status_p") && !text.contains("p."), format!("{label} {text:?}"));
    key(cx, "escape");
    key(cx, "cmd-z");
    report("…and undoes in one step", ed(cx).5 == original, String::new());

    let big: String = original.lines().cycle().take(2000).collect::<Vec<_>>().join("\n");
    let _ = cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.set_text(&big, cx));
        window.focus(&editor.focus_handle(cx), cx);
    });
    let t = Instant::now();
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(crate::keymap::keystroke("x"), cx);
    });
    let closed_ms = t.elapsed().as_secs_f64() * 1e3;
    key(cx, "backspace");
    key(cx, "escape");
    let t0 = Instant::now();
    let _ = cx.update(|window, cx| {
        bar.update(cx, |b, cx| {
            b.set_options(FindOptions::default(), cx);
            b.set_query("p.", cx);
        });
        editor.update(cx, |e, cx| e.open_find(false, window, cx));
    });
    let open_ms = t0.elapsed().as_secs_f64() * 1e3;
    let label = ed(cx).1;
    let _ = cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    let t1 = Instant::now();
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(crate::keymap::keystroke("x"), cx);
    });
    let key_ms = t1.elapsed().as_secs_f64() * 1e3;
    println!("     2,000 lines: search {open_ms:.2} ms ({label}); a key {closed_ms:.2} ms with the bar closed, {key_ms:.2} ms open");
    report("a 2,000-line script searches and edits fast", open_ms < crate::check::budget(Duration::from_millis(8)).as_secs_f64() * 1e3 && key_ms < crate::check::budget(Duration::from_millis(4)).as_secs_f64() * 1e3, format!("{open_ms:.2} {key_ms:.2}"));
    key(cx, "escape");
    key(cx, "escape");
    let _ = cx.update(|_, cx| editor.update(cx, |e, cx| e.set_text(&original, cx)));

    let _ = cx.update(|window, cx| {
        window.focus(&grid.focus_handle(cx), cx);
        grid.update(cx, |g, cx| g.select_cell(0, 0, cx));
    });
    key(cx, "cmd-f");
    typ(cx, "shipped");
    grid_settled(grid, cx).await;
    let st = cx.update(|_, cx| grid.read(cx).demo_find(cx)).unwrap();
    let Some((open, label, hits, covered, _)) = st else {
        report("⌘F opens the grid's bar", false, String::new());
        println!("SOME CHECKS FAILED");
        return false;
    };
    let expected = expect(grid, covered, "shipped", cx);
    report(
        "the grid finds exactly the loaded cells that match",
        open && covered > 0 && hits == expected && !hits.is_empty(),
        format!("open {open} covered {covered} hits {} expected {}", hits.len(), expected.len()),
    );
    let total = hits.len();
    let active = |cx: &mut AsyncWindowContext| cx.update(|_, cx| grid.read(cx).demo_selection().map(|s| s.1)).unwrap();
    report("the first match from the active cell becomes active", active(cx) == Some((hits[0].0 as usize, hits[0].1 as usize)) && label == format!("1/{}", crate::grid::ui_count(total)), format!("{label} {:?}", active(cx)));
    key(cx, "enter");
    let a1 = active(cx);
    key(cx, "shift-enter");
    key(cx, "shift-enter");
    let a2 = active(cx);
    let last = hits[total - 1];
    report(
        "↩ / ⇧↩ move the active cell through the matches, around the ends",
        a1 == Some((hits[1].0 as usize, hits[1].1 as usize)) && a2 == Some((last.0 as usize, last.1 as usize)),
        format!("{a1:?} {a2:?}"),
    );
    key(cx, "escape");
    let (open, focused) = cx.update(|window, cx| (grid.read(cx).demo_find(cx).is_some_and(|f| f.0), grid.focus_handle(cx).is_focused(window))).unwrap();
    report("esc closes the grid's bar; the grid keeps the focus", !open && focused, format!("open {open} focused {focused}"));
    key(cx, "cmd-f");
    typ(cx, "null");
    grid_settled(grid, cx).await;
    let (hits, covered) = cx.update(|_, cx| grid.read(cx).demo_find(cx).map(|f| (f.2, f.3))).unwrap().unwrap();
    let nothing = cx.update(|_, cx| grid.read(cx).result().unwrap().columns().unwrap().iter().position(|c| c.name == "nothing")).unwrap();
    let nulls = nothing.map_or(0, |n| hits.iter().filter(|h| h.1 as usize == n).count());
    report(
        "a NULL query finds the NULL cells (and text with \"null\" in it)",
        nulls == covered && hits == expect(grid, covered, "null", cx),
        format!("{nulls} NULL of {covered} rows, {} cells", hits.len()),
    );
    key(cx, "escape");
    let id = cx.update(|_, cx| grid.read(cx).result().unwrap().columns().unwrap().iter().position(|c| c.name == "id")).unwrap().unwrap_or(0);
    let _ = cx.update(|_, cx| {
        grid.update(cx, |g, cx| {
            g.hide_column(id, cx);
            g.select_cell(0, 0, cx);
        })
    });
    key(cx, "cmd-f");
    typ(cx, "1");
    grid_settled(grid, cx).await;
    let (hits, order) = cx.update(|_, cx| (grid.read(cx).demo_find(cx).map(|f| f.2).unwrap_or_default(), grid.read(cx).display_order())).unwrap();
    let pos = |c: u32| order.iter().position(|&x| x == c as usize).unwrap_or(usize::MAX);
    let sorted = hits.windows(2).all(|w| (w[0].0, pos(w[0].1)) < (w[1].0, pos(w[1].1)));
    report(
        "find skips a hidden column and follows the display order",
        !hits.is_empty() && hits.iter().all(|h| h.1 as usize != id) && sorted,
        format!("{} hits, sorted {sorted}", hits.len()),
    );
    key(cx, "enter");
    key(cx, "enter");
    let landed = active(cx);
    report("stepping lands on shown cells only", landed.is_some_and(|(_, c)| c < order.len()), format!("{landed:?}"));
    key(cx, "escape");
    let _ = cx.update(|_, cx| grid.update(cx, |g, cx| g.show_all_columns(cx)));
    let _ = ws;
    println!("{}", if ok { "ALL PASS" } else { "SOME CHECKS FAILED" });
    ok
}
