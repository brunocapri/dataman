//! `welcome-check`: the Welcome tab: its items by hand, clicked or reached with ↓ ↩; the agents'
//! commands and the prompt copied; a connection the agent saves, listed while the tab is open;
//! the tab itself, clicked, closed and opened again. `palette-check`: ⌘K and its scopes (commands, tables, tabs, favorites, connections,
//! settings) and ⌘P.

use gpui::{AsyncWindowContext, Entity, Focusable};
use dataman_core::config::Engine;

use crate::app::Workspace;
use crate::check::*;
use crate::conn_dialog::{ConnDialog, Step};
use crate::conns::ConnState;

fn dialog(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Entity<ConnDialog>> {
    cx.update(|_, cx| ws.read(cx).dialog.clone()).ok().flatten()
}

fn close_dialog(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.close_dialog(cx)));
    draw(cx);
}

pub async fn run_welcome(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let first = cx.update(|_, cx| ws.read(cx).welcome_front).unwrap_or(false);
    r.check("nothing saved: the Welcome tab, in the strip and in front", first && probed("tab-welcome", cx).is_some(), "");

    click_probe("welcome-0", cx);
    let step = dialog(ws, cx).map(|d| cx.update(|_, cx| d.read(cx).step()).unwrap());
    r.check("New connection opens the sheet on the engines", step == Some(Step::Database), format!("{step:?}"));
    close_dialog(ws, cx);

    let (pg, pw) = profile(Engine::Postgres);
    set_clipboard(&format!("postgresql://{}:{pw}@{}:{}/{}", pg.user, pg.host, pg.port, pg.database), cx);
    click_probe("welcome-1", cx);
    let got = dialog(ws, cx).map(|d| cx.update(|_, cx| d.read(cx).current(cx)).unwrap());
    r.check(
        "Paste a connection URL: the clipboard's URL prefills the sheet",
        got.as_ref().is_some_and(|(p, gpw)| p.port == pg.port && p.database == pg.database && *gpw == pw),
        format!("{:?}", got.map(|(p, _)| (p.host, p.port, p.database))),
    );
    close_dialog(ws, cx);
    set_clipboard("", cx);

    click_probe("welcome-2", cx);
    let got = localhost_sheet(ws, cx);
    r.check("Connect to localhost:5432 prefills that", got.as_ref().is_some_and(|p| p.host == "localhost" && p.port == 5432 && p.engine == Engine::Postgres), format!("{:?}", got.map(|p| (p.host, p.port))));
    close_dialog(ws, cx);

    let file = std::env::temp_dir().join(format!("dataman-e2e-{}.sql", std::process::id()));
    std::fs::write(&file, "SELECT 'from a file' AS origin;\n").unwrap();
    crate::sql_picker::set_test_paths(vec![file.clone()]);
    click_probe("welcome-3", cx);
    until(cx, 5, |cx| active(ws, cx).is_some_and(|t| cx.update(|_, cx| t.read(cx).editor.read(cx).text().contains("from a file")).unwrap_or(false))).await;
    let text = active(ws, cx).map(|t| cx.update(|_, cx| t.read(cx).editor.read(cx).text().to_string()).unwrap()).unwrap_or_default();
    r.check("Open a .sql file: its text in a tab", text.contains("from a file"), &text);
    let _ = std::fs::remove_file(&file);
    welcome_tab(ws, r, cx).await;

    let _ = cx.update(|window, cx| {
        let w = ws.read(cx).welcome.clone();
        window.focus(&w.focus_handle(cx), cx);
    });
    for _ in 0..5 {
        key(cx, "up");
    }
    key(cx, "down");
    key(cx, "down");
    key(cx, "enter");
    let got = localhost_sheet(ws, cx);
    r.check("↑ to the top, ↓ ↓ to Connect to localhost:5432, ↩ prefills it", got.is_some_and(|p| p.host == "localhost" && p.port == 5432), "no sheet");
    close_dialog(ws, cx);

    agent_copy(ws, r, cx).await;
    from_agent(ws, r, cx).await;
}

fn welcome_focused(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|window, cx| ws.read(cx).welcome.focus_handle(cx).contains_focused(window, cx)).unwrap_or(false)
}

fn welcome_open(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|_, cx| ws.read(cx).welcome_open).unwrap_or(false)
}

/// The Welcome tab beside the query tabs: clicked to the front and away, closed with ⌘W, opened
/// again from the palette.
async fn welcome_tab(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    r.check("the file's tab comes to the front; the Welcome tab stays in the strip", !welcome(ws, cx) && probed("tab-welcome", cx).is_some(), "");
    click_probe("tab-welcome", cx);
    r.check("…the Welcome tab clicked: the welcome in front again, with the focus", welcome(ws, cx) && welcome_focused(ws, cx), "");
    let tab = cx.update(|_, cx| ws.read(cx).panes[0].tab_bounds.borrow().first().copied()).ok().flatten();
    if let Some(b) = tab {
        click_with(cx, centre(b), gpui::MouseButton::Left, gpui::Modifiers::default(), 1);
    }
    r.check("…a query tab clicked: that tab in front, its editor focused", tab.is_some() && !welcome(ws, cx) && editor_focused(ws, cx), format!("{tab:?}"));
    click_probe("tab-welcome", cx);
    let tabs = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap_or(0);
    let before = tabs(cx);
    key(cx, "cmd-w");
    draw(cx);
    r.check(
        "⌘W on the Welcome tab closes it, not a query tab: the editor in front, focused",
        !welcome(ws, cx) && !welcome_open(ws, cx) && probed("tab-welcome", cx).is_none() && tabs(cx) == before && editor_focused(ws, cx),
        format!("{before} tabs, now {}", tabs(cx)),
    );
    let (label, _) = palette(ws, "cmd-k", ">welcome", cx).await;
    key(cx, "enter");
    draw(cx);
    r.check("the palette's Welcome opens it again, in front, with the focus", label == "Welcome" && welcome(ws, cx) && probed("tab-welcome", cx).is_some() && welcome_focused(ws, cx), &label);
}

fn localhost_sheet(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<dataman_core::ConnectionProfile> {
    dialog(ws, cx).map(|d| cx.update(|_, cx| d.read(cx).current(cx).0).unwrap())
}

/// ADD MCP: each agent's Copy gives what adds Dataman to it; ASK YOUR AGENT's, the prompt. Side by
/// side, the two panels end level.
async fn agent_copy(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let listing = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).welcome.read(cx).demo_agents()).ok().flatten();
    until(cx, 5, |cx| listing(cx).is_some()).await;
    let Some(l) = listing(cx) else {
        r.check("ADD MCP lists the agents", false, "never listed");
        return;
    };
    let tops: Vec<Option<f32>> = l.rows.iter().map(|row| probed(&format!("welcome-agent-copy-{}", row.agent.id), cx).map(|b| f32::from(b.top()))).collect();
    let drawn: Vec<f32> = tops.iter().flatten().copied().collect();
    r.check(
        "ADD MCP: a row with its Copy for every agent, above the prompt, in the listing's order",
        l.rows.len() == dataman_mcp::agents::AGENTS.len() && drawn.len() == l.rows.len() && drawn.is_sorted() && probed("welcome-prompt-copy", cx).zip(drawn.last()).is_some_and(|(p, last)| *last < f32::from(p.top())),
        format!("{tops:?}"),
    );
    // The first row, then the last: the flash goes with the click.
    for row in [&l.rows[0], &l.rows[l.rows.len() - 1]] {
        set_clipboard("", cx);
        let id = format!("welcome-agent-copy-{}", row.agent.id);
        click_probe(&id, cx);
        let flash = probed(&format!("{id}-copied"), cx).is_some();
        r.check(&format!("Copy on {} puts its command on the clipboard, and says Copied", row.agent.label), clipboard(cx) == row.copy && flash, format!("{:?} flash {flash}", clipboard(cx)));
    }
    set_clipboard("", cx);
    click_probe("welcome-prompt-copy", cx);
    r.check("ASK YOUR AGENT: Copy gives the prompt", clipboard(cx) == crate::welcome::PROMPT, clipboard(cx));
    let panels = probed("welcome-hand", cx).zip(probed("welcome-agent", cx));
    r.check(
        "the two panels side by side, as tall as each other",
        panels.is_some_and(|(h, a)| a.left() > h.right() && (h.top() - a.top()).abs() < gpui::px(0.5) && (h.bottom() - a.bottom()).abs() < gpui::px(0.5)),
        format!("{panels:?}"),
    );
}

