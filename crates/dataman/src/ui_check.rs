use std::time::Duration;

use gpui::{
    AsyncWindowContext, Entity, Focusable, KeyDownEvent, KeyUpEvent, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput,
    Pixels, Point, point, px,
};
use dataman_core::result::Phase;

use crate::app::Workspace;
use crate::tab::QueryTab;

pub fn is_stage(s: &str) -> bool {
    s == "ui-check" || s == "picker" || s == "copy-menu" || s == "tip-error" || s == "zoom"
}

async fn tip_error(ws: &Entity<Workspace>, id: &str, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|_, cx| {
        let tree = ws.read(cx).tree.clone();
        tree.update(cx, |t, cx| t.collapse_id(&format!("c:{id}"), cx));
    });
    let long = "could not connect to server: Connection refused\n\tIs the server running on host \"replica-eu-west-1.db.internal\" (10.20.30.40) and accepting TCP/IP connections on port 5432?\nDETAIL: the replica has been unreachable since the last failover; the cluster manager reported the node as fenced, and every retry since then ended the same way, after the full connect timeout, without a single byte from the server.\nHINT: check the host, the port, the VPN and the firewall rules of the subnet. ".repeat(3);
    let failed = cx
        .update(|_, cx| {
            ws.update(cx, |w, cx| {
                let core = w.store.read(cx).core.clone();
                let swatches = ["rose", "amber", "teal", "violet", "sky", "lime"];
                for i in 0..30 {
                    let mut p = dataman_core::ConnectionProfile::new(format!("{}-{:02}", ["staging", "analytics", "billing", "search", "ledger"][i % 5], i + 1));
                    (p.host, p.port, p.color) = ("127.0.0.1".into(), 9, Some(swatches[i % swatches.len()].into()));
                    let _ = core.save_connection(p, None, None);
                }
                let mut p = dataman_core::ConnectionProfile::new("replica-eu-west-1");
                (p.host, p.port) = ("127.0.0.1".into(), 9);
                let id = p.id.clone();
                let _ = core.save_connection(p, None, None);
                w.store.update(cx, |s, cx| {
                    s.reload(cx);
                    s.states.insert(id.clone(), crate::conns::ConnState::Failed(long.clone()));
                });
                id
            })
        })
        .unwrap();
    pause(cx, 400).await;
    draw(cx);
    let b = cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_conn_bounds(&failed)).ok().flatten();
    if let Some(b) = b {
        let p = point(b.left() + px(60.), b.top() + b.size.height / 2.);
        mouse_move(cx, p, false);
        pause(cx, 900).await;
        mouse_move(cx, p, false);
        pause(cx, 300).await;
    }
}

fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn input(cx: &mut AsyncWindowContext, e: PlatformInput) {
    let _ = cx.update(|window, cx| {
        window.dispatch_event(e, cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn key(cx: &mut AsyncWindowContext, k: &str) {
    draw(cx);
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(crate::keymap::keystroke(k), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn mouse_move(cx: &mut AsyncWindowContext, p: Point<Pixels>, pressed: bool) {
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: pressed.then_some(MouseButton::Left), modifiers: Modifiers::default() }));
}

fn down(cx: &mut AsyncWindowContext, p: Point<Pixels>) {
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: p, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }));
}

fn up(cx: &mut AsyncWindowContext, p: Point<Pixels>) {
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: p, modifiers: Modifiers::default(), click_count: 1 }));
}

fn click(cx: &mut AsyncWindowContext, p: Point<Pixels>) {
    mouse_move(cx, p, false);
    down(cx, p);
    up(cx, p);
}

fn snap(name: &str, cx: &mut AsyncWindowContext) {
    let Ok(dir) = std::env::var("DATAMAN_SHOT_DIR") else { return };
    let path = format!("{dir}/ui-{name}.png");
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
    });
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn tabs(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Vec<Entity<QueryTab>> {
    cx.update(|_, cx| ws.read(cx).tabs.clone()).unwrap_or_default()
}

fn tab_points(ws: &Entity<Workspace>, i: usize, cx: &mut AsyncWindowContext) -> Option<(Point<Pixels>, Point<Pixels>)> {
    draw(cx);
    let b = cx.update(|_, cx| ws.read(cx).tab_bounds.borrow().get(i).copied()).ok().flatten()?;
    let mid = point(b.left() + b.size.width / 2., b.top() + b.size.height / 2.);
    // The × is 16 pt wide, 8 pt in from the tab's right edge.
    Some((mid, point(b.right() - px(16.), mid.y)))
}

async fn run_sql(ws: &Entity<Workspace>, sql: &str, cx: &mut AsyncWindowContext) -> Entity<QueryTab> {
    let tab = cx.update(|_, cx| ws.read(cx).active_tab().unwrap().clone()).unwrap();
    let editor = cx.update(|_, cx| tab.read(cx).editor.clone()).unwrap();
    let _ = cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.set_text(sql, cx));
        window.focus(&editor.focus_handle(cx), cx);
    });
    draw(cx);
    crate::editor::dispatch(cx, Box::new(crate::editor::RunStatement));
    for _ in 0..200 {
        pause(cx, 50).await;
        if cx.update(|_, cx| tab.read(cx).run_phase().is_some_and(|p| p.is_finished())).unwrap_or(true) {
            break;
        }
    }
    draw(cx);
    tab
}

/// A new tab grows in (motion::TAB_OPEN): its bounds are only its own once that is over.
async fn grown(ws: &Entity<Workspace>, tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
    for _ in 0..40 {
        draw(cx);
        if !cx.update(|_, cx| ws.read(cx).tab_motion.growing(tab.entity_id())).unwrap_or(false) {
            break;
        }
        pause(cx, 25).await;
    }
    draw(cx);
}

