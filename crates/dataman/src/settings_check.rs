//! `settings-check`: ⌘, then each Settings row clicked, its effect checked where the app shows
//! one (theme, fonts, reduce motion on a new tab and the result zoom, logos, SQL formatting, rows
//! per table page), every choice saved to settings.toml and read back as a relaunch would.

use std::sync::Arc;

use gpui::{AsyncWindowContext, Entity, Focusable};
use dataman_core::config::Engine;
use dataman_core::{Core, Dirs, MemorySecrets};

use crate::app::Workspace;
use crate::check::*;

fn section(name: &str, cx: &mut AsyncWindowContext) -> bool {
    let ok = click_probe(name, cx);
    draw(cx);
    ok
}

fn settings(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> dataman_core::Settings {
    core(ws, cx).settings.get()
}

fn growing_after_new_tab(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let tab = cx.update(|window, cx| ws.update(cx, |w, cx| w.new_tab(window, cx))).unwrap();
    draw(cx);
    cx.update(|_, cx| ws.read(cx).tab_motion.growing(tab.entity_id())).unwrap_or(false)
}

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (pg, pw) = profile(Engine::Postgres);
    let up = connect(ws, &pg, &pw, cx).await;
    if !r.check("PostgreSQL connects", up.is_ok(), format!("{up:?}")) {
        return;
    }
    key(cx, "cmd-,");
    pause(cx, 200).await;
    let open = cx.update(|_, cx| ws.read(cx).settings_view.as_ref().map(|v| v.read(cx).demo_section())).ok().flatten();
    let modal = cx.update(|_, cx| ws.read(cx).demo_modal(cx)).unwrap_or_default();
    r.check("⌘, opens Settings on Appearance", open == Some("Appearance"), format!("{open:?} modal {modal:?}"));

    click_probe("th-light", cx);
    let light = cx.update(|_, cx| !crate::theme::ActiveTheme::theme(cx).is_dark()).unwrap_or(false);
    click_probe("th-dark", cx);
    let dark = cx.update(|_, cx| crate::theme::ActiveTheme::theme(cx).is_dark()).unwrap_or(false);
    r.check("Theme: Light, then Dark, applied at once", light && dark && settings(ws, cx).theme == dataman_core::ThemePref::Dark, format!("{light} {dark}"));

    let before = crate::theme::mono();
    click_probe("font-1", cx);
    let after = crate::theme::mono();
    click_probe("font-0", cx);
    r.check("Font: another family is used at once, and back", before != after && crate::theme::mono() == before, format!("{before} → {after}"));

    click_probe("fs-14", cx);
    let fs = settings(ws, cx).editor_font_size;
    click_probe("fs-13", cx);
    r.check("Code font size: 14 saved", fs == 14, fs);

    if crate::theme::translucency_works() {
        click_probe("tr-off", cx);
        let tr = settings(ws, cx).translucency;
        r.check("Window translucency: Off saved", tr == dataman_core::Translucency::Off, format!("{tr:?}"));
    }

    click_probe("rm-on", cx);
    let reduced = settings(ws, cx).reduce_motion;
    let toggle = |cx: &mut AsyncWindowContext| cx.update(|window, cx| ws.update(cx, |w, cx| w.toggle_settings(window, cx)));
    let _ = toggle(cx);
    let grows_reduced = growing_after_new_tab(ws, cx);
    let _ = toggle(cx);
    click_probe("rm-off", cx);
    let _ = toggle(cx);
    let grows = growing_after_new_tab(ws, cx);
    r.check("Reduce motion: a new tab is there at once; Animate: it grows in", reduced && !grows_reduced && grows, format!("saved {reduced}, reduced grows {grows_reduced}, animated grows {grows}"));
    let _ = toggle(cx);

    click_probe("logos-off", cx);
    let hidden = !crate::assets::logos_on() && !settings(ws, cx).db_logos;
    click_probe("logos-on", cx);
    r.check("Database logos: Hide, then Show", hidden && crate::assets::logos_on(), hidden);

    let format = |cx: &mut AsyncWindowContext| match active(ws, cx) {
        Some(tab) => {
            let editor = cx.update(|_, cx| tab.read(cx).editor.clone()).unwrap();
            let _ = cx.update(|window, cx| {
                editor.update(cx, |e, cx| e.set_text("SELECT c.id, c.name FROM customer c WHERE c.id IN (SELECT customer FROM orders WHERE total > 10) ORDER BY c.name", cx));
                window.focus(&editor.focus_handle(cx), cx);
            });
            key(cx, "alt-shift-f");
            cx.update(|_, cx| editor.read(cx).text().to_string()).unwrap_or_default()
        }
        None => String::new(),
    };
    let deepest = |s: &str| s.lines().map(|l| l.len() - l.trim_start().len()).max().unwrap_or(0);
    section("Editor", cx);
    click_probe("kc-1", cx);
    click_probe("in-2", cx);
    let _ = toggle(cx);
    let two = format(cx);
    let _ = toggle(cx);
    section("Editor", cx);
    click_probe("in-4", cx);
    let _ = toggle(cx);
    let four = format(cx);
    r.check(
        "Editor: keyword case lower and the indent shape ⌥⇧F's output",
        four.contains("select") && four.contains("from") && !four.contains("SELECT") && deepest(&four) > deepest(&two),
        format!("{two}\n--\n{four}"),
    );
    let _ = toggle(cx);

    section("Results grid", cx);
    click_probe("pg-200", cx);
    click_probe("pf-10000", cx);
    click_probe("cc-off", cx);
    let s = settings(ws, cx);
    r.check("Results grid: rows per page, rows fetched ahead and commit at once saved", s.page_size == 200 && s.prefetch_rows == 10_000 && !s.confirm_commit, format!("{} {} {}", s.page_size, s.prefetch_rows, s.confirm_commit));
    click_probe("cc-on", cx);
    let _ = toggle(cx);
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_relation(&pg.id, &pg.database, "public", "orders", window, cx)));
    let tab = active(ws, cx);
    if let Some(tab) = &tab {
        until(cx, 15, |cx| idle(tab, cx) && result(tab, cx).1 > 0).await;
    }
    let rows = tab.as_ref().map(|t| result(t, cx).1).unwrap_or(0);
    r.check("…and a table opens 200 rows to a page", rows == 200, rows);
    let dir = std::env::temp_dir().join(format!("dataman-e2e-exportdir-{}", std::process::id()));
    let _ = core(ws, cx).settings.update(|s| s.export_dir = Some(dir.display().to_string()));
    let _ = toggle(cx);
    section("Export", cx);
    click_probe("ed-reset", cx);
    r.check("Export: Reset puts the default folder back", settings(ws, cx).export_dir.is_none(), format!("{:?}", settings(ws, cx).export_dir));

    section("MCP", cx);
    click_probe("mcp-off", cx);
    r.check("MCP: Off saved", !settings(ws, cx).mcp_enabled, "");
    let hidden = probed("copy-stdio", cx).is_none();
    click_probe("mcp-on", cx);
    pause(cx, 50).await;
    let rows = (probed("copy-stdio", cx).is_some(), probed("open-agents", cx).is_some(), probed("mwl-1000", cx).is_none(), probed("open-mcp-log", cx).is_none());
    r.check("MCP: On, the Claude Code command and a way to Agents; the rest lives in Agents", settings(ws, cx).mcp_enabled && hidden && rows == (true, true, true, true), format!("{hidden} {rows:?}"));
    click_probe("open-agents", cx);
    pause(cx, 100).await;
    let agents = cx.update(|_, cx| ws.read(cx).view == dataman_core::layout::NavView::Agents && ws.read(cx).settings_view.is_none()).unwrap_or(false);
    r.check("Open Agents closes Settings on the Agents area", agents, "");
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.show_nav(dataman_core::layout::NavView::Explorer, window, cx)));
    let _ = toggle(cx);
    section("MCP", cx);
    click_probe("mcp-off", cx);

    section("About", cx);
    click_probe("upd-off", cx);
    r.check("About: update checks Off saved", !settings(ws, cx).update_check, "");

    key(cx, "escape");
    let closed = cx.update(|_, cx| ws.read(cx).settings_view.is_none()).unwrap_or(false);
    r.check("esc closes Settings", closed, "");

    let dirs = core(ws, cx).dirs.clone();
    let again = Core::new(Dirs::at(dirs.config.clone()), Arc::new(MemorySecrets::default())).map(|c| c.settings.get());
    let mine = settings(ws, cx);
    r.check(
        "a relaunch reads every choice back from settings.toml",
        again.as_ref().is_ok_and(|s| s.theme == mine.theme && s.translucency == mine.translucency && s.page_size == 200 && s.indent == 4 && s.keyword_case == mine.keyword_case && !s.update_check && !s.mcp_enabled),
        format!("{:?}", again.as_ref().map(|s| (s.page_size, s.indent, s.update_check)).map_err(|e| format!("{e:#}"))),
    );
}