fn welcome(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|_, cx| ws.read(cx).welcome_front).unwrap_or(false)
}

/// FROM YOUR AGENT as it reads: each row's connection, and what it says of its connect.
fn arrivals(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Vec<(String, Option<String>)> {
    cx.update(|_, cx| ws.read(cx).welcome.read(cx).demo_arrivals()).unwrap_or_default()
}

/// A connection as the agent's `create_connection` saves one (agent-made; with the password
/// the agent was given, if any), then the app told, as the tool tells it.
fn agent_saves(ws: &Entity<Workspace>, mut p: dataman_core::ConnectionProfile, password: Option<&str>, cx: &mut AsyncWindowContext) -> String {
    let core = core(ws, cx);
    p.agent_made = true;
    p.save_password = password.is_some();
    if let Some(pw) = password {
        core.secrets.set(&p.id, pw).expect("the check's own secret store");
    }
    core.config.upsert(p.clone()).expect("the check's own connections file");
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.connections_on_disk(cx)));
    p.id
}

/// As Settings › Connections' auto-connect does for a connection that arrives.
fn quietly(ws: &Entity<Workspace>, id: &str, cx: &mut AsyncWindowContext) {
    let ids = [id.to_string()];
    let _ = cx.update(|_, cx| ws.read(cx).store.clone().update(cx, |s, cx| s.demo_connect_quietly(&ids, cx)));
}

/// Every connection deleted: the welcome of a first run again.
async fn forget_all(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let ids: Vec<String> = cx.update(|_, cx| ws.read(cx).store.read(cx).profiles.iter().map(|p| p.id.clone()).collect()).unwrap_or_default();
    for id in ids {
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.request_delete_connections(vec![id], true, window, cx)));
    }
    until(cx, 5, |cx| welcome(ws, cx) && arrivals(ws, cx).is_empty()).await
}

fn editor_focused(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|window, cx| ws.read(cx).active_tab().is_some_and(|t| t.read(cx).editor.focus_handle(cx).is_focused(window))).unwrap_or(false)
}

fn focus_welcome(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        let w = ws.read(cx).welcome.clone();
        window.focus(&w.focus_handle(cx), cx);
    });
}

/// The welcome in the smallest window (960 by 600, the explorer open): the agent's column goes
/// under the items and FROM YOUR AGENT, inside the window; scrolled to its end, ↑ from the agent's
/// row to the first item shows it in the frame that ↑ draws; wider again, side by side.
async fn narrow(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let size = cx.update(|window, _| window.viewport_size()).expect("the window");
    let stacked = |cx: &mut AsyncWindowContext| probed("welcome-prompt-copy", cx).zip(probed("welcome-from-0", cx)).is_some_and(|(prompt, row)| prompt.top() > row.bottom());
    let _ = cx.update(|window, _| window.resize(gpui::size(gpui::px(960.), gpui::px(600.))));
    let under = until(cx, 5, stacked).await;
    let width = cx.update(|window, _| window.viewport_size().width).expect("the window");
    let prompt = probed("welcome-prompt-copy", cx);
    r.check("960 wide: the agent's column under the items and FROM YOUR AGENT, inside the window", under && prompt.is_some_and(|b| b.right() <= width), format!("{prompt:?} in {width:?}"));
    // What shows: under the welcome's top, above the status bar drawn over the window's foot.
    let view = |cx: &mut AsyncWindowContext| cx.update(|window, cx| (ws.read(cx).welcome.read(cx).demo_view().top(), window.viewport_size().height - gpui::px(crate::theme::metrics::STATUS_H))).ok();
    focus_welcome(ws, cx);
    let _ = cx.update(|_, cx| ws.read(cx).welcome.read(cx).demo_scroll_to_end());
    draw(cx);
    let hidden = !probed("welcome-0", cx).zip(view(cx)).is_some_and(|(b, (top, bottom))| b.top() >= top && b.bottom() <= bottom);
    // From the agent's row, one ↑ per item to the first: the last frame is the one that ↑ drew
    // (`key` draws once, and the probes are that frame's), no frame asked for by anything else.
    let items = if probed("welcome-5", cx).is_some() { 6 } else { 5 };
    for _ in 0..items {
        key(cx, "up");
    }
    let seen = crate::ui::probed("welcome-0").zip(view(cx)).is_some_and(|(b, (top, bottom))| b.top() >= top && b.bottom() <= bottom);
    r.check("…and short, scrolled to its end: ↑ from the agent's row to the first item shows it in the frame that ↑ draws", hidden && seen, format!("hidden first {hidden}, then seen {seen}"));
    for _ in 0..items {
        key(cx, "down");
    }
    let _ = cx.update(|window, _| window.resize(size));
    let beside = until(cx, 5, |cx| probed("welcome-prompt-copy", cx).zip(probed("welcome-0", cx)).is_some_and(|(prompt, hand)| prompt.left() > hand.right())).await;
    r.check("…wider again, side by side, the items on the left", beside, "");
}

fn tab_on(ws: &Entity<Workspace>, id: &str, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|_, cx| {
        let w = ws.read(cx);
        let now = w.store.read(cx).connection(id);
        w.active_tab().and_then(|t| t.read(cx).connection().cloned()).zip(now).is_some_and(|(on, now)| std::sync::Arc::ptr_eq(&on, &now))
    })
    .unwrap_or(false)
}

/// The agent edits a saved connection, as `update_connection` writes it, and the app is told.
fn agent_edits(ws: &Entity<Workspace>, id: &str, edit: impl FnOnce(&mut dataman_core::ConnectionProfile), cx: &mut AsyncWindowContext) {
    let core = core(ws, cx);
    let mut p = core.config.get(id).expect("saved");
    edit(&mut p);
    core.config.upsert(p).expect("the check's own connections file");
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.connections_on_disk(cx)));
}