fn new_tab(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Entity<QueryTab> {
    let t = cx.update(|window, cx| ws.update(cx, |w, cx| w.new_tab(window, cx))).unwrap();
    draw(cx);
    t
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, id: &str, db: &str, cx: &mut AsyncWindowContext) -> bool {
    if stage == "tip-error" {
        tip_error(ws, id, cx).await;
        return true;
    }
    if stage == "zoom" {
        pause(cx, 600).await;
        let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.demo_toggle_side(cx)));
        pause(cx, 400).await;
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.toggle_result_zoom(window, cx)));
        pause(cx, 400).await;
        return true;
    }
    if stage == "copy-menu" {
        pause(cx, 600).await;
        let _ = cx.update(|_, cx| {
            ws.update(cx, |w, cx| {
                w.copy_menu = true;
                cx.notify();
            })
        });
        return true;
    }
    if stage == "picker" {
        let (mut p, pw) = crate::bench::bench_profile(None);
        p.name = "analytics-warehouse-replica-eu-west-1".into();
        let other = p.id.clone();
        let _ = cx.update(|_, cx| crate::bench::connect(ws, p, pw, cx));
        for _ in 0..200 {
            pause(cx, 50).await;
            if cx.update(|_, cx| ws.read(cx).store.read(cx).connection(&other).is_some()).unwrap_or(false) {
                break;
            }
        }
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.pick_target(&other, db, window, cx)));
        pause(cx, 300).await;
        let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.toggle_conn_menu(cx)));
        pause(cx, 600).await;
        return true;
    }
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };

    let (p, pw) = crate::bench::bench_profile(None);
    let first = p.id.clone();
    let _ = cx.update(|_, cx| {
        let core = ws.read(cx).store.read(cx).core.clone();
        core.save_connection(p, Some(&pw), None).unwrap();
        let store = ws.read(cx).store.clone();
        store.update(cx, |s, cx| s.reload(cx));
    });
    draw(cx);
    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    let cid = format!("c:{first}");
    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&cid, cx)));
    let (dbrow, schema) = (format!("{cid}/d:{db}"), format!("{cid}/d:{db}/s:public"));
    let mut shown = (false, false);
    for _ in 0..200 {
        pause(cx, 50).await;
        draw(cx);
        shown = cx.update(|_, cx| (tree.read(cx).has_row(&dbrow), tree.read(cx).has_row(&schema))).unwrap_or_default();
        if shown.0 && shown.1 {
            break;
        }
    }
    report("a connection opened before it connects shows its databases and schemas", shown == (true, true), format!("{shown:?}"));
    let _ = id;

    let menu_ro = |cx: &mut AsyncWindowContext| {
        let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.demo_menu(&dbrow, false, false, window, cx)));
        draw(cx);
        cx.update(|_, cx| tree.read(cx).demo_menu_checked(crate::tree_menu::Act::ToggleReadOnly)).ok().flatten()
    };
    let toggle = |cx: &mut AsyncWindowContext| {
        let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.demo_menu_run(crate::tree_menu::Act::ToggleReadOnly, window, cx)));
        draw(cx);
    };
    let before = menu_ro(cx);
    toggle(cx);
    let after = menu_ro(cx);
    let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.close_menu(false, window, cx)));
    let state = cx
        .update(|_, cx| {
            let store = ws.read(cx).store.read(cx);
            let live = store.connection(&first).is_some_and(|c| c.read_only_for(db));
            let saved = store.core.config.get(&first).is_some_and(|p| p.read_only_for(db) && !p.read_only);
            (tree.read(cx).demo_row_lock(&dbrow), live, saved)
        })
        .unwrap_or_default();
    report(
        "the database menu's Read Only checks, locks the row and is saved for that database only",
        before == Some(false) && after == Some(true) && state == (Some(true), true, true),
        format!("{before:?} → {after:?} {state:?}"),
    );
    let ro_tab = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_sql_tab(&first, Some(db), "", false, window, cx))).ok().flatten();
    if let Some(t) = ro_tab {
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| {
            if let Some(i) = w.tabs.iter().position(|x| *x == t) {
                w.activate(i, window, cx);
            }
        }));
        let write = "CREATE TEMP TABLE dataman_ro_ui (a int)";
        let tab = run_sql(ws, write, cx).await;
        let code = cx.update(|_, cx| tab.read(cx).result().and_then(|r| r.status().error).and_then(|e| e.code)).ok().flatten();
        report("a tab on it refuses a write (25006, before the server)", code.as_deref() == Some("25006"), format!("{code:?}"));
        menu_ro(cx);
        toggle(cx);
        let tab = run_sql(ws, write, cx).await;
        let (phase, lock) = cx.update(|_, cx| (tab.read(cx).run_phase(), tree.read(cx).demo_row_lock(&dbrow))).unwrap_or_default();
        report("Read Only off: the same tab writes, the lock goes", phase == Some(Phase::Done) && lock == Some(false), format!("{phase:?} {lock:?}"));
        key(cx, "cmd-w");
        pause(cx, 100).await;
    }

    let n0 = tabs(ws, cx).len();
    new_tab(ws, cx);
    let failed = run_sql(ws, "SELECT * FROM no_such_table_here", cx).await;
    let phase = cx.update(|_, cx| failed.read(cx).run_phase()).ok().flatten();
    report("the run failed", phase == Some(Phase::Failed), format!("{phase:?}"));
    key(cx, "cmd-w");
    pause(cx, 200).await;
    let gone = !tabs(ws, cx).contains(&failed);
    report("⌘W closes a tab whose run failed", gone && tabs(ws, cx).len() == n0, format!("{} tabs", tabs(ws, cx).len()));
    new_tab(ws, cx);
    let failed = run_sql(ws, "SELECT * FROM no_such_table_here", cx).await;
    grown(ws, &failed, cx).await;
    let ix = tabs(ws, cx).iter().position(|t| *t == failed).unwrap_or(0);
    if let Some((mid, x)) = tab_points(ws, ix, cx) {
        mouse_move(cx, mid, false);
        click(cx, x);
        pause(cx, 200).await;
    }
    let gone = !tabs(ws, cx).contains(&failed);
    report("its × (shown on hover in place of ✕) closes it", gone, format!("{} tabs", tabs(ws, cx).len()));

    while tabs(ws, cx).len() < 3 {
        new_tab(ws, cx);
    }
    pause(cx, 250).await;
    let before = tabs(ws, cx);
    let (a, b, c) = (tab_points(ws, 0, cx), tab_points(ws, 1, cx), tab_points(ws, 2, cx));
    let mut during = (false, None, false);
    if let (Some((p0, _)), Some((p1, _)), Some((p2, _))) = (a, b, c) {
        mouse_move(cx, p0, false);
        down(cx, p0);
        for p in [point(p0.x + px(6.), p0.y), point(p1.x + px(4.), p1.y), point(p2.x + px(4.), p2.y)] {
            mouse_move(cx, p, true);
            draw(cx);
        }
        pause(cx, 200).await;
        snap("tab-gap", cx);
        during = cx.update(|_, cx| (cx.has_active_drag(), ws.read(cx).tab_drop, ws.read(cx).tabs == before)).unwrap_or_default();
        let gap = cx.update(|_, cx| ws.read(cx).tab_motion.gap_target()).ok().flatten();
        report("the tabs make room: the gap the dragged tab would land in opens there", gap == Some((dataman_core::groups::Pane::Left, 3)), format!("{gap:?}"));
        up(cx, point(p2.x + px(4.), p2.y));
    }
    report("while a tab is dragged its ghost follows, a gap opens where it would land and the tabs stay put", during == (true, Some(3), true), format!("{during:?}"));
    let after = tabs(ws, cx);
    let moved = after.len() == before.len() && after[2] == before[0] && after[0] == before[1] && after[1] == before[2];
    let active = cx.update(|_, cx| ws.read(cx).active_tab().cloned()).ok().flatten();
    report("dragging the first tab past two others puts it third, the others in order", moved, format!("{} tabs", after.len()));
    report("the dragged tab is the active one", active.as_ref() == Some(&before[0]), String::new());
    if let Some((p1, _)) = tab_points(ws, 1, cx) {
        click(cx, p1);
    }
    let again = tabs(ws, cx);
    let active = cx.update(|_, cx| ws.read(cx).active_tab().cloned()).ok().flatten();
    report("a click activates a tab without moving it", again == after && active.as_ref() == Some(&after[1]), String::new());

    while tabs(ws, cx).len() < 4 {
        new_tab(ws, cx);
    }
    // A right click on tab `i`, then a click on the menu's item `k` (26 pt items under 4 pt of
    // padding, from the pointer).
    let menu_pick = |cx: &mut AsyncWindowContext, i: usize, k: usize| -> bool {
        let Some((p, _)) = tab_points(ws, i, cx) else { return false };
        mouse_move(cx, p, false);
        input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Right, position: p, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }));
        input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Right, position: p, modifiers: Modifiers::default(), click_count: 1 }));
        draw(cx);
        let open = cx.update(|_, cx| ws.read(cx).tab_menu.is_some()).unwrap_or(false);
        click(cx, point(p.x + px(40.), p.y + px(4. + 26. * k as f32 + 13.)));
        draw(cx);
        open
    };
    let four = tabs(ws, cx);
    let sql = "SELECT 42 AS tab_menu_answer";
    let _ = cx.update(|_, cx| four[2].read(cx).editor.clone().update(cx, |e, cx| e.set_text(sql, cx)));
    let saved = match tab_points(ws, 2, cx) {
        Some((p, _)) => {
            mouse_move(cx, p, false);
            input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Right, position: p, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }));
            input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Right, position: p, modifiers: Modifiers::default(), click_count: 1 }));
            draw(cx);
            pause(cx, 150).await;
            snap("tab-menu", cx);
            click(cx, point(p.x + px(40.), p.y + px(4. + 26. * 3. + 9. + 13.)));
            draw(cx);
            cx.update(|_, cx| {
                let favs = ws.read(cx).store.read(cx).core.favorites.list();
                let linked = four[2].read(cx).favorite.clone();
                linked.is_some_and(|(id, _)| favs.iter().any(|f| f.id == id && f.sql == sql))
            })
            .unwrap_or(false)
        }
        None => false,
    };
    report("a tab's menu Add to Favorites saves its script as a favorite, linked to the tab", saved, String::new());
    let open = menu_pick(cx, 3, 2);
    let still = cx.update(|_, cx| ws.read(cx).tab_menu.is_some()).unwrap_or(false);
    let _ = cx.update(|_, cx| ws.update(cx, |w, _| w.tab_menu = None));
    report("a right click opens the tab's menu; on the last tab Close to the Right does nothing", open && still && tabs(ws, cx) == four, format!("{} tabs", tabs(ws, cx).len()));
    let hit = menu_pick(cx, 1, 2);
    let left = tabs(ws, cx);
    report("Close to the Right closes the tabs after it", hit && left == four[..2], format!("{} tabs", left.len()));
    let hit = menu_pick(cx, 1, 1);
    let left = tabs(ws, cx);
    let active = cx.update(|_, cx| ws.read(cx).active_tab().cloned()).ok().flatten();
    report("Close Others leaves only that tab, the active one", hit && left == four[1..2] && active.as_ref() == Some(&four[1]), format!("{} tabs", left.len()));
    let hit = menu_pick(cx, 0, 0);
    let left = tabs(ws, cx);
    report("Close closes it (the last one gives way to a new tab)", hit && left.len() == 1 && left[0] != four[1], format!("{} tabs", left.len()));

    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    let mut turned = Vec::new();
    for name in ["schema-open", "schema-shut"] {
        let started = cx.update(|_, cx| tree.update(cx, |t, cx| t.demo_turn_schema(cx))).unwrap_or(false);
        // Read at once: the fill runs on the clock, and a short pause can outlast it on a slow
        // renderer.
        let moving = cx.update(|_, cx| tree.read(cx).demo_folder_turning()).unwrap_or(false);
        pause(cx, 80).await;
        snap(name, cx);
        // Stops within a few frames of its length (a slow renderer's frames are far apart).
        let mut still = false;
        for _ in 0..40 {
            pause(cx, 100).await;
            still = !cx.update(|_, cx| tree.read(cx).demo_folder_turning()).unwrap_or(true);
            if still {
                break;
            }
        }
        turned.push(started && moving && still);
    }
    report("a schema opened or shut in the explorer: its folder's fill drains away or comes back, then stops", turned == [true, true], format!("{turned:?}"));

    let (mut p2, pw2) = crate::bench::bench_profile(None);
    p2.name = "second".into();
    let second = p2.id.clone();
    let _ = cx.update(|_, cx| {
        let core = ws.read(cx).store.read(cx).core.clone();
        core.save_connection(p2, Some(&pw2), None).unwrap();
        let store = ws.read(cx).store.clone();
        store.update(cx, |s, cx| s.reload(cx));
    });
    draw(cx);
    draw(cx);
    let row = |cx: &mut AsyncWindowContext, id: &str| {
        draw(cx);
        cx.update(|_, cx| tree.read(cx).conn_bounds.borrow().get(format!("c:{id}").as_str()).copied()).ok().flatten()
    };
    let mut during = (false, None);
    let top = cx.update(|_, cx| ws.read(cx).store.read(cx).profiles.first().map(|p| p.id.clone())).ok().flatten().unwrap_or_default();
    let _ = cx.update(|_, cx| {
        let ids: Vec<String> = ws.read(cx).store.read(cx).profiles.iter().map(|p| format!("c:{}", p.id)).collect();
        tree.update(cx, |t, cx| ids.iter().for_each(|i| t.collapse_id(i, cx)));
    });
    let rows = (row(cx, &top), row(cx, &second));
    if let (Some(a), Some(b)) = rows {
        let from = point(b.left() + px(40.), b.center().y);
        let to = point(a.left() + px(40.), a.top() + px(4.));
        mouse_move(cx, from, false);
        down(cx, from);
        for p in [point(from.x, from.y - px(6.)), point(to.x, to.y + px(6.)), to] {
            mouse_move(cx, p, true);
            draw(cx);
        }
        during = cx.update(|_, cx| (cx.has_active_drag(), tree.read(cx).conn_drop(cx))).unwrap_or_default();
        up(cx, to);
        draw(cx);
    }
    report("a dragged connection shows its ghost and the line before the first one", during == (true, Some(0)), format!("{during:?} {rows:?}"));
    let order = cx
        .update(|_, cx| {
            let s = ws.read(cx).store.read(cx);
            let ids = |v: Vec<dataman_core::ConnectionProfile>| v.into_iter().map(|p| p.id).collect::<Vec<_>>();
            (ids(s.profiles.clone()), ids(s.core.config.connections()))
        })
        .unwrap_or_default();
    let dropped = order.0.first() == Some(&second) && order.0.get(1) == Some(&top) && order.1 == order.0;
    report("dropped, it is first in the explorer and in connections.toml", dropped, format!("{order:?}"));

    let target = cx.update(|_, cx| ws.read(cx).picker_bounds.get()).ok().flatten();
    if let Some(b) = target {
        click(cx, point(b.left() + b.size.width / 2., b.top() + b.size.height / 2.));
    }
    let open = cx.update(|_, cx| ws.read(cx).conn_menu).unwrap_or(false);
    report("a click on the picker opens it", open, format!("{target:?}"));
    let tab = cx.update(|_, cx| ws.read(cx).active_tab().unwrap().clone()).unwrap();
    let conn = cx.update(|_, cx| tab.read(cx).connection().map(|c| c.profile.id.clone())).ok().flatten().unwrap_or_default();
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.pick_target(&conn, "postgres", window, cx)));
    draw(cx);
    let (shut, now) = cx.update(|_, cx| (!ws.read(cx).conn_menu, tab.read(cx).database().to_string())).unwrap_or_default();
    report("picking a database moves the tab there and shuts the popup", shut && now == "postgres", now.clone());
    let back = run_sql(ws, "SELECT current_database()", cx).await;
    let cur = cx.update(|_, cx| back.read(cx).result().and_then(|r| r.snapshot().text(0, 0).map(|v| v.into_owned()))).ok().flatten();
    report("the tab's statements run on it", cur.as_deref() == Some("postgres"), format!("{cur:?}"));
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.toggle_conn_menu(cx)));
    draw(cx);
    click(cx, point(px(700.), px(400.)));
    let shut = cx.update(|_, cx| !ws.read(cx).conn_menu).unwrap_or(false);
    report("a click outside shuts it", shut, String::new());

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dialog(None, window, cx)));
    draw(cx);
    let saved = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).store.read(cx).core.config.connections().len()).unwrap_or(0);
    let saved_before = saved(cx);
    if let Some(d) = cx.update(|_, cx| ws.read(cx).dialog.clone()).ok().flatten() {
        let _ = cx.update(|window, cx| d.update(cx, |d, cx| d.set_entry(crate::conn_dialog::Entry::Url, window, cx)));
        draw(cx);
        use dataman_core::config::Engine;
        for (s, engine, host, port, user, db, pw) in [
            ("postgresql://app:s3cret@db.example.com:6543/shop?sslmode=require", Engine::Postgres, "db.example.com", 6543, "app", "shop", "s3cret"),
            ("mysql://app:s3cret@db.example.com:3307/shop", Engine::MySql, "db.example.com", 3307, "app", "shop", "s3cret"),
            ("jdbc:sqlserver://db.example.com:14330;databaseName=shop;user=app;password=s3cret", Engine::SqlServer, "db.example.com", 14330, "app", "shop", "s3cret"),
            ("Server=tcp:db.example.com,1433;Database=shop;User Id=app;Encrypt=False", Engine::SqlServer, "db.example.com", 1433, "app", "shop", ""),
        ] {
            let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.set_conn_str(s, cx)));
            draw(cx);
            let (p, typed) = cx.update(|_, cx| d.read(cx).current(cx)).unwrap();
            let got = (p.engine, p.host.as_str(), p.port, p.user.as_str(), p.database.as_str(), typed.as_str());
            report(&format!("URL mode reads {}", s.split([':', '=']).next().unwrap_or(s)), got == (engine, host, port, user, db, pw), format!("{got:?}"));
        }
        let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.set_conn_str("postgres//app@db.example.com/shop", cx)));
        key(cx, "cmd-enter");
        pause(cx, 200).await;
        let (red, open) = cx.update(|_, cx| (d.read(cx).conn_str_invalid(cx), ws.read(cx).dialog.is_some())).unwrap_or_default();
        report("a string that is not a connection string is red and does not save", red && open, format!("red {red}, open {open}"));
        let _ = cx.update(|window, cx| d.update(cx, |d, cx| d.set_entry(crate::conn_dialog::Entry::Fields, window, cx)));
        let (p, _) = cx.update(|_, cx| d.read(cx).current(cx)).unwrap();
        report("Fields shows what the last good string filled", p.engine == Engine::SqlServer && p.port == 1433, format!("{:?} {}", p.engine, p.port));
        key(cx, "escape");
        pause(cx, 200).await;
    }
    let closed = cx.update(|_, cx| ws.read(cx).dialog.is_none()).unwrap_or(false);
    let saved_after = saved(cx);
    report("esc closes the sheet and nothing was saved", closed && saved_after == saved_before, format!("closed {closed}, {saved_before} → {saved_after}"));

    // TLS follows the host until the user picks: opportunistic only on this machine.
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dialog(None, window, cx)));
    draw(cx);
    if let Some(d) = cx.update(|_, cx| ws.read(cx).dialog.clone()).ok().flatten() {
        use dataman_core::config::{Engine, SslMode};
        let tls = |cx: &mut AsyncWindowContext| cx.update(|_, cx| (d.read(cx).current(cx).0.sslmode, d.read(cx).demo_tls_note(cx).map(|n| n.0))).unwrap();
        let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.pick_engine(Engine::Postgres, cx)));
        let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_host("db.example.com", cx)));
        let remote = tls(cx);
        let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_host("localhost", cx)));
        let local = tls(cx);
        report("a remote host starts with SSL required, localhost with prefer", remote == (SslMode::Require, None) && local == (SslMode::Prefer, None), format!("{remote:?} {local:?}"));
        let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.pick_engine(Engine::Redis, cx)));
        let here = tls(cx);
        let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_host("cache.example.com", cx)));
        let there = tls(cx);
        report("Redis: off on this machine, on across the network", here == (SslMode::Disable, None) && there == (SslMode::Require, None), format!("{here:?} {there:?}"));
        let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_ssh(true, cx)));
        let tunneled = tls(cx);
        let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_ssh(false, cx)));
        let back = tls(cx);
        report("through an SSH tunnel it is as on this machine, no warning; without it, on again", tunneled == (SslMode::Disable, None) && back == (SslMode::Require, None), format!("{tunneled:?} {back:?}"));
        let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_tls(SslMode::Disable, cx)));
        let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_host("other.example.com", cx)));
        draw(cx);
        let off = tls(cx);
        report("the user's off stays, with the warning that it is in clear", off == (SslMode::Disable, Some(true)), format!("{off:?}"));
        key(cx, "escape");
        pause(cx, 200).await;
    }

    use dataman_core::UiFont;
    let ids = cx
        .update(|window, cx| {
            crate::theme::ensure_fonts(&UiFont::ALL, cx);
            let ts = window.text_system();
            UiFont::ALL
                .iter()
                .flat_map(|f| {
                    let base = gpui::font(f.family());
                    [400., 500., 600., 700.]
                        .map(|w| gpui::Font { weight: gpui::FontWeight(w), ..base.clone() })
                        .into_iter()
                        .chain([gpui::Font { style: gpui::FontStyle::Italic, ..base.clone() }])
                        .map(|font| ts.resolve_font(&font))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let distinct = ids.iter().collect::<std::collections::HashSet<_>>().len();
    report("each family resolves its Regular, Medium, SemiBold, Bold and Italic faces", ids.len() == 20 && distinct == 20, format!("{distinct} of {}", ids.len()));
    new_tab(ws, cx);
    let tab = run_sql(ws, "SELECT 42 AS n, 'café naïve ≠ ≤ → 東京 😀' AS sample, now() AS at", cx).await;
    let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
    draw(cx);
    let widths = |cx: &mut AsyncWindowContext| {
        draw(cx);
        cx.update(|_, cx| grid.read(cx).demo_widths()).unwrap_or_default()
    };
    let (w0, cw0) = widths(cx);
    let switch = |cx: &mut AsyncWindowContext, a: Box<dyn gpui::Action>| {
        let _ = cx.update(|window, cx| window.dispatch_action(a, cx));
        draw(cx);
    };
    switch(cx, Box::new(crate::app::FontZedMono));
    let (w1, cw1) = widths(cx);
    let k = cw1 / cw0;
    let scaled = w0.len() == w1.len() && w0.iter().zip(&w1).all(|(a, b)| (a * k - b).abs() <= 1.);
    report(
        "Zed Mono live: the grid measures its 0.5 em advance and scales every column to it",
        crate::theme::mono() == "Zed Mono" && (cw1 - 6.).abs() < 0.05 && (cw0 - 7.2).abs() < 0.05 && scaled && w1 != w0,
        format!("advance {cw0:.2} → {cw1:.2}, widths {w0:?} → {w1:?}"),
    );
    switch(cx, Box::new(crate::app::FontJetBrainsMono));
    let (w2, cw2) = widths(cx);
    let back = w0.len() == w2.len() && w0.iter().zip(&w2).all(|(a, b)| (a - b).abs() <= 1.5);
    report("JetBrains Mono live: back to 0.6 em and the first widths", crate::theme::mono() == "JetBrains Mono" && (cw2 - 7.2).abs() < 0.05 && back, format!("advance {cw2:.2}, widths {w2:?}"));
    switch(cx, Box::new(crate::app::FontLilex));
    let (_, cw3) = widths(cx);
    report("Lilex live", crate::theme::mono() == "Lilex" && (cw3 - 7.2).abs() < 0.05, format!("advance {cw3:.2}"));
    switch(cx, Box::new(crate::app::FontGeistMono));
    let (w4, _) = widths(cx);
    report("Geist Mono again: the widths it started with", crate::theme::mono() == "Geist Mono" && w4.len() == w0.len() && w0.iter().zip(&w4).all(|(a, b)| (a - b).abs() <= 1.5), format!("{w4:?}"));

    let n0 = tabs(ws, cx).len();
    let doomed = new_tab(ws, cx);
    grown(ws, &doomed, cx).await;
    let ix = tabs(ws, cx).iter().position(|t| *t == doomed).unwrap_or(0);
    if let Some((mid, _)) = tab_points(ws, ix, cx) {
        middle_click(cx, mid);
        pause(cx, 100).await;
    }
    report("a middle click closes a query tab", !tabs(ws, cx).contains(&doomed) && tabs(ws, cx).len() == n0, format!("{} tabs", tabs(ws, cx).len()));
    let held = new_tab(ws, cx);
    run_sql(ws, "BEGIN", cx).await;
    let ix = tabs(ws, cx).iter().position(|t| *t == held).unwrap_or(0);
    if let Some((mid, _)) = tab_points(ws, ix, cx) {
        middle_click(cx, mid);
        pause(cx, 100).await;
    }
    let asked = cx.update(|_, cx| ws.read(cx).tx_prompt.is_some()).unwrap_or(false);
    report("with a transaction open it asks first, like ⌘W", asked && tabs(ws, cx).contains(&held), format!("prompt {asked}"));
    key(cx, "escape");
    pause(cx, 100).await;
    run_sql(ws, "ROLLBACK", cx).await;
    let still = tabs(ws, cx).contains(&held) && cx.update(|_, cx| ws.read(cx).tx_prompt.is_none()).unwrap_or(false);
    report("esc keeps the tab", still, String::new());
    let script = cx.update(|_, cx| ws.read(cx).active_tab().unwrap().clone()).unwrap();
    let editor = cx.update(|_, cx| script.read(cx).editor.clone()).unwrap();
    let _ = cx.update(|window, cx| {
        editor.update(cx, |e, cx| e.set_text("SELECT 1 AS a;\nSELECT 2 AS b;\nSELECT 3 AS c;", cx));
        window.focus(&editor.focus_handle(cx), cx);
    });
    key(cx, "cmd-enter");
    for _ in 0..200 {
        pause(cx, 50).await;
        if cx.update(|_, cx| !script.read(cx).is_running() && script.read(cx).result_tabs().len() >= 3).unwrap_or(true) {
            break;
        }
    }
    draw(cx);
    let before = cx.update(|_, cx| script.read(cx).result_tabs().iter().map(|t| t.key).collect::<Vec<_>>()).unwrap_or_default();
    report("⌘↩ runs the whole script", before.len() >= 3, format!("{before:?}"));
    let b = cx.update(|_, cx| ws.read(cx).result_tab_bounds.borrow().get(1).copied()).ok().flatten();
    if let Some(b) = b {
        middle_click(cx, point(b.left() + b.size.width / 2., b.top() + b.size.height / 2.));
        pause(cx, 100).await;
    }
    let after = cx.update(|_, cx| script.read(cx).result_tabs().iter().map(|t| t.key).collect::<Vec<_>>()).unwrap_or_default();
    let closed = before.len() >= 3 && after.len() + 1 == before.len() && !after.contains(&before[1]);
    report("a middle click closes a result tab", closed, format!("{before:?} → {after:?}"));
    let run_one = |sel: Option<(usize, usize)>, k: &'static str, cx: &mut AsyncWindowContext| {
        let _ = cx.update(|window, cx| {
            editor.update(cx, |e, cx| match sel {
                Some((a, b)) => e.demo_select(a, b, cx),
                None => e.set_cursor("SELECT 1 AS a;\nSELECT 2".len(), cx),
            });
            window.focus(&editor.focus_handle(cx), cx);
        });
        key(cx, k);
    };
    let ran = |cx: &mut AsyncWindowContext| {
        cx.update(|_, cx| {
            let t = script.read(cx);
            let cols = t.result().and_then(|r| r.columns()).map(|c| c.iter().map(|c| c.name.clone()).collect::<Vec<_>>());
            (t.result_tabs().len(), cols)
        })
        .unwrap_or_default()
    };
    async fn settle(script: &Entity<QueryTab>, cx: &mut AsyncWindowContext) {
        for _ in 0..200 {
            pause(cx, 50).await;
            if cx.update(|_, cx| !script.read(cx).is_running() && script.read(cx).run_phase().is_some_and(|p| p.is_finished())).unwrap_or(true) {
                break;
            }
        }
        draw(cx);
    }
    run_one(None, "cmd-shift-enter", cx);
    settle(&script, cx).await;
    let one = ran(cx);
    report("⌘⇧↩ runs the statement under the cursor", one.0 == 1 && one.1.as_deref() == Some(&["b".to_string()][..]), format!("{one:?}"));
    let third = "SELECT 1 AS a;\nSELECT 2 AS b;\n".len();
    run_one(Some((third, third + "SELECT 3 AS c;".len())), "cmd-enter", cx);
    settle(&script, cx).await;
    let sel = ran(cx);
    report("⌘↩ with a selection runs only the selection", sel.0 == 1 && sel.1.as_deref() == Some(&["c".to_string()][..]), format!("{sel:?}"));

    let put = |text: &str, cx: &mut AsyncWindowContext| {
        let _ = cx.update(|window, cx| {
            editor.update(cx, |e, cx| {
                e.set_text(text, cx);
                e.set_cursor(0, cx);
            });
            window.focus(&editor.focus_handle(cx), cx);
        });
        draw(cx);
        draw(cx);
        cx.update(|_, cx| (editor.read(cx).demo_bars(), editor.read(cx).scroll_points())).unwrap()
    };
    let long: String = (1..=300).map(|i| format!("SELECT {i} AS n;\n")).collect();
    let (([v, h], (_, max_y)), (_, y0)) = put(&long, cx);
    report("a script taller than the editor has a vertical scrollbar (no horizontal one)", v.is_some() && h.is_none() && y0 == 0., format!("{v:?} {h:?} {y0}"));
    if let Some((track, thumb)) = v {
        let x = track.left() + track.size.width / 2.;
        click(cx, point(x, thumb.bottom() + px(30.)));
        let y1 = cx.update(|_, cx| editor.read(cx).scroll_points().1).unwrap();
        let cursor0 = cx.update(|_, cx| editor.read(cx).demo_selection().start).unwrap();
        report("a press on the track pages down (the cursor stays)", y1 > 100. && cursor0 == 0, format!("{y1} cursor {cursor0}"));
        let ([v, _], _) = cx.update(|_, cx| editor.read(cx).demo_bars()).unwrap();
        if let Some((track, thumb)) = v {
            let grab = point(x, thumb.top() + thumb.size.height / 2.);
            mouse_move(cx, grab, false);
            down(cx, grab);
            mouse_move(cx, point(x, track.bottom() + px(40.)), true);
            up(cx, point(x, track.bottom() + px(40.)));
        }
        let y2 = cx.update(|_, cx| editor.read(cx).scroll_points().1).unwrap();
        report("the thumb drags to the end", (y2 - max_y).abs() < 1., format!("{y2} of {max_y}"));
    }
    let (([v, h], _), _) = put("SELECT 1;", cx);
    report("a short script has no scrollbar", v.is_none() && h.is_none(), format!("{v:?} {h:?}"));
    let wide = format!("SELECT {} 1;", "x_column_name + ".repeat(40));
    let (([v, h], _), _) = put(&wide, cx);
    report("a line wider than the editor has a horizontal scrollbar", v.is_none() && h.is_some(), format!("{v:?} {h:?}"));

    run_sql(ws, "SELECT generate_series(1, 2000) AS n, 'x' AS t", cx).await;
    let export_one = async |format: dataman_core::export::ExportFormat, cx: &mut AsyncWindowContext| {
        let e = cx.update(|_, cx| ws.read(cx).export.clone()).ok().flatten()?;
        let _ = cx.update(|_, cx| e.update(cx, |e, cx| e.demo_start(format, cx)));
        for _ in 0..200 {
            pause(cx, 50).await;
            if let Some(o) = cx.update(|_, cx| e.read(cx).demo_outcome()).ok().flatten() {
                draw(cx);
                let status = cx.update(|_, cx| e.read(cx).demo_status(cx)).ok().flatten();
                return Some((o, status));
            }
        }
        None
    };
    let open_export = |cx: &mut AsyncWindowContext| {
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_export(window, cx)));
        draw(cx);
    };
    open_export(cx);
    let csv = export_one(dataman_core::export::ExportFormat::Csv, cx).await;
    let json = export_one(dataman_core::export::ExportFormat::Json, cx).await;
    let ok_file = |r: &Option<(Result<(std::path::PathBuf, u64), String>, Option<String>)>, ext: &str| match r {
        Some((Ok((p, rows)), Some(s))) => *rows == 2000 && p.extension().is_some_and(|e| e == ext) && p.exists() && s.starts_with("Exported 2,000 rows"),
        _ => false,
    };
    report("an export, then another format from the same sheet (CSV, JSON)", ok_file(&csv, "csv") && ok_file(&json, "json"), format!("{csv:?} {json:?}"));
    for r in [&csv, &json] {
        if let Some((Ok((p, _)), _)) = r {
            let _ = std::fs::remove_file(p);
        }
    }
    let at = cx.update(|_, cx| ws.read(cx).export.as_ref().and_then(|e| e.read(cx).demo_close_point())).ok().flatten();
    if let Some(p) = at {
        click(cx, p);
        pause(cx, 100).await;
    }
    let closed = cx.update(|_, cx| ws.read(cx).export.is_none()).unwrap_or(false);
    report("after an export the header's × closes the sheet", at.is_some() && closed, format!("{at:?}"));
    open_export(cx);
    let again = export_one(dataman_core::export::ExportFormat::Csv, cx).await;
    if let Some((Ok((p, _)), _)) = &again {
        let _ = std::fs::remove_file(p);
    }
    let _ = cx.update(|window, cx| {
        if let Some(e) = ws.read(cx).export.clone() {
            window.focus(&e.focus_handle(cx), cx);
        }
    });
    key(cx, "escape");
    pause(cx, 100).await;
    let closed = cx.update(|_, cx| ws.read(cx).export.is_none()).unwrap_or(false);
    report("…and esc too", again.is_some() && closed, String::new());

    let dir = std::env::temp_dir().join(format!("dataman-open-sql-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let files: Vec<std::path::PathBuf> = [("a report.sql", "SELECT 1 AS a;"), ("b.sql", "SELECT 2 AS b;")]
        .iter()
        .map(|(n, s)| {
            let p = dir.join(n);
            let _ = std::fs::write(&p, s);
            p
        })
        .collect();
    let n0 = tabs(ws, cx).len();
    crate::sql_picker::set_test_paths(files);
    key(cx, "cmd-o");
    pause(cx, 200).await;
    draw(cx);
    let opened: Vec<(String, String)> = cx
        .update(|_, cx| {
            let all = ws.read(cx).tabs.clone();
            all.iter().skip(n0).map(|t| (t.read(cx).title(cx).to_string(), t.read(cx).editor.read(cx).text().to_string())).collect()
        })
        .unwrap_or_default();
    report(
        "⌘O opens each picked .sql file in a tab of its own, named after the file",
        opened == [("a report.sql".to_string(), "SELECT 1 AS a;".to_string()), ("b.sql".to_string(), "SELECT 2 AS b;".to_string())],
        format!("{opened:?}"),
    );
    let _ = std::fs::remove_dir_all(&dir);
    for _ in 0..opened.len() {
        key(cx, "cmd-w");
        pause(cx, 50).await;
    }

    use crate::glass::{Blur, blur, own_view_installed, own_view_layout};
    let own = blur() == Blur::Own;
    let level_on = crate::theme::effective_level(dataman_core::Translucency::On).is_on();
    let fits = |cx: &mut AsyncWindowContext| {
        draw(cx);
        let _ = cx;
        match own_view_layout() {
            Some((first, v, c)) => first && (v.0 - c.0).abs() < 0.5 && (v.1 - c.1).abs() < 0.5,
            None => false,
        }
    };
    report(&format!("blur {blur:?}: Dataman's view only where it is the blur", blur = blur()), own_view_installed() == (own && level_on), format!("installed {}", own_view_installed()));
    if own && level_on {
        report("Dataman's view is under GPUI's and fills the window", fits(cx), format!("{:?}", own_view_layout()));
        let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::app::TranslucencyOff), cx));
        draw(cx);
        report("Transparency off removes it", !own_view_installed(), String::new());
        let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::app::TranslucencyOn), cx));
        draw(cx);
        report("Transparency on puts it back", own_view_installed() && fits(cx), format!("{:?}", own_view_layout()));
        let _ = cx.update(|window, _| window.resize(gpui::size(px(1180.), px(760.))));
        pause(cx, 300).await;
        report("it follows a resize", fits(cx), format!("{:?}", own_view_layout()));
    }

    new_tab(ws, cx);
    let cols: Vec<String> = (1..=40).map(|i| format!("g * {i} AS column_{i:02}")).collect();
    let wide = run_sql(ws, &format!("SELECT {} FROM generate_series(1, 2000) g", cols.join(", ")), cx).await;
    let grid = cx.update(|_, cx| wide.read(cx).grid.clone()).unwrap();
    draw(cx);
    let (gx, gy, gw, gh) = cx.update(|_, cx| grid.read(cx).demo_cells_rect()).unwrap_or_default();
    let start = cx.update(|_, cx| grid.read(cx).demo_cell_point(0, 0)).unwrap();
    let visible_cols = cx.update(|_, cx| (0..40).filter(|c| grid.read(cx).cell_bounds(0, *c).is_some_and(|b| b.0 + b.2 <= gx + gw)).count()).unwrap_or(0);
    let window_w = cx.update(|window, _| window.viewport_size().width.as_f32()).unwrap_or(0.);
    mouse_move(cx, start, false);
    down(cx, start);
    let out = point(px(window_w.max(gx + gw) + 40.), start.y);
    mouse_move(cx, point(px(gx + gw - 30.), start.y), true);
    mouse_move(cx, out, true);
    pause(cx, 600).await;
    draw(cx);
    let (sx, running) = cx.update(|_, cx| (grid.read(cx).demo_scroll().0, grid.read(cx).demo_autoscrolling())).unwrap_or_default();
    let head = cx.update(|_, cx| grid.read(cx).demo_selection()).ok().flatten();
    up(cx, out);
    pause(cx, 100).await;
    let sx_after = cx.update(|_, cx| grid.read(cx).demo_scroll().0).unwrap_or_default();
    pause(cx, 200).await;
    let (sx_later, stopped) = cx.update(|_, cx| (grid.read(cx).demo_scroll().0, !grid.read(cx).demo_autoscrolling())).unwrap_or_default();
    let far = head.is_some_and(|(a, h)| a == (0, 0) && h.0 == 0 && h.1 >= visible_cols);
    report(
        "a selection dragged past the right edge scrolls sideways and extends past the columns that were visible",
        running && sx > 50. && far,
        format!("scroll x {sx:.0}, running {running}, selection {head:?}, {visible_cols} columns visible"),
    );
    report("the release stops it", stopped && (sx_later - sx_after).abs() < 0.5, format!("{sx_after:.0} → {sx_later:.0}"));
    let _ = cx.update(|_, cx| grid.update(cx, |g, cx| g.set_scroll_x(0., cx)));
    draw(cx);
    let visible_rows = (gh / crate::theme::metrics::GRID_ROW).floor() as usize;
    let start = cx.update(|_, cx| grid.read(cx).demo_cell_point(0, 0)).unwrap();
    mouse_move(cx, start, false);
    down(cx, start);
    let below = point(start.x, px(gy + gh + 60.));
    mouse_move(cx, below, true);
    pause(cx, 600).await;
    draw(cx);
    let sy = cx.update(|_, cx| grid.read(cx).demo_scroll().1).unwrap_or_default();
    let head = cx.update(|_, cx| grid.read(cx).demo_selection()).ok().flatten();
    up(cx, below);
    let down_far = head.is_some_and(|(a, h)| a == (0, 0) && h.1 == 0 && h.0 > visible_rows);
    report("past the bottom edge it scrolls down and extends through the rows", sy > 50. && down_far, format!("scroll y {sy:.0}, selection {head:?}, {visible_rows} rows visible"));

    let editor = cx.update(|_, cx| wide.read(cx).editor.clone()).unwrap();
    let measure = |cx: &mut AsyncWindowContext| {
        draw(cx);
        draw(cx);
        cx.update(|_, cx| {
            let tab_h = ws.read(cx).tab_bounds.borrow().first().map_or(0., |b| b.size.height.as_f32());
            let (lh, cw) = editor.read(cx).demo_metrics();
            let (row, gw) = grid.read(cx).demo_row_metrics();
            (tab_h, lh, cw, row, gw, grid.read(cx).demo_widths().0)
        })
        .unwrap_or_default()
    };
    let m0 = measure(cx);
    key(cx, "cmd-=");
    key(cx, "cmd-=");
    let m1 = measure(cx);
    let z = crate::theme::zoom();
    let saved = cx.update(|_, cx| ws.read(cx).store.read(cx).core.settings.get().zoom).unwrap_or(0.);
    let near = |a: f32, b: f32, tol: f32| (a - b).abs() <= tol;
    report(
        "⌘= ⌘= zooms to 125 %: tabs, editor lines and grid rows 1.25× taller, the editor's advance measured 1.25× wider, saved",
        z == 1.25 && saved == 1.25 && near(m1.0, m0.0 * 1.25, 0.5) && near(m1.1, (m0.1 * 1.25).round(), 0.5) && near(m1.2, m0.2 * 1.25, 0.3) && near(m1.3, m0.3 * 1.25, 0.01),
        format!("zoom {z} saved {saved}: tab {:.1}→{:.1}, line {:.1}→{:.1}, advance {:.2}→{:.2}, row {:.1}→{:.1}", m0.0, m1.0, m0.1, m1.1, m0.2, m1.2, m0.3, m1.3),
    );
    report(
        "the grid's columns keep their widths in points and its advance in points",
        m1.5 == m0.5 && near(m1.4, m0.4, 0.05),
        format!("advance {:.2}→{:.2}, widths {:?}→{:?}", m0.4, m1.4, &m0.5[..m0.5.len().min(4)], &m1.5[..m1.5.len().min(4)]),
    );
    key(cx, "cmd--");
    report("⌘- steps back to 110 %", crate::theme::zoom() == 1.1, format!("{}", crate::theme::zoom()));
    key(cx, "cmd-0");
    let m2 = measure(cx);
    report(
        "⌘0 is 100 % again, with the first metrics",
        crate::theme::zoom() == 1.0 && near(m2.0, m0.0, 0.01) && near(m2.1, m0.1, 0.01) && near(m2.2, m0.2, 0.01) && near(m2.3, m0.3, 0.01) && m2.5 == m0.5,
        format!("tab {:.1}, line {:.1}, advance {:.2}, row {:.1}", m2.0, m2.1, m2.2, m2.3),
    );

    use crate::keymap;
    let saved_keys = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).store.read(cx).core.settings.get().keys).unwrap_or_default();
    let settings = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).settings_view.clone()).ok().flatten();
    let open_keyboard = |cx: &mut AsyncWindowContext| {
        press(cx, "cmd-,");
        let sv = settings(cx);
        if let Some(sv) = &sv {
            let _ = cx.update(|_, cx| sv.update(cx, |v, cx| v.show_section("keyboard", cx)));
            draw(cx);
            draw(cx);
        }
        sv
    };
    let Some(sv) = open_keyboard(cx) else {
        report("⌘, opens Settings", false, String::new());
        return ok;
    };
    let points = |sv: &Entity<crate::settings_view::SettingsView>, id: &str, cx: &mut AsyncWindowContext| {
        draw(cx);
        cx.update(|_, cx| sv.read(cx).demo_key_points(id)).unwrap_or_default()
    };
    let recording = |sv: &Entity<crate::settings_view::SettingsView>, cx: &mut AsyncWindowContext| cx.update(|_, cx| sv.read(cx).demo_recording()).ok().flatten();
    let k = |s: &str| keymap::parse(&keymap::native(s)).unwrap_or_default();
    let (cap, reset) = points(&sv, "new_tab", cx);
    report("Keyboard lists New tab on ⌘T, no reset while it is the default", cap.is_some() && reset.is_none() && keymap::key_label("new_tab") == keymap::label("cmd-t"), format!("{cap:?} {reset:?}"));
    if let Some(cap) = cap {
        click(cx, cap);
        let rec = recording(&sv, cx);
        press(cx, "escape");
        report(
            "a click on the key records; esc cancels and keeps ⌘T and the sheet",
            rec == Some("new_tab") && recording(&sv, cx).is_none() && keymap::effective("new_tab") == [k("cmd-t")] && settings(cx).is_some(),
            format!("{rec:?} {:?}", keymap::effective("new_tab")),
        );
        click(cx, cap);
        press(cx, "cmd-alt-t");
        let bound = cx
            .update(|_, cx| {
                cx.key_bindings().borrow().bindings_for_action(&crate::app::NewTab).map(|b| b.keystrokes().iter().map(|k| k.inner().unparse()).collect::<Vec<_>>().join(" ")).collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let saved = saved_keys(cx);
        report(
            "⌥⌘T becomes New tab's key: saved in [keys], on the ⌘? sheet, the only binding the menus see",
            keymap::effective("new_tab") == [k("alt-cmd-t")]
                && saved.get("new_tab") == Some(&k("alt-cmd-t"))
                && crate::shortcuts::keys("{new_tab}") == keymap::label("alt-cmd-t")
                && bound == [k("alt-cmd-t")],
            format!("{:?} {saved:?} {bound:?}", keymap::effective("new_tab")),
        );
        let (_, reset) = points(&sv, "new_tab", cx);
        report("a changed key shows its reset", reset.is_some(), String::new());
        let search = |sv: &Entity<crate::settings_view::SettingsView>, q: &str, cx: &mut AsyncWindowContext| {
            let _ = cx.update(|_, cx| sv.update(cx, |v, cx| v.demo_search(q, cx)));
            draw(cx);
        };
        search(&sv, "run statement", cx);
        if let (Some(run), _) = points(&sv, "run", cx) {
            click(cx, run);
            press(cx, "cmd-alt-t");
            let both = keymap::conflicts();
            let (_, run_reset) = points(&sv, "run", cx);
            if let Some(r) = run_reset {
                click(cx, r);
            }
            report(
                "⌥⌘T on Run Statement too is a conflict on both; its reset clears it",
                both.contains("run") && both.contains("new_tab") && keymap::conflicts().is_empty() && keymap::effective("run") == [k("cmd-shift-enter")],
                format!("{both:?} → {:?}", keymap::conflicts()),
            );
        }
        search(&sv, "", cx);
        press(cx, "escape");
        let closed = settings(cx).is_none();
        let n0 = tabs(ws, cx).len();
        press(cx, "cmd-alt-t");
        let n1 = tabs(ws, cx).len();
        press(cx, "cmd-t");
        let n2 = tabs(ws, cx).len();
        report("with Settings shut, ⌥⌘T opens a tab and ⌘T does not", closed && n1 == n0 + 1 && n2 == n1, format!("closed {closed}, {n0} → {n1} → {n2}"));
        if let Some(sv) = open_keyboard(cx) {
            if let (_, Some(r)) = points(&sv, "new_tab", cx) {
                click(cx, r);
            }
            report("the reset puts ⌘T back and drops it from [keys]", keymap::effective("new_tab") == [k("cmd-t")] && !saved_keys(cx).contains_key("new_tab"), format!("{:?}", keymap::effective("new_tab")));
            search(&sv, "favorite query", cx);
            if let (Some(h), _) = points(&sv, "favorite_query", cx) {
                click(cx, h);
                press(cx, "backspace");
            }
            let removed = keymap::effective("favorite_query").is_empty() && saved_keys(cx).get("favorite_query").map(String::as_str) == Some("") && keymap::key("favorite_query").is_none();
            search(&sv, "", cx);
            draw(cx);
            if let Some(p) = cx.update(|_, cx| sv.read(cx).demo_reset_all_point()).ok().flatten() {
                click(cx, p);
            }
            report(
                "⌫ removes Favorite query's key; Reset all restores ⌘D and empties [keys]",
                removed && keymap::effective("favorite_query") == [k("cmd-d")] && saved_keys(cx).is_empty(),
                format!("removed {removed}, {:?} {:?}", keymap::effective("favorite_query"), saved_keys(cx)),
            );
            let key_search = |sv: &Entity<crate::settings_view::SettingsView>, cx: &mut AsyncWindowContext| cx.update(|_, cx| sv.read(cx).demo_key_search()).unwrap_or_default();
            let n0 = tabs(ws, cx).len();
            draw(cx);
            if let Some(p) = cx.update(|_, cx| sv.read(cx).demo_key_search_point()).ok().flatten() {
                click(cx, p);
            }
            press(cx, "cmd-t");
            let (on, key, ids) = key_search(&sv, cx);
            press(cx, "cmd-shift-=");
            let (_, plus, zoom_ids) = key_search(&sv, cx);
            press(cx, "backspace");
            let (_, cleared, all) = key_search(&sv, cx);
            press(cx, "escape");
            let (still, _, _) = key_search(&sv, cx);
            report(
                "search by shortcut: ⌘T lists New tab and opens none, ⇧⌘= finds Zoom in by its second key; ⌫ clears, esc ends it",
                on && key == Some(k("cmd-t"))
                    && ids == ["new_tab"]
                    && tabs(ws, cx).len() == n0
                    && plus == Some(k("cmd-shift-="))
                    && zoom_ids == ["zoom_in"]
                    && crate::theme::zoom() == 1.0
                    && cleared.is_none()
                    && all.len() == keymap::COMMANDS.len()
                    && !still
                    && settings(cx).is_some(),
                format!("{on} {key:?} {ids:?} {plus:?} {zoom_ids:?} {cleared:?} {} still {still}, {n0} → {} tabs", all.len(), tabs(ws, cx).len()),
            );
            press(cx, "escape");
        }
        let n0 = tabs(ws, cx).len();
        press(cx, "cmd-t");
        report("⌘T opens a tab again", tabs(ws, cx).len() == n0 + 1, format!("{n0} → {}", tabs(ws, cx).len()));
    }
    let small = run_sql(ws, "SELECT 7 AS answer", cx).await;
    let grid = cx.update(|_, cx| small.read(cx).grid.clone()).unwrap();
    let clip = cx.update(|_, cx| cx.read_from_clipboard()).ok().flatten();
    let _ = cx.update(|window, cx| {
        grid.update(cx, |g, cx| g.select_cell(0, 0, cx));
        window.focus(&grid.focus_handle(cx), cx);
    });
    press(cx, "cmd-shift-c");
    let with = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text())).ok().flatten();
    press(cx, "cmd-alt-c");
    let csv = cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text())).ok().flatten();
    if let Some(c) = clip.clone() {
        let _ = cx.update(|_, cx| cx.write_to_clipboard(c));
    }
    report("⇧⌘C in the grid copies with headers, ⌥⌘C as CSV", with.as_deref().map(str::trim_end) == Some("answer\n7") && csv.as_deref().map(str::trim_end) == Some("7"), format!("{with:?} {csv:?}"));
    let shown = |cx: &mut AsyncWindowContext| cx.update(|_, cx| grid.read(cx).copied_at.and_then(|at| crate::motion::copied(at, std::time::Instant::now(), cx.reduce_motion())).is_some()).unwrap_or(false);
    let copied_now = shown(cx);
    pause(cx, 2100).await;
    draw(cx);
    report("a copy shows Copied, then the Copy button again after 2 s", copied_now && !shown(cx), String::new());
    let quoted = run_sql(ws, "SELECT 'a,b' AS \"x y\", 'say \"hi\"' AS z, NULL AS n", cx).await;
    let qgrid = cx.update(|_, cx| quoted.read(cx).grid.clone()).unwrap();
    let _ = cx.update(|window, cx| window.focus(&qgrid.focus_handle(cx), cx));
    press(cx, "cmd-a");
    let mut copies = Vec::new();
    for (fmt, header) in [(dataman_core::result::CopyFormat::Csv, true), (dataman_core::result::CopyFormat::Tsv, true)] {
        let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.copy_selection(fmt, header, cx)));
        copies.push(cx.update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text())).ok().flatten().unwrap_or_default());
    }
    report(
        "Copy as CSV with Headers quotes as RFC 4180; Copy as TSV with Headers escapes as TSV",
        copies == ["x y,z,n\n\"a,b\",\"say \"\"hi\"\"\",\n", "x y\tz\tn\na,b\tsay \"hi\"\t\\N\n"],
        format!("{copies:?}"),
    );
    if let Some(c) = clip {
        let _ = cx.update(|_, cx| cx.write_to_clipboard(c));
    }
    ok &= motion_checks(ws, id, cx).await;
    ok &= grid_row_keys(ws, cx).await;
    ok &= zoom_checks(ws, cx).await;
    ok
}

