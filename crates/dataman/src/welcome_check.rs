//! `welcome-check`: the first-run screen's items, clicked or reached with ↓ ↩. `palette-check`:
//! ⌘K and its scopes (commands, tables, tabs, favorites, connections), ⌘P, and the shortcuts
//! sheet.

use gpui::{AsyncWindowContext, Entity, Focusable};
use dataman_core::config::Engine;

use crate::app::Workspace;
use crate::check::*;
use crate::conn_dialog::{ConnDialog, Step};

fn dialog(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Entity<ConnDialog>> {
    cx.update(|_, cx| ws.read(cx).dialog.clone()).ok().flatten()
}

fn close_dialog(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.close_dialog(cx)));
    draw(cx);
}

pub async fn run_welcome(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let first = cx.update(|_, cx| ws.read(cx).first_run(cx)).unwrap_or(false);
    r.check("no connection: the welcome screen", first, "");

    click_probe("welcome-0", cx);
    let step = dialog(ws, cx).map(|d| cx.update(|_, cx| d.read(cx).step()).unwrap());
    r.check("New connection opens the sheet on the engines", step == Some(Step::Engine), format!("{step:?}"));
    close_dialog(ws, cx);

    let (pg, pw) = profile(Engine::Postgres);
    set_clipboard(&format!("postgresql://{}:{pw}@{}:{}/{}", pg.user, pg.host, pg.port, pg.database), cx);
    click_probe("welcome-2", cx);
    let got = dialog(ws, cx).map(|d| cx.update(|_, cx| d.read(cx).current(cx)).unwrap());
    r.check(
        "Paste a connection URL: the clipboard's URL prefills the sheet",
        got.as_ref().is_some_and(|(p, gpw)| p.port == pg.port && p.database == pg.database && *gpw == pw),
        format!("{:?}", got.map(|(p, _)| (p.host, p.port, p.database))),
    );
    close_dialog(ws, cx);
    set_clipboard("", cx);

    click_probe("welcome-3", cx);
    let got = dialog(ws, cx).map(|d| cx.update(|_, cx| d.read(cx).current(cx).0).unwrap());
    r.check("Connect to localhost:5432 prefills that", got.as_ref().is_some_and(|p| p.host == "localhost" && p.port == 5432 && p.engine == Engine::Postgres), format!("{:?}", got.map(|p| (p.host, p.port))));
    close_dialog(ws, cx);

    let file = std::env::temp_dir().join(format!("dataman-e2e-{}.sql", std::process::id()));
    std::fs::write(&file, "SELECT 'from a file' AS origin;\n").unwrap();
    crate::sql_picker::set_test_paths(vec![file.clone()]);
    click_probe("welcome-4", cx);
    until(cx, 5, |cx| active(ws, cx).is_some_and(|t| cx.update(|_, cx| t.read(cx).editor.read(cx).text().contains("from a file")).unwrap_or(false))).await;
    let text = active(ws, cx).map(|t| cx.update(|_, cx| t.read(cx).editor.read(cx).text().to_string()).unwrap()).unwrap_or_default();
    r.check("Open a .sql file: its text in a tab", text.contains("from a file"), &text);
    let _ = std::fs::remove_file(&file);

    let _ = cx.update(|window, cx| {
        let w = ws.read(cx).welcome.clone();
        window.focus(&w.focus_handle(cx), cx);
    });
    for _ in 0..5 {
        key(cx, "down");
    }
    key(cx, "enter");
    let sheet = cx.update(|_, cx| ws.read(cx).shortcuts_open).unwrap_or(false);
    key(cx, "escape");
    let gone = !cx.update(|_, cx| ws.read(cx).shortcuts_open).unwrap_or(true);
    r.check("↓ to Keyboard shortcuts and ↩ opens the sheet; esc closes it", sheet && gone, format!("{sheet} {gone}"));

    click_probe("welcome-1", cx);
    let section = cx.update(|_, cx| ws.read(cx).settings_view.as_ref().map(|v| v.read(cx).demo_section())).ok().flatten();
    r.check("Add with an agent opens Settings › MCP", section == Some("MCP"), format!("{section:?}"));
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

    key(cx, "cmd-shift-/");
    let sheet = cx.update(|_, cx| ws.read(cx).shortcuts_open).unwrap_or(false);
    key(cx, "escape");
    r.check("⇧⌘/ opens the shortcuts sheet; esc closes it", sheet && !cx.update(|_, cx| ws.read(cx).shortcuts_open).unwrap_or(true), sheet);
    let _ = cx.update(|window, cx| {
        if let Some(t) = ws.read(cx).active_tab() {
            let e = t.read(cx).editor.clone();
            window.focus(&e.focus_handle(cx), cx);
        }
    });
}