/// What the agent saves while the Welcome tab is open: listed under FROM YOUR AGENT, each row as
/// its connect went, until the tab closes; the work to the front when the user opens one.
async fn from_agent(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    r.check("FROM YOUR AGENT is not there while nothing came", probed("welcome-from-0", cx).is_none(), "");

    // Nothing listens on this port.
    let mut p = dataman_core::ConnectionProfile::new("shop");
    (p.host, p.port, p.user, p.database) = ("127.0.0.1".into(), 9, "app".into(), "shop".into());
    click_probe("welcome-0", cx);
    let focus = |cx: &mut AsyncWindowContext| cx.update(|window, cx| window.focused(cx)).ok().flatten();
    let before = focus(cx);
    let on_welcome = cx.update(|window, cx| ws.read(cx).welcome.focus_handle(cx).contains_focused(window, cx)).unwrap_or(true);
    let shop = agent_saves(ws, p, None, cx);
    draw(cx);
    r.check(
        "the agent's first save while a sheet is open: the sheet keeps the focus",
        dialog(ws, cx).is_some() && before.is_some() && !on_welcome && focus(cx) == before,
        format!("{before:?} on the welcome {on_welcome}"),
    );
    close_dialog(ws, cx);
    let shows = until(cx, 5, |cx| probed("welcome-from-0", cx).is_some()).await;
    let tree = cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_conn_row(&shop).map(|r| r.0)).ok().flatten();
    let picked = cx.update(|_, cx| ws.read(cx).welcome.read(cx).demo_selected_arrival()).ok().flatten();
    r.check(
        "a connection the agent saves: under FROM YOUR AGENT, selected, not tried yet so saying nothing; in the explorer; the welcome stays",
        shows && welcome(ws, cx) && arrivals(ws, cx) == [(shop.clone(), None)] && picked == Some(0) && tree.as_deref() == Some("shop"),
        format!("{:?} {picked:?} {tree:?}", arrivals(ws, cx)),
    );
    let last = if probed("welcome-5", cx).is_some() { "welcome-5" } else { "welcome-4" };
    let left = probed("welcome-from-0", cx).zip(probed(last, cx)).zip(probed("welcome-prompt-copy", cx)).is_some_and(|((row, item), prompt)| row.top() > item.bottom() && row.right() <= prompt.left());
    r.check("FROM YOUR AGENT: in the left column, under the items", left, "");
    narrow(ws, r, cx).await;

    let mut q = dataman_core::ConnectionProfile::new("shop-staging");
    (q.host, q.port) = ("127.0.0.1".into(), 9);
    let staging = agent_saves(ws, q, None, cx);
    let two = arrivals(ws, cx).len() == 2;
    let _ = core(ws, cx).config.remove(&staging);
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.connections_on_disk(cx)));
    let listed: Vec<String> = arrivals(ws, cx).into_iter().map(|a| a.0).collect();
    r.check("a second one joins; deleted, it leaves the list", two && listed == [shop.clone()] && probed("welcome-from-1", cx).is_none(), format!("{listed:?}"));

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.focus_region(crate::app::Region::Explorer, window, cx)));
    key(cx, "down");
    key(cx, "up");
    key(cx, "cmd-i");
    let edit = dialog(ws, cx).map(|d| cx.update(|_, cx| (d.read(cx).profile_id().to_string(), d.read(cx).demo_asks_password())).unwrap());
    r.check("⌘I in the explorer on it (never connected, no password): the Edit sheet, not the password one", edit == Some((shop.clone(), false)), format!("{edit:?}"));
    key(cx, "escape");
    let in_tree = |cx: &mut AsyncWindowContext| cx.update(|window, cx| ws.read(cx).tree.read(cx).demo_has_focus(window)).unwrap_or(false);
    r.check("…esc: the focus back in the explorer it was opened from", dialog(ws, cx).is_none() && in_tree(cx), "");

    quietly(ws, &shop, cx);
    let failed = until(cx, 10, |cx| matches!(state(ws, &shop, cx), Some(ConnState::Failed { .. }))).await;
    let said = arrivals(ws, cx).first().and_then(|a| a.1.clone());
    r.check(
        "its connect refused: the row says why, in a line",
        failed && said.as_ref().is_some_and(|s| !s.trim().is_empty() && s != "Needs your password" && !s.contains('\n')),
        format!("{said:?}"),
    );
    click_probe("welcome-from-0", cx);
    let sheet = dialog(ws, cx).map(|d| cx.update(|_, cx| (d.read(cx).profile_id().to_string(), d.read(cx).is_new(), d.read(cx).step(), d.read(cx).demo_status(cx).0)).unwrap());
    r.check(
        "…and clicked, New connection's steps on it, at Connection, saying why as the row does",
        sheet.as_ref().is_some_and(|(id, new, step, status)| *id == shop && *new && *step == Step::Connection && said.as_ref().is_some_and(|s| *status == format!("failed: {s}"))),
        format!("{sheet:?} {said:?}"),
    );
    close_dialog(ws, cx);
    r.check("…closed without connecting, the welcome is still there", welcome(ws, cx) && probed("welcome-from-0", cx).is_some(), "");

    agent_edits(ws, &shop, |p| p.database = "shop2".into(), cx);
    let retried = matches!(state(ws, &shop, cx), Some(ConnState::Connecting));
    let again = until(cx, 10, |cx| matches!(state(ws, &shop, cx), Some(ConnState::Failed { .. }))).await;
    r.check("the agent edits it after it failed: tried again, on its own, and the welcome stays", retried && again && welcome(ws, cx), format!("{:?}", arrivals(ws, cx)));

    focus_welcome(ws, cx);
    let sel = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).welcome.read(cx).demo_selected_arrival()).ok().flatten();
    for _ in 0..8 {
        key(cx, "down");
    }
    let bottom = sel(cx);
    key(cx, "up");
    let above = sel(cx);
    key(cx, "down");
    let back = sel(cx);
    key(cx, "enter");
    let sheet = dialog(ws, cx).map(|d| cx.update(|_, cx| d.read(cx).profile_id().to_string()).unwrap());
    r.check(
        "↑↓: the agent's rows come after the items; ↩ on one opens it",
        bottom == Some(0) && above.is_none() && back == Some(0) && sheet.as_deref() == Some(shop.as_str()),
        format!("{bottom:?} {above:?} {back:?} {sheet:?}"),
    );
    close_dialog(ws, cx);

    let shown = cx.update(|window, cx| ws.update(cx, |w, cx| w.show_place(dataman_mcp::Place::AgentsGeneral, window, cx))).ok();
    draw(cx);
    let on_agents = cx.update(|_, cx| ws.read(cx).view == dataman_core::layout::NavView::Agents).unwrap_or(false);
    r.check(
        "show_in_dataman to the Agents page over the Welcome tab: the page shows over it",
        shown == Some(Ok(())) && on_agents && probed("welcome-from-0", cx).is_none() && welcome(ws, cx),
        format!("{shown:?} {on_agents}"),
    );
    let on_welcome = |cx: &mut AsyncWindowContext| welcome_focused(ws, cx);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.focus_editor(window, cx)));
    let hidden = on_welcome(cx);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.show_nav(dataman_core::layout::NavView::Explorer, window, cx)));
    let back = until(cx, 5, |cx| probed("welcome-from-0", cx).is_some()).await;
    r.check(
        "…back to the explorer, the welcome with its row (no focus put on it while it was hidden)",
        !hidden && back && arrivals(ws, cx).len() == 1,
        format!("focus while hidden {hidden} {:?}", arrivals(ws, cx)),
    );
    let settings_round = async |cx: &mut AsyncWindowContext| {
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.toggle_settings(window, cx)));
        draw(cx);
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.toggle_settings(window, cx)));
        draw(cx);
        draw(cx);
    };
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.focus_region(crate::app::Region::Explorer, window, cx)));
    settings_round(cx).await;
    let kept = in_tree(cx) && !on_welcome(cx);
    focus_welcome(ws, cx);
    settings_round(cx).await;
    let back = on_welcome(cx);
    r.check("Settings opened and closed over it: the focus back where it was, the explorer or the welcome", kept && back, format!("explorer kept {kept}, welcome back {back}"));

    let row = cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_conn_bounds(&shop)).ok().flatten();
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.focus_region(crate::app::Region::Explorer, window, cx)));
    key(cx, "down");
    key(cx, "up");
    let selected = cx.update(|_, cx| ws.read(cx).tree.read(cx).selected_connection()).ok().flatten();
    let kept_keys = welcome(ws, cx);
    if let Some(b) = row {
        click_with(cx, centre(b), gpui::MouseButton::Right, gpui::Modifiers::default(), 1);
        key(cx, "escape");
    }
    let kept_menu = welcome(ws, cx);
    r.check(
        "in the explorer, selecting it (↑↓, a right click) is not opening it: the welcome stays",
        row.is_some() && selected.as_deref() == Some(shop.as_str()) && kept_keys && kept_menu,
        format!("{row:?} {selected:?} keys {kept_keys} menu {kept_menu}"),
    );
    if let Some(b) = row {
        move_to(cx, centre(b));
    }
    let tip = until(cx, 3, |cx| probed("tooltip", cx).is_some()).await;
    if let Some(b) = row {
        click_with(cx, centre(b), gpui::MouseButton::Right, gpui::Modifiers::default(), 1);
    }
    draw(cx);
    let menu_over = probed("tooltip", cx).is_none();
    key(cx, "escape");
    key(cx, "cmd-i");
    let edit = dialog(ws, cx).is_some_and(|d| cx.update(|_, cx| !d.read(cx).demo_asks_password()).unwrap_or(false));
    r.check("its error's tooltip up, a right click: the menu, the tooltip gone; ⌘I: the Edit sheet", tip && menu_over && edit, format!("tip {tip} gone under the menu {menu_over} edit {edit}"));
    close_dialog(ws, cx);
    // Cold again (a tooltip right after another shows at once), then ⌘I before the delay is up.
    if let Some(b) = row {
        move_to(cx, gpui::point(b.right() + gpui::px(400.), b.top()));
    }
    until(cx, 3, |_| crate::ui::tip_stats().2.is_none_or(|ago| ago > std::time::Duration::from_millis(600))).await;
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.focus_region(crate::app::Region::Explorer, window, cx)));
    if let Some(b) = row {
        move_to(cx, centre(b));
    }
    key(cx, "cmd-i");
    let sheet = dialog(ws, cx).is_some();
    let over_sheet = until(cx, 1, |cx| probed("tooltip", cx).is_some()).await;
    r.check("⌘I before its tooltip's delay: the sheet, and no tooltip shows over it after", sheet && !over_sheet, format!("sheet {sheet} tooltip over it {over_sheet}"));
    key(cx, "escape");
    if let Some(b) = row {
        click_with(cx, centre(b), gpui::MouseButton::Left, gpui::Modifiers::default(), 1);
        click_with(cx, centre(b), gpui::MouseButton::Left, gpui::Modifiers::default(), 2);
    }
    let gone = until(cx, 5, |cx| !welcome(ws, cx)).await;
    let error_row = until(cx, 10, |cx| cx.update(|_, cx| !ws.read(cx).tree.read(cx).demo_children(&format!("c:{shop}")).is_empty()).unwrap_or(false)).await;
    r.check(
        "…double-clicked (refused, not for a password): open on its error and Retry, no sheet, the work in front",
        gone && error_row && dialog(ws, cx).is_none(),
        format!("{gone} error row {error_row} sheet {}", dialog(ws, cx).is_some()),
    );
    until(cx, 10, |cx| matches!(state(ws, &shop, cx), Some(ConnState::Failed { .. }))).await;
    click_probe("close-welcome", cx);
    let closed = !welcome_open(ws, cx) && probed("tab-welcome", cx).is_none() && arrivals(ws, cx).is_empty();
    agent_edits(ws, &shop, |p| p.database = "shop3".into(), cx);
    r.check(
        "the Welcome tab closed by its ×, its rows gone: an edit from outside of a failed one, auto-connect off, forgets its failure, not tried again",
        closed && state(ws, &shop, cx).is_none(),
        format!("closed {closed}"),
    );
    let mut later = dataman_core::ConnectionProfile::new("cache");
    (later.host, later.port) = ("127.0.0.1".into(), 9);
    agent_saves(ws, later, None, cx);
    r.check("one the agent saves with the Welcome tab closed: not listed, the tab not opened", !welcome(ws, cx) && !welcome_open(ws, cx) && arrivals(ws, cx).is_empty(), format!("{:?}", arrivals(ws, cx)));
    r.check("deleting every connection brings the welcome back", forget_all(ws, cx).await, "");

    // The test server, with the password the agent was given; a blank tab behind the Welcome tab,
    // as on a first launch.
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.new_tab(window, cx)));
    click_probe("tab-welcome", cx);
    let (pg, pw) = profile(Engine::Postgres);
    let orders = agent_saves(ws, dataman_core::ConnectionProfile { id: dataman_core::ConnectionProfile::new("orders").id, name: "orders".into(), ..pg.clone() }, Some(&pw), cx);
    quietly(ws, &orders, cx);
    let up = until(cx, 15, |cx| is_connected(ws, &orders, cx)).await;
    r.check(
        "connected on its own: the row says nothing more, the welcome stays, the tab on it",
        up && welcome(ws, cx) && arrivals(ws, cx) == [(orders.clone(), None)] && tab_on(ws, &orders, cx),
        format!("{:?} tab {}", arrivals(ws, cx), tab_on(ws, &orders, cx)),
    );
    let row_id = format!("c:{orders}");
    let _ = cx.update(|_, cx| ws.read(cx).tree.clone().update(cx, |t, cx| t.expand_id(&row_id, cx)));
    draw(cx);
    r.check("the app expanding its row in the explorer is not the user opening it: the welcome stays", welcome(ws, cx), "");
    let before = cx.update(|_, cx| ws.read(cx).store.read(cx).connection(&orders)).ok().flatten();
    agent_edits(ws, &orders, |p| p.name = "orders (by agent)".into(), cx);
    let again = until(cx, 15, |cx| cx.update(|_, cx| ws.read(cx).store.read(cx).connection(&orders)).ok().flatten().zip(before.clone()).is_some_and(|(now, old)| !std::sync::Arc::ptr_eq(&now, &old))).await;
    r.check(
        "the agent edits it (update_connection): it reconnects on its own, the tab on the new connection, the welcome stays",
        again && tab_on(ws, &orders, cx) && welcome(ws, cx) && arrivals(ws, cx) == [(orders.clone(), None)],
        format!("{again} tab {} {:?}", tab_on(ws, &orders, cx), arrivals(ws, cx)),
    );
    let returns = agent_saves(ws, dataman_core::ConnectionProfile { id: dataman_core::ConnectionProfile::new("returns").id, name: "returns".into(), ..pg.clone() }, Some(&pw), cx);
    quietly(ws, &returns, cx);
    let both = until(cx, 15, |cx| is_connected(ws, &returns, cx)).await;
    focus_welcome(ws, cx);
    key(cx, "down");
    key(cx, "enter");
    let gone = until(cx, 5, |cx| !welcome(ws, cx)).await;
    r.check(
        "two up on their own, the blank tab on the first: ↓ ↩ on the second, the work in front, the tab on that one, the editor focused",
        both && gone && tab_on(ws, &returns, cx) && editor_focused(ws, cx),
        format!("{both} {gone} tab {} focused {}", tab_on(ws, &returns, cx), editor_focused(ws, cx)),
    );
    // Every connection removed outside Dataman while the user types in the editor's find bar:
    // the welcome comes back, and the focus with it (the find bar is not drawn any more).
    key(cx, "cmd-f");
    let in_find = cx.update(|window, cx| ws.read(cx).active_tab().is_some_and(|t| t.read(cx).editor.read(cx).find_has_focus(window, cx))).unwrap_or(false);
    let core_now = core(ws, cx);
    for id in [&orders, &returns] {
        core_now.config.remove(id).expect("the check's own connections file");
    }
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.connections_on_disk(cx)));
    let on_welcome = |cx: &mut AsyncWindowContext| cx.update(|window, cx| ws.read(cx).welcome.focus_handle(cx).contains_focused(window, cx)).unwrap_or(false);
    let taken = until(cx, 5, |cx| welcome(ws, cx) && on_welcome(cx)).await;
    r.check("every connection removed outside while typing in the find bar: the welcome back, with the focus", in_find && taken, format!("in the find bar first {in_find}"));

    // The Welcome tab in front and focused, a sheet opened from it, an agent's tab opening under
    // the sheet comes to the front: esc gives the focus to what shows, the tab, not the welcome.
    let mut p = dataman_core::ConnectionProfile::new("shop");
    (p.host, p.port) = ("127.0.0.1".into(), 9);
    agent_saves(ws, p, None, cx);
    focus_welcome(ws, cx);
    click_probe("welcome-0", cx);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.new_tab(window, cx)));
    let ended = !welcome(ws, cx) && dialog(ws, cx).is_some();
    key(cx, "escape");
    let shown = until(cx, 5, |cx| editor_focused(ws, cx)).await;
    r.check("…a tab come to the front under a sheet, esc: the focus on the tab's editor, not on the welcome behind it", ended && shown && !on_welcome(cx), format!("ended {ended}"));
    forget_all(ws, cx).await;

    // ⌘W on the Welcome tab in front, with the agent's row: the tab closes, not the one behind.
    let mut p = dataman_core::ConnectionProfile::new("shop");
    (p.host, p.port) = ("127.0.0.1".into(), 9);
    agent_saves(ws, p, None, cx);
    focus_welcome(ws, cx);
    let tabs = cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap_or(0);
    let listed = arrivals(ws, cx).len() == 1;
    key(cx, "cmd-w");
    draw(cx);
    r.check(
        "⌘W on the Welcome tab with the agent's row: it closes and its row goes, the tabs as they were",
        listed && !welcome(ws, cx) && !welcome_open(ws, cx) && arrivals(ws, cx).is_empty() && cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap_or(0) == tabs,
        format!("{tabs} tabs {:?}", arrivals(ws, cx)),
    );
    forget_all(ws, cx).await;

    // An agent's tab taking the blank tab behind the Welcome tab while the user is in the
    // explorer: it comes to the front, the explorer keeps the focus.
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.new_tab(window, cx)));
    click_probe("tab-welcome", cx);
    let agents = agent_saves(ws, dataman_core::ConnectionProfile { id: dataman_core::ConnectionProfile::new("orders").id, name: "orders".into(), ..pg.clone() }, Some(&pw), cx);
    quietly(ws, &agents, cx);
    until(cx, 15, |cx| is_connected(ws, &agents, cx)).await;
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.focus_region(crate::app::Region::Explorer, window, cx)));
    let kept = welcome(ws, cx) && in_tree(cx);
    let made = cx.update(|window, cx| ws.update(cx, |w, cx| w.demo_agent_tab(&agents, window, cx))).unwrap_or(false);
    draw(cx);
    let agent_tab = cx.update(|_, cx| ws.read(cx).active_tab().is_some_and(|t| t.read(cx).agent.is_some())).unwrap_or(false);
    r.check(
        "an agent's tab taking the blank tab while the user is in the explorer: it comes to the front, the explorer keeps the focus",
        kept && made && agent_tab && !welcome(ws, cx) && in_tree(cx),
        format!("kept {kept} made {made} agent tab {agent_tab} in the explorer {}", in_tree(cx)),
    );
    forget_all(ws, cx).await;

    // Connecting on its own, its row filtered out, ↩ on it: the connect is the user's now, so
    // when it fails for its password its sheet opens.
    let asking = agent_saves(ws, dataman_core::ConnectionProfile { id: dataman_core::ConnectionProfile::new("billing").id, name: "billing".into(), user: "welcome_e2e".into(), ..pg.clone() }, None, cx);
    let _ = cx.update(|_, cx| ws.read(cx).tree.clone().update(cx, |t, cx| t.set_filter("zzz-nothing-matches", cx)));
    quietly(ws, &asking, cx);
    let connecting = matches!(state(ws, &asking, cx), Some(ConnState::Connecting));
    focus_welcome(ws, cx);
    key(cx, "enter");
    let sheet = until(cx, 15, |cx| dialog(ws, cx).is_some_and(|d| cx.update(|_, cx| d.read(cx).profile_id() == asking && d.read(cx).demo_asks_password()).unwrap_or(false))).await;
    let _ = cx.update(|_, cx| ws.read(cx).tree.clone().update(cx, |t, cx| t.set_filter("", cx)));
    r.check("connecting on its own, filtered out of the explorer, ↩ on it: failing for its password, its sheet opens", connecting && sheet, format!("connecting first {connecting}"));
    close_dialog(ws, cx);
    forget_all(ws, cx).await;

    // Never tried (auto-connect off), its row hidden by the explorer's filter: ↩ on it connects.
    let hidden = agent_saves(ws, dataman_core::ConnectionProfile { id: dataman_core::ConnectionProfile::new("ledger").id, name: "ledger".into(), ..pg.clone() }, Some(&pw), cx);
    // Its delete sheet cancelled over the Welcome tab: the focus back in the explorer.
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.focus_region(crate::app::Region::Explorer, window, cx)));
    key(cx, "down");
    key(cx, "up");
    if let Some(b) = cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_conn_bounds(&hidden)).ok().flatten() {
        click_with(cx, centre(b), gpui::MouseButton::Right, gpui::Modifiers::default(), 1);
    }
    let _ = cx.update(|window, cx| ws.read(cx).tree.clone().update(cx, |t, cx| t.demo_menu_run(crate::tree_menu::Act::DeleteConnection, window, cx)));
    let asked = until(cx, 5, |cx| probed("dc-cancel", cx).is_some()).await;
    click_probe("dc-cancel", cx);
    draw(cx);
    r.check(
        "its delete sheet cancelled over the Welcome tab: the explorer has the focus back, the welcome its row",
        asked && in_tree(cx) && !on_welcome(cx) && welcome(ws, cx) && arrivals(ws, cx).len() == 1,
        format!("asked {asked} in the explorer {}", in_tree(cx)),
    );
    let _ = cx.update(|_, cx| ws.read(cx).tree.clone().update(cx, |t, cx| t.set_filter("zzz-nothing-matches", cx)));
    let no_row = cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_conn_row(&hidden).is_none()).unwrap_or(false);
    focus_welcome(ws, cx);
    key(cx, "enter");
    let up = until(cx, 15, |cx| is_connected(ws, &hidden, cx) && tab_on(ws, &hidden, cx)).await;
    let _ = cx.update(|_, cx| ws.read(cx).tree.clone().update(cx, |t, cx| t.set_filter("", cx)));
    r.check("never tried, its row filtered out of the explorer, ↩ on it: it connects, the tab on it", no_row && up && !welcome(ws, cx), format!("row hidden {no_row}"));

    // The last one deleted from the explorer, through its sheet: the welcome, with the focus.
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.focus_region(crate::app::Region::Explorer, window, cx)));
    key(cx, "down");
    key(cx, "up");
    if let Some(b) = cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_conn_bounds(&hidden)).ok().flatten() {
        click_with(cx, centre(b), gpui::MouseButton::Right, gpui::Modifiers::default(), 1);
    }
    let _ = cx.update(|window, cx| ws.read(cx).tree.clone().update(cx, |t, cx| t.demo_menu_run(crate::tree_menu::Act::DeleteConnection, window, cx)));
    let asked = until(cx, 5, |cx| probed("dc-delete", cx).is_some()).await;
    click_probe("dc-delete", cx);
    let back = until(cx, 5, |cx| welcome(ws, cx) && on_welcome(cx)).await;
    r.check("the last connection deleted from the explorer, confirmed in its sheet: the welcome, with the focus", asked && back, format!("asked {asked}"));

    // Saved with a password that is not there (the Keychain lost it): none went, so none was tried.
    let mut lost = dataman_core::ConnectionProfile { id: dataman_core::ConnectionProfile::new("ledger").id, name: "ledger".into(), user: "welcome_e2e".into(), ..pg.clone() };
    (lost.agent_made, lost.save_password) = (true, true);
    let lost_id = lost.id.clone();
    core(ws, cx).config.upsert(lost).expect("the check's own connections file");
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.connections_on_disk(cx)));
    quietly(ws, &lost_id, cx);
    let refused = until(cx, 15, |cx| matches!(state(ws, &lost_id, cx), Some(ConnState::Failed { .. }))).await;
    r.check("saved with a password the Keychain does not have: none went, the row says it needs yours", refused && arrivals(ws, cx) == [(lost_id.clone(), Some("Needs your password".into()))], format!("{:?}", arrivals(ws, cx)));
    forget_all(ws, cx).await;

    // A user ~/.pgpass has no line for, and no password: the server asks for one.
    let asks = agent_saves(ws, dataman_core::ConnectionProfile { id: dataman_core::ConnectionProfile::new("billing").id, name: "billing".into(), user: "welcome_e2e".into(), ..pg.clone() }, None, cx);
    quietly(ws, &asks, cx);
    let refused = until(cx, 15, |cx| matches!(state(ws, &asks, cx), Some(ConnState::Failed { .. }))).await;
    r.check("a server that wants a password: the row says it needs yours", refused && arrivals(ws, cx) == [(asks.clone(), Some("Needs your password".into()))], format!("{:?}", arrivals(ws, cx)));
    click_probe("welcome-from-0", cx);
    let d = dialog(ws, cx);
    let sheet = d.as_ref().map(|d| cx.update(|window, cx| (d.read(cx).profile_id() == asks, d.read(cx).is_new(), d.read(cx).step(), d.read(cx).password_focus(cx).is_focused(window), d.read(cx).demo_asks_password(), d.read(cx).demo_status(cx))).unwrap());
    r.check(
        "…clicked, New connection's steps on it, at Sign-in, the password focused, with no failure said nor the field marked (none was tried)",
        sheet.as_ref().is_some_and(|(same, new, step, focused, asks, (status, marked))| *same && *new && *step == Step::SignIn && *focused && *asks && status == "untested" && !marked),
        format!("{sheet:?}"),
    );
    let type_in = async |password: &str, cx: &mut AsyncWindowContext| {
        let Some(d) = dialog(ws, cx) else { return };
        let _ = cx.update(|_, cx| {
            d.update(cx, |d, cx| {
                d.demo_set_field(3, &pg.user, cx);
                d.demo_set_field(4, password, cx);
            })
        });
        // ↩ is the footer's last button: on to the last step, then Save. (A click could land on
        // the sheet fading out, which has the same buttons.)
        for _ in 0..4 {
            if cx.update(|_, cx| d.read(cx).step()).ok() == Some(Step::Mcp) {
                break;
            }
            key(cx, "enter");
        }
        key(cx, "enter");
    };
    type_in("not-the-password", cx).await;
    let wrong = until(cx, 15, |cx| matches!(state(ws, &asks, cx), Some(ConnState::Failed { password_tried: true, .. })) && dialog(ws, cx).is_some()).await;
    let said = arrivals(ws, cx).first().and_then(|a| a.1.clone());
    let shown = dialog(ws, cx).map(|d| cx.update(|_, cx| (d.read(cx).demo_asks_password(), d.read(cx).demo_status(cx).0, d.read(cx).is_new() && d.read(cx).step() == Step::SignIn)).unwrap());
    r.check(
        "…a wrong one: the steps again, at Sign-in, on the password, saying why (the server's code and words); the row says the same",
        wrong && shown.as_ref().is_some_and(|(asks, status, steps)| *asks && *steps && status.contains("28P01")) && said.as_ref().is_some_and(|s| shown.as_ref().is_some_and(|(_, status, _)| *status == format!("failed: {s}"))),
        format!("{shown:?} {said:?}"),
    );
    let method = dialog(ws, cx).map(|d| cx.update(|_, cx| (d.read(cx).demo_method(cx).1, d.read(cx).demo_field(4, cx))).unwrap());
    r.check(
        "…and its sheet does not offer to keep the refused password: User & password, the field empty",
        method.as_ref().is_some_and(|(m, field)| *m == "User & password" && field.is_empty()),
        format!("{method:?}"),
    );
    type_in(&pw, cx).await;
    let gone = until(cx, 15, |cx| !welcome(ws, cx) && is_connected(ws, &asks, cx) && editor_focused(ws, cx)).await;
    r.check(
        "…typed and saved: it connects, the work in front, the editor focused",
        gone && is_connected(ws, &asks, cx) && editor_focused(ws, cx),
        format!("{gone} connected {} focused {}", is_connected(ws, &asks, cx), editor_focused(ws, cx)),
    );
    forget_all(ws, cx).await;

    // show_in_dataman on one the agent saved with no password: New connection's steps, at its
    // password; saved, the same connection (no second one), still the agent's.
    let asks = agent_saves(ws, dataman_core::ConnectionProfile { id: dataman_core::ConnectionProfile::new("billing").id, name: "billing".into(), user: "welcome_e2e".into(), ..pg.clone() }, None, cx);
    let shown = cx.update(|window, cx| ws.update(cx, |w, cx| w.show_place(dataman_mcp::Place::Connection(asks.clone()), window, cx))).ok();
    let at = dialog(ws, cx).map(|d| cx.update(|window, cx| (d.read(cx).profile_id() == asks, d.read(cx).is_new(), d.read(cx).step(), d.read(cx).password_focus(cx).is_focused(window))).unwrap());
    r.check(
        "show_in_dataman to one the agent saved with no password: New connection's steps on it, at Sign-in, the password focused",
        shown == Some(Ok(())) && at == Some((true, true, Step::SignIn, true)),
        format!("{shown:?} {at:?}"),
    );
    type_in(&pw, cx).await;
    let up = until(cx, 15, |cx| is_connected(ws, &asks, cx)).await;
    let saved: Vec<(String, bool)> = core(ws, cx).config.connections().into_iter().filter(|p| p.name == "billing").map(|p| (p.id, p.agent_made)).collect();
    r.check("…typed and saved: the same connection, still the agent's, connected", up && saved == [(asks.clone(), true)], format!("up {up} {saved:?}"));
    forget_all(ws, cx).await;

    let mut p = dataman_core::ConnectionProfile::new("shop");
    (p.host, p.port) = ("127.0.0.1".into(), 9);
    let shop = agent_saves(ws, p, None, cx);
    let mine = dataman_core::ConnectionProfile::new_file(Engine::Sqlite, std::env::temp_dir().join(format!("dataman-e2e-mine-{}.sqlite", std::process::id())).to_string_lossy());
    let mine_id = mine.id.clone();
    core(ws, cx).config.upsert(mine).expect("the check's own connections file");
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.connections_on_disk(cx)));
    let listed = arrivals(ws, cx).into_iter().map(|a| a.0).collect::<Vec<_>>() == [shop.clone()];
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.request_delete_connections(vec![mine_id], true, window, cx)));
    draw(cx);
    r.check(
        "one saved that is not the agent's: not listed; the Welcome tab stays in front with the agent's row, its deletion too",
        listed && welcome(ws, cx) && arrivals(ws, cx).into_iter().map(|a| a.0).collect::<Vec<_>>() == [shop.clone()],
        format!("{:?}", arrivals(ws, cx)),
    );
    forget_all(ws, cx).await;

    // Long addresses, a wrong password's FATAL line and "Needs your password": the name whole,
    // the address cut first, the state keeping what it needs, the whole of it in a tooltip.
    let long_db = "analytics_replica_internal_warehouse_reporting";
    let wrong = agent_saves(ws, dataman_core::ConnectionProfile { id: dataman_core::ConnectionProfile::new("warehouse").id, name: "warehouse".into(), database: long_db.into(), ..pg.clone() }, Some("not-the-password"), cx);
    let asks = agent_saves(ws, dataman_core::ConnectionProfile { id: dataman_core::ConnectionProfile::new("reports").id, name: "reports".into(), database: long_db.into(), user: "welcome_e2e".into(), ..pg.clone() }, None, cx);
    quietly(ws, &wrong, cx);
    quietly(ws, &asks, cx);
    until(cx, 15, |cx| [&wrong, &asks].iter().all(|id| matches!(state(ws, id, cx), Some(ConnState::Failed { .. })))).await;
    let keeps = gpui::px(19. * crate::theme::ADVANCE * crate::theme::text::UI - 1.);
    let size = cx.update(|window, _| window.viewport_size()).expect("the window");
    let mut seen = Vec::new();
    for w in [1400., 1290.] {
        let _ = cx.update(|window, _| window.resize(gpui::size(gpui::px(w), size.height)));
        until(cx, 5, |cx| cx.update(|window, _| window.viewport_size().width == gpui::px(w)).unwrap_or(false)).await;
        draw(cx);
        for i in 0..2 {
            let (row, at, st) = (probed(&format!("welcome-from-{i}"), cx), probed(&format!("welcome-from-at-{i}"), cx), probed(&format!("welcome-from-state-{i}"), cx));
            let ok = row.zip(at).zip(st).is_some_and(|((row, at), st)| at.right() <= row.right() && st.right() <= row.right() && st.size.width >= keeps && at.size.width >= gpui::px(90.));
            seen.push((w, i, ok, at.map(|b| b.size.width), st.map(|b| b.size.width)));
        }
    }
    let said = arrivals(ws, cx);
    r.check(
        "long addresses, 1400 and 1290 wide: in the row, the address cut first, the state still says it all (FATAL line, Needs your password)",
        seen.iter().all(|s| s.2) && said.iter().any(|a| a.1.as_deref().is_some_and(|s| s.contains("28P01"))) && said.iter().any(|a| a.1.as_deref() == Some("Needs your password")),
        format!("{seen:?} {said:?}"),
    );
    // From a place with no tooltip (none up), onto the state: its tooltip, however long it takes.
    if let Some(b) = probed("welcome-from-0", cx) {
        move_to(cx, gpui::point(b.left(), b.bottom() + gpui::px(120.)));
    }
    until(cx, 5, |cx| probed("tooltip", cx).is_none()).await;
    if let Some(b) = probed("welcome-from-state-0", cx) {
        move_to(cx, centre(b));
    }
    let tip = until(cx, 10, |cx| probed("tooltip", cx).is_some()).await;
    r.check("…and the state's whole text in its tooltip", tip, "");
    let _ = cx.update(|window, _| window.resize(size));
    forget_all(ws, cx).await;

    // ↩ in the explorer on one that wants a password: its sheet on the password, as a click.
    let asks = agent_saves(ws, dataman_core::ConnectionProfile { id: dataman_core::ConnectionProfile::new("billing").id, name: "billing".into(), user: "welcome_e2e".into(), ..pg.clone() }, None, cx);
    quietly(ws, &asks, cx);
    until(cx, 15, |cx| matches!(state(ws, &asks, cx), Some(ConnState::Failed { .. }))).await;
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.focus_region(crate::app::Region::Explorer, window, cx)));
    key(cx, "down");
    key(cx, "up");
    let kept = welcome(ws, cx);
    key(cx, "enter");
    let on_password = dialog(ws, cx).is_some_and(|d| cx.update(|_, cx| d.read(cx).profile_id() == asks && d.read(cx).demo_asks_password()).unwrap_or(false));
    r.check("in the explorer, ↩ on one that wants a password: its sheet on the password, and the welcome gives way", kept && on_password && !welcome(ws, cx), format!("kept {kept} {on_password}"));
    close_dialog(ws, cx);
    forget_all(ws, cx).await;

    // A DuckDB file is locked while open: an edit after it failed does not open it on its own.
    let duck = agent_saves(ws, dataman_core::ConnectionProfile::new_file(Engine::DuckDb, "/nonexistent-dataman-e2e/x.duckdb"), None, cx);
    let _ = cx.update(|_, cx| ws.read(cx).store.clone().update(cx, |s, cx| s.connect(&duck, None, cx)));
    let failed = until(cx, 10, |cx| matches!(state(ws, &duck, cx), Some(ConnState::Failed { .. }))).await;
    agent_edits(ws, &duck, |p| p.name = "x (by agent)".into(), cx);
    let left = state(ws, &duck, cx).is_none();
    r.check("a DuckDB file the agent edits after it failed: not opened on its own", failed && left, format!("{failed} {:?}", state(ws, &duck, cx).is_some()));
    forget_all(ws, cx).await;

    // The agent's and another in one reload: no list, and ↑↓ on nothing gone.
    let mut a = dataman_core::ConnectionProfile::new("shop");
    (a.host, a.port, a.agent_made) = ("127.0.0.1".into(), 9, true);
    let a_id = a.id.clone();
    let mut b = dataman_core::ConnectionProfile::new("mine");
    (b.host, b.port) = ("127.0.0.1".into(), 9);
    let core = core(ws, cx);
    core.config.upsert(a).expect("the check's own connections file");
    core.config.upsert(b).expect("the check's own connections file");
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.connections_on_disk(cx)));
    let sel = cx.update(|_, cx| ws.read(cx).welcome.read(cx).demo_selected_arrival()).ok().flatten();
    let listed: Vec<String> = arrivals(ws, cx).into_iter().map(|a| a.0).collect();
    r.check("the agent's and another in one reload: only the agent's listed, and selected", welcome(ws, cx) && listed == [a_id.clone()] && sel == Some(0), format!("{listed:?} {sel:?}"));
    forget_all(ws, cx).await;

    // Saved in the app (a duplicate, an import: the store reloaded at once), not the agent's.
    let mut p = dataman_core::ConnectionProfile::new("shop");
    (p.host, p.port) = ("127.0.0.1".into(), 9);
    agent_saves(ws, p, None, cx);
    let kept = welcome(ws, cx);
    focus_welcome(ws, cx);
    let mut dup = dataman_core::ConnectionProfile::new("shop copy");
    (dup.host, dup.port) = ("127.0.0.1".into(), 9);
    core.config.upsert(dup).expect("the check's own connections file");
    let _ = cx.update(|_, cx| ws.read(cx).store.clone().update(cx, |s, cx| s.reload(cx)));
    draw(cx);
    r.check(
        "one saved in the app (a duplicate, an import), not the agent's: the Welcome tab stays in front, with the focus and the agent's row",
        kept && welcome(ws, cx) && welcome_focused(ws, cx) && arrivals(ws, cx).len() == 1,
        format!("kept {kept} {:?}", arrivals(ws, cx)),
    );
    forget_all(ws, cx).await;

    let mut p = dataman_core::ConnectionProfile::new("shop");
    (p.host, p.port) = ("127.0.0.1".into(), 9);
    agent_saves(ws, p, None, cx);
    let kept = welcome(ws, cx);
    let file = std::env::temp_dir().join(format!("dataman-e2e-held-{}.sql", std::process::id()));
    std::fs::write(&file, "SELECT 'while kept' AS origin;\n").unwrap();
    crate::sql_picker::set_test_paths(vec![file.clone()]);
    click_probe("welcome-3", cx);
    let opened = until(cx, 5, |cx| active(ws, cx).is_some_and(|t| cx.update(|_, cx| t.read(cx).editor.read(cx).text().contains("while kept")).unwrap_or(false))).await;
    r.check("Open a .sql file from the Welcome tab: the file's tab comes to the front", kept && opened && !welcome(ws, cx), format!("kept {kept} opened {opened}"));
    let _ = std::fs::remove_file(&file);
}