async fn grid_row_keys(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let cols: Vec<String> = (1..=40).map(|i| format!("'value number {i}' AS column_{i}")).collect();
    let wide = run_sql(ws, &format!("SELECT {} FROM generate_series(1, 5)", cols.join(", ")), cx).await;
    let grid = cx.update(|_, cx| wide.read(cx).grid.clone()).unwrap();
    let _ = cx.update(|window, cx| {
        grid.update(cx, |g, cx| g.select_cell(2, 3, cx));
        window.focus(&grid.focus_handle(cx), cx);
    });
    draw(cx);
    let state = |cx: &mut AsyncWindowContext| cx.update(|_, cx| (grid.read(cx).demo_selection(), grid.read(cx).demo_scroll().0)).unwrap();
    let active = cx.update(|_, cx| ws.read(cx).active).unwrap();
    press(cx, "cmd-right");
    let (sel, x) = state(cx);
    report("⌘→ on the grid: the row's last column, scrolled into view", sel == Some(((2, 39), (2, 39))) && x > 100., format!("{sel:?} x {x}"));
    press(cx, "cmd-left");
    let (sel, x) = state(cx);
    report("⌘← on the grid: the row's first column, scrolled back", sel == Some(((2, 0), (2, 0))) && x == 0., format!("{sel:?} x {x}"));
    report("… and neither is Back or Forward", cx.update(|_, cx| ws.read(cx).active).unwrap() == active, String::new());
    press(cx, "cmd-shift-right");
    let (sel, _) = state(cx);
    report("⇧⌘→ extends the selection to the row's last column", sel == Some(((2, 0), (2, 39))), format!("{sel:?}"));
    press(cx, "cmd-shift-left");
    let (sel, _) = state(cx);
    report("⇧⌘← back to its first", sel == Some(((2, 0), (2, 0))), format!("{sel:?}"));
    report("Back and Forward are ⌃⌘← ⌃⌘→", crate::keymap::effective("go_back") == [crate::keymap::native("ctrl-cmd-left")] && crate::keymap::effective("go_forward") == [crate::keymap::native("ctrl-cmd-right")], format!("{:?}", crate::keymap::effective("go_back")));
    let ix = cx.update(|_, cx| ws.read(cx).tabs.iter().position(|t| *t == wide)).ok().flatten().unwrap_or(0);
    let other = if ix == 0 { 1 } else { 0 };
    if cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap_or(0) < 2 {
        new_tab(ws, cx);
    }
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.activate(other, window, cx)));
    draw(cx);
    let can_back = cx.update(|_, cx| ws.read(cx).can_go(false)).unwrap_or(false);
    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
    let _ = cx.update(|window, cx| {
        let f = ws.read(cx).active_tab().map(|t| t.read(cx).grid.focus_handle(cx));
        if let Some(f) = f {
            window.focus(&f, cx);
        }
    });
    press(cx, "ctrl-cmd-left");
    let now = cx.update(|_, cx| ws.read(cx).active).unwrap();
    report("⌃⌘← goes back", can_back && now == ix, format!("back {can_back}: tab {now}, want {ix}"));
    ok
}

async fn zoom_checks(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let tab = run_sql(ws, "SELECT n, n * 2 AS twice FROM generate_series(1, 40) n", cx).await;
    let grid = cx.update(|_, cx| tab.read(cx).grid.clone()).unwrap();
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.demo_toggle_side(cx)));
    pause(cx, 350).await;
    draw(cx);
    let bounds = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).result_bounds.get()).ok().flatten();
    let zoom = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).zoom_now(&tab, cx)).unwrap_or(-1.);
    let Some(before) = bounds(cx) else {
        report("the result panel is painted", false, String::new());
        return false;
    };
    let win = cx.update(|window, _| window.viewport_size()).unwrap();
    let explorer = cx.update(|_, cx| ws.read(cx).explorer_now()).unwrap_or(0.);
    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
    let t0 = std::time::Instant::now();
    press(cx, "cmd-shift-enter");
    let motion = !cx.update(|_, cx| cx.reduce_motion()).unwrap_or(false);
    pause(cx, 60).await;
    draw(cx);
    let mid = zoom(cx);
    let mid_b = bounds(cx);
    let in_time = crate::check::mid_possible(t0, crate::motion::ZOOM);
    pause(cx, 320).await;
    draw(cx);
    let after = bounds(cx);
    let landed = cx.update(|_, cx| ws.read(cx).zoom_tween.is_none() || ws.read(cx).zoom_now(&tab, cx) == 1.).unwrap_or(false);
    report("⇧⌘↩ on the grid zooms the result, animated", (!motion || !in_time || (mid > 0.05 && mid < 0.999)) && zoom(cx) == 1. && landed, format!("mid {mid} end {}{}", zoom(cx), if in_time { "" } else { " (frames too slow for a mid-way sample)" }));
    report(
        "mid-way the result has grown part of the way",
        !motion || !in_time || mid_b.zip(after).is_some_and(|(m, a)| m.top() < before.top() && m.top() > a.top()),
        format!("{:?} {:?} {:?}", before.top(), mid_b.map(|b| b.top()), after.map(|b| b.top())),
    );
    let title = crate::theme::metrics::TITLE_H;
    report(
        "zoomed: it covers the Run row, the editor and the side panel",
        after.is_some_and(|a| a.top() < before.top() && (a.top().as_f32() - title).abs() < 4. && (a.right() - win.width).abs() < px(2.) && a.right() > before.right()),
        format!("{before:?} → {after:?} (window {win:?})"),
    );
    report(
        "… but not the explorer nor the tabs (the title band)",
        after.is_some_and(|a| (a.left() - before.left()).abs() < px(1.) && a.left().as_f32() >= explorer - 1. && a.top().as_f32() >= title - 1.),
        format!("{after:?} explorer {explorer}"),
    );
    let side_kept = cx.update(|_, cx| ws.read(cx).side.is_some()).unwrap_or(false);
    report("the side panel stays open underneath (it comes back)", side_kept, String::new());
    let zoomed_icon = cx.update(|_, cx| tab.read(cx).zoomed).unwrap_or(false);
    report("the zoom button turns into its collapse control", zoomed_icon, String::new());
    let first = cx.update(|_, cx| tab.read(cx).result().map(|r| std::sync::Arc::as_ptr(r) as usize)).ok().flatten();
    press(cx, "cmd-enter");
    for _ in 0..100 {
        pause(cx, 50).await;
        let again = cx.update(|_, cx| tab.read(cx).result().map(|r| std::sync::Arc::as_ptr(r) as usize)).ok().flatten();
        if again != first && cx.update(|_, cx| tab.read(cx).run_phase().is_some_and(|p| p.is_finished())).unwrap_or(false) {
            break;
        }
    }
    let again = cx.update(|_, cx| tab.read(cx).result().map(|r| (std::sync::Arc::as_ptr(r) as usize, r.rows()))).ok().flatten();
    report("⌘↩ on the zoomed grid re-runs the tab's script", again.is_some_and(|(p, n)| Some(p) != first && n == 40) && zoom(cx) == 1., format!("{first:?} → {again:?}"));
    let other = new_tab(ws, cx);
    let other_zoomed = cx.update(|_, cx| other.read(cx).zoomed).unwrap_or(true);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.activate_tab(&tab, window, cx)));
    draw(cx);
    report("zoom is per tab", !other_zoomed && cx.update(|_, cx| tab.read(cx).zoomed).unwrap_or(false), String::new());
    let _ = cx.update(|window, cx| window.focus(&grid.focus_handle(cx), cx));
    press(cx, "escape");
    pause(cx, 340).await;
    draw(cx);
    let restored = bounds(cx);
    report("esc on the zoomed grid closes it; the layout is back", zoom(cx) == 0. && restored.is_some_and(|r| (r.top() - before.top()).abs() < px(1.) && (r.right() - before.right()).abs() < px(1.)), format!("{before:?} → {restored:?}"));
    let _ = cx.update(|_, cx| ws.update(cx, |w, _| w.side = None));
    draw(cx);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.toggle_result_zoom(window, cx)));
    pause(cx, 340).await;
    draw(cx);
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.demo_toggle_side(cx)));
    draw(cx);
    pause(cx, 340).await;
    draw(cx);
    report("another side view opening ends the zoom", !cx.update(|_, cx| tab.read(cx).zoomed).unwrap_or(true), String::new());
    let _ = cx.update(|_, cx| ws.update(cx, |w, _| w.side = None));
    draw(cx);
    ok
}