fn palette_open(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|_, cx| ws.read(cx).palette.is_some()).unwrap_or(false)
}

async fn palette(ws: &Entity<Workspace>, keys: &str, query: &str, cx: &mut AsyncWindowContext) -> (String, usize) {
    if palette_open(ws, cx) {
        key(cx, "escape");
    }
    key(cx, keys);
    pause(cx, 100).await;
    typ(cx, query);
    pause(cx, 300).await;
    cx.update(|_, cx| ws.read(cx).palette.as_ref().map(|p| p.read(cx).demo_state()).map(|s| (s.0.to_string(), s.2))).ok().flatten().unwrap_or_default()
}

pub async fn run_palette(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (pg, pw) = profile(Engine::Postgres);
    let up = connect(ws, &pg, &pw, cx).await;
    if !r.check("PostgreSQL connects", up.is_ok(), format!("{up:?}")) {
        return;
    }
    pause(cx, 500).await;
    let (label, _) = palette(ws, "cmd-k", ">theme: light", cx).await;
    key(cx, "enter");
    let light = cx.update(|_, cx| !crate::theme::ActiveTheme::theme(cx).is_dark()).unwrap_or(false);
    r.check("⌘K, \">theme: light\", ↩ runs the command", label == "Theme: light" && light && !palette_open(ws, cx), &label);
    let _ = cx.update(|_, cx| crate::theme::set_pref(dataman_core::ThemePref::Dark, cx));

    let (label, _) = palette(ws, "cmd-k", "@orders", cx).await;
    key(cx, "enter");
    let tab = active(ws, cx);
    if let Some(t) = &tab {
        until(cx, 15, |cx| result(t, cx).1 > 0).await;
    }
    let rows = tab.as_ref().map(|t| result(t, cx).1).unwrap_or(0);
    r.check("\"@orders\" finds the table; ↩ opens its rows", label.contains("orders") && rows > 0, format!("{label} {rows}"));
    let orders = tab;

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.new_tab(window, cx)));
    let (label, n) = palette(ws, "cmd-k", "#orders", cx).await;
    key(cx, "enter");
    let back = active(ws, cx);
    r.check("\"#\" lists the tabs; ↩ goes to the one picked", n >= 1 && back.is_some() && back == orders, format!("{label} {n}"));

    let core = core(ws, cx);
    let mut fav = dataman_core::Favorite::new("E2E answer", "SELECT 42 AS answer");
    fav.connection_id = Some(pg.id.clone());
    let _ = core.favorites.add(fav);
    let (label, _) = palette(ws, "cmd-k", "*e2e answer", cx).await;
    key(cx, "enter");
    pause(cx, 200).await;
    let shown = cx.update(|_, cx| ws.read(cx).view == dataman_core::layout::NavView::Favorites).unwrap_or(false);
    r.check("\"*\" finds a favorite; ↩ shows it in the Favorites sidebar", label == "E2E answer" && shown, &label);
    palette(ws, "cmd-k", "*e2e answer", cx).await;
    key(cx, "cmd-enter");
    pause(cx, 300).await;
    let text = active(ws, cx).map(|t| cx.update(|_, cx| t.read(cx).editor.read(cx).text().to_string()).unwrap()).unwrap_or_default();
    r.check("…and ⌘↩ opens it in a tab", text.contains("SELECT 42"), &text);

    let (label, _) = palette(ws, "cmd-k", &format!(":{}", pg.name), cx).await;
    key(cx, "escape");
    r.check("\":\" lists the connections", label == pg.name, &label);

    let (_, n) = palette(ws, "cmd-k", "xyzzy-nothing", cx).await;
    key(cx, "escape");
    r.check("a query nothing matches lists nothing; esc closes", n == 0 && !palette_open(ws, cx), n);

    let (label, n) = palette(ws, "cmd-p", "orders", cx).await;
    key(cx, "escape");
    r.check("⌘P (Go to) finds tables and tabs by name", n >= 1 && label.contains("orders"), format!("{label} {n}"));

    let section_of = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).settings_view.as_ref().map(|v| v.read(cx).demo_section())).ok().flatten();
    for (query, want, section, why) in [
        (",rows fetched", "Rows fetched ahead", "Results grid", "a setting's name"),
        ("prefetch", "Rows fetched ahead", "Results grid", "another word for it, in the All scope"),
        (",connect on start", "Connect on start", "Connections", "a row of another section"),
        (",results grid", "Results grid", "Results grid", "a section's name"),
        (",keyboard", "Keyboard", "Keyboard", "a section with no rows"),
    ] {
        let (label, _) = palette(ws, "cmd-k", query, cx).await;
        key(cx, "enter");
        draw(cx);
        let at = section_of(cx);
        let closed = !palette_open(ws, cx);
        r.check(&format!("⌘K, {why}: {query:?}, ↩ opens Settings on {section}"), label == want && at == Some(section) && closed, format!("{label:?} {at:?} closed {closed}"));
        key(cx, "escape");
    }
    r.check("esc closes Settings after a jump", section_of(cx).is_none(), format!("{:?}", section_of(cx)));

    let (label, _) = palette(ws, "cmd-k", ",explorer view", cx).await;
    key(cx, "enter");
    let in_view = until(cx, 5, |cx| {
        let page = cx.update(|_, cx| ws.read(cx).settings_view.as_ref().map(|v| v.read(cx).demo_page())).ok().flatten();
        page.zip(probed("ev-1", cx)).is_some_and(|(page, row)| row.top() >= page.top() && row.bottom() <= page.bottom())
    })
    .await;
    r.check("the last row of the longest section is in view on arrival", label == "Explorer view" && in_view, format!("{label:?} in view {in_view}"));
    key(cx, "escape");

    let (_, n) = palette(ws, "cmd-k", ",shared in the project", cx).await;
    key(cx, "escape");
    r.check("a row that does not show now (no project open) is not offered", n == 0, n);
    let (_, n) = palette(ws, "cmd-k", ",", cx).await;
    key(cx, "escape");
    r.check("\",\" lists the sections and the rows", n >= 9 + 15, n);

    let _ = cx.update(|window, cx| {
        if let Some(t) = ws.read(cx).active_tab() {
            let e = t.read(cx).editor.clone();
            window.focus(&e.focus_handle(cx), cx);
        }
    });
}