async fn motion_checks(ws: &Entity<Workspace>, id: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let off = cx
        .update(|_, cx| {
            ws.update(cx, |w, cx| {
                let mut p = dataman_core::ConnectionProfile::new("offline");
                (p.host, p.port) = ("127.0.0.1".into(), 9);
                let id = p.id.clone();
                let core = w.store.read(cx).core.clone();
                let _ = core.save_connection(p, None, None);
                w.store.update(cx, |s, cx| s.reload(cx));
                id
            })
        })
        .unwrap();
    pause(cx, 100).await;
    draw(cx);
    let (on, offl, tx, tx3) = cx
        .update(|_, cx| {
            let tree = ws.read(cx).tree.clone();
            let t = crate::theme::ActiveTheme::theme(&*cx);
            (tree.read(cx).demo_conn_label(id, cx), tree.read(cx).demo_conn_label(&off, cx), t.tx, t.tx3)
        })
        .unwrap();
    let _ = tx3;
    let tx4 = cx.update(|_, cx| crate::theme::ActiveTheme::theme(&*cx).tx4).unwrap();
    report(
        "a connection that is not connected is dim (tx4) and regular; a connected one keeps its look",
        on == Some((tx, true)) && offl == Some((tx4, false)),
        format!("{on:?} {offl:?}"),
    );
    let chips = cx.update(|_, cx| {
        let tree = ws.read(cx).tree.clone();
        (tree.read(cx).demo_conn_row(id).map(|r| r.1), tree.read(cx).demo_conn_row(&off).map(|r| r.1))
    });
    report("its colour chip is faded; a connected one's is not", chips.as_ref().ok() == Some(&(Some(false), Some(true))), format!("{chips:?}"));

    let long = format!(
        "could not connect to server: Connection refused\n\tIs the server running on host \"offline.db.internal\" (10.20.30.40) and accepting TCP/IP connections on port 9?\nDETAIL: {}\nHINT: check the host, the port and the firewall.",
        "a very long detail that goes on and on ".repeat(30)
    );
    // Expanding connects it (and fails at once: nothing listens on port 9); then the long error.
    let _ = cx.update(|_, cx| {
        let tree = ws.read(cx).tree.clone();
        tree.update(cx, |t, cx| t.expand_id(&format!("c:{off}"), cx));
    });
    for _ in 0..100 {
        pause(cx, 50).await;
        if cx.update(|_, cx| matches!(ws.read(cx).store.read(cx).states.get(&off), Some(crate::conns::ConnState::Failed(_)))).unwrap_or(true) {
            break;
        }
    }
    let _ = cx.update(|_, cx| {
        ws.update(cx, |w, cx| {
            w.store.update(cx, |s, cx| {
                s.states.insert(off.clone(), crate::conns::ConnState::Failed(long.clone()));
                cx.notify();
            })
        })
    });
    pause(cx, 100).await;
    draw(cx);
    let row = cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_conn_row(&off)).ok().flatten();
    report(
        "a failed connection's row and its error row carry the whole error on hover",
        row.as_ref().is_some_and(|(_, dim, tip, err)| *dim && tip.as_deref() == Some(long.as_str()) && err.as_ref().is_some_and(|(l, t)| !l.contains('\n') && t.as_deref() == Some(long.as_str()))),
        format!("{:?}", row.as_ref().map(|r| (r.0.clone(), r.1, r.2.as_ref().map(|t| t.len()), r.3.as_ref().map(|e| e.0.clone())))),
    );
    let (fit, w) = crate::ui::fit_tip(&long);
    report(
        "its tooltip is cut to fit: at most 360 pt wide, 14 lines, an ellipsis at the end",
        w <= crate::ui::TIP_MAX_W && fit.ends_with('…') && fit.lines().count() <= crate::ui::TIP_LINES && fit.starts_with("could not connect"),
        format!("{w} {} lines", fit.lines().count()),
    );
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.store.update(cx, |s, cx| s.forget(&off, cx))));

    // Tooltips can be hovered: the pointer moved from the control onto the tooltip keeps it, and
    // leaving both closes it.
    let tip = |cx: &mut AsyncWindowContext| crate::check::probed("tooltip", cx);
    let rail = crate::check::probed("rail-agents", cx);
    if let Some(r) = rail {
        crate::check::move_to(cx, crate::check::centre(r));
    }
    pause(cx, 800).await;
    let shown = tip(cx);
    if let (Some(r), Some(b)) = (rail, shown) {
        let (from, to) = (crate::check::centre(r), crate::check::centre(b));
        for k in 1..=4 {
            crate::check::move_to(cx, point(from.x + (to.x - from.x) * (k as f32 / 4.), from.y + (to.y - from.y) * (k as f32 / 4.)));
        }
    }
    pause(cx, 900).await;
    let kept = tip(cx);
    let win = cx.update(|window, _| window.viewport_size()).unwrap();
    crate::check::move_to(cx, point(win.width * 0.6, win.height * 0.35));
    pause(cx, 900).await;
    let gone = tip(cx);
    report(
        "a tooltip stays while the pointer is on it, and closes once it leaves the control and the tooltip",
        rail.is_some() && shown.is_some() && kept.is_some() && gone.is_none(),
        format!("rail {rail:?} shown {shown:?} kept {kept:?} gone {gone:?}"),
    );

    let motion = !cx.update(|_, cx| cx.reduce_motion()).unwrap_or(false);
    let sliding = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).demo_side_sliding()).unwrap_or(false);
    let history = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).side.is_some()).unwrap_or(false);
    let _ = cx.update(|_, cx| ws.update(cx, |w, _| w.side = None));
    draw(cx);
    pause(cx, 300).await;
    draw(cx);
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.demo_toggle_side(cx)));
    draw(cx);
    let opening = sliding(cx);
    pause(cx, 320).await;
    draw(cx);
    let landed = !sliding(cx) && history(cx);
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.demo_toggle_side(cx)));
    draw(cx);
    let closing = sliding(cx);
    pause(cx, 320).await;
    draw(cx);
    let gone = !sliding(cx) && !history(cx);
    report(
        "the side panel slides in and out like the sidebar",
        opening == motion && closing == motion && landed && gone,
        format!("opening {opening} landed {landed} closing {closing} gone {gone} (motion {motion})"),
    );

    let fill = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).explorer_now() / ws.read(cx).explorer_w).unwrap_or(-1.);
    let editor = cx.update(|_, cx| ws.read(cx).active_tab().unwrap().read(cx).editor.clone()).unwrap();
    let _ = cx.update(|window, cx| window.focus(&editor.focus_handle(cx), cx));
    let before = fill(cx);
    let t0 = std::time::Instant::now();
    key(cx, "cmd-b");
    pause(cx, 70).await;
    draw(cx);
    let mid = fill(cx);
    let in_time = crate::check::mid_possible(t0, std::time::Duration::from_millis(180));
    pause(cx, 300).await;
    draw(cx);
    let shut = fill(cx);
    key(cx, "cmd-b");
    pause(cx, 320).await;
    draw(cx);
    let back = fill(cx);
    report(
        "the explorer toggle's left part fills and empties with the explorer's slide",
        (before - 1.).abs() < 1e-3 && (!motion || !in_time || (mid > 0.01 && mid < 0.99)) && shut.abs() < 1e-3 && (back - 1.).abs() < 1e-3,
        format!("{before} → {mid} → {shut} → {back}{}", if in_time { "" } else { " (frames too slow for a mid-way sample)" }),
    );

    let pref = cx.update(|_, cx| crate::theme::pref(cx)).unwrap_or_default();
    let _ = cx.update(|_, cx| crate::theme::set_pref(dataman_core::ThemePref::Light, cx));
    let light = crate::dock_icon::shown_light();
    let _ = cx.update(|_, cx| crate::theme::set_pref(dataman_core::ThemePref::Dark, cx));
    let dark = crate::dock_icon::shown_light();
    let _ = cx.update(|_, cx| crate::theme::set_pref(pref, cx));
    report("the Dock icon is the light one in the light theme, the dark one in the dark theme", light == Some(true) && dark == Some(false), format!("{light:?} {dark:?}"));
    ok
}

fn press(cx: &mut AsyncWindowContext, k: &str) {
    draw(cx);
    let ks = crate::keymap::keystroke(k);
    input(cx, PlatformInput::KeyDown(KeyDownEvent { keystroke: ks.clone(), is_held: false, prefer_character_input: false }));
    input(cx, PlatformInput::KeyUp(KeyUpEvent { keystroke: ks }));
}

fn middle_click(cx: &mut AsyncWindowContext, p: Point<Pixels>) {
    mouse_move(cx, p, false);
    input(cx, PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Middle, position: p, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }));
    input(cx, PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Middle, position: p, modifiers: Modifiers::default(), click_count: 1 }));
}
