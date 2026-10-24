//! `project-check`: File › Open Project with no server. A folder whose `.dataman/project.toml`
//! a teammate wrote opens from the welcome screen and brings its connection (a SQLite file
//! inside the folder), favorite and SQL format; the gear under the robot opens Settings ›
//! Project, where what is shared changes the file; a connection made here joins it, an edit
//! "pulled" into the file reaches the app, and projects off stops it all.

use gpui::{AsyncWindowContext, Entity};
use dataman_core::config::Engine;
use dataman_core::format::KeywordCase;
use dataman_core::project::{self, ProjectFile};

use crate::app::Workspace;
use crate::check::{Report, click_probe, core, draw, key, pause, probed, typ, until};

fn snap(name: &str, cx: &mut AsyncWindowContext) {
    let Ok(dir) = std::env::var("DATAMAN_SHOT_DIR") else { return };
    let path = format!("{dir}/project-{name}.png");
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
    });
}

fn file(root: &std::path::Path) -> Option<ProjectFile> {
    ProjectFile::parse(&std::fs::read_to_string(project::file_in(root)).ok()?).ok()
}

fn names(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Vec<String> {
    cx.update(|_, cx| ws.read(cx).store.read(cx).profiles.iter().map(|p| p.name.clone()).collect()).unwrap_or_default()
}

async fn palette_label(query: &str, cx: &mut AsyncWindowContext, ws: &Entity<Workspace>) -> String {
    key(cx, "cmd-k");
    pause(cx, 100).await;
    typ(cx, query);
    pause(cx, 300).await;
    let label = cx.update(|_, cx| ws.read(cx).palette.as_ref().map(|p| p.read(cx).demo_state().0)).ok().flatten().unwrap_or_default();
    key(cx, "escape");
    label
}

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let dir = std::env::temp_dir().join(format!("dataman-project-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(project::DIR)).unwrap();
    let root = std::fs::canonicalize(&dir).unwrap();
    let db = root.join("shop.sqlite");
    let _ = dataman_core::import::create_database(Engine::Sqlite, &db);
    // What a teammate committed: the database file relative to the folder, no password.
    let teammate = format!(
        "version = 1\n\n[editor]\nkeyword_case = \"lower\"\nindent = 4\nconfirm_commit = true\n\n[[connection]]\nid = \"team-shop\"\nname = \"Team shop\"\nengine = \"sqlite\"\npath = \"shop.sqlite\"\n\n[[favorite]]\nid = \"team-fav\"\nname = \"Team orders\"\nsql = \"SELECT 1\"\nconnection_id = \"team-shop\"\n"
    );
    std::fs::write(project::file_in(&root), teammate).unwrap();
    let core = core(ws, cx);

    let first = cx.update(|_, cx| ws.read(cx).first_run(cx)).unwrap_or(false);
    r.check("no connection: the welcome screen", first, "");
    snap("welcome", cx);
    crate::sql_picker::set_test_paths(vec![root.clone()]);
    let clicked = click_probe("welcome-6", cx);
    r.check("the welcome screen offers Open a project folder", clicked, "no welcome-6");
    until(cx, 5, |cx| names(ws, cx).contains(&"Team shop".to_string())).await;
    let profile = core.config.get("team-shop");
    let want = dataman_core::config::tilde(&db.to_string_lossy());
    r.check(
        "the project's connection arrives, its file resolved in this folder",
        profile.as_ref().is_some_and(|p| p.path.as_deref() == Some(want.as_str()) && !p.save_password),
        format!("{:?}", profile.map(|p| p.path)),
    );
    let favs: Vec<String> = core.favorites.list().into_iter().map(|f| f.name).collect();
    r.check("…and its favorite", favs.contains(&"Team orders".to_string()), format!("{favs:?}"));
    let s = core.settings.get();
    r.check("…and its SQL format, over the local one", (s.keyword_case, s.indent) == (KeywordCase::Lower, 4), format!("{:?} {}", s.keyword_case, s.indent));
    let open = cx.update(|_, cx| crate::project_ui::projects(cx).open).ok().flatten();
    r.check("the project is open", open.as_deref() == root.file_name().and_then(|n| n.to_str()), format!("{open:?}"));
    r.check("an existing project opens without Settings", cx.update(|_, cx| ws.read(cx).settings_view.is_none()).unwrap_or(false), "");

    snap("opened", cx);
    let label = palette_label(">open project", cx, ws).await;
    r.check("the palette has Open project…", label == "Open project…", &label);

    let gear = click_probe("rail-settings", cx);
    let shown = cx.update(|_, cx| ws.read(cx).settings_view.is_some()).unwrap_or(false);
    r.check("the gear under the robot opens Settings", gear && shown, format!("{gear} {shown}"));
    let view = cx.update(|_, cx| ws.read(cx).settings_view.clone()).ok().flatten();
    if let Some(v) = &view {
        let _ = cx.update(|_, cx| v.update(cx, |v, cx| v.show_section("project", cx)));
    }
    draw(cx);
    let section = view.as_ref().and_then(|v| cx.update(|_, cx| v.read(cx).demo_section()).ok());
    r.check("Settings › Project", section == Some("Project"), format!("{section:?}"));
    snap("settings", cx);

    click_probe("project-share-editor", cx);
    let f = file(&root);
    r.check("unticking SQL format takes it out of the file", f.as_ref().is_some_and(|f| f.editor.is_none() && !f.share.editor), format!("{:?}", f.map(|f| f.share)));

    // A connection made while the project is open joins it.
    let mut made = dataman_core::ConnectionProfile::new_file(Engine::Sqlite, dataman_core::config::tilde(&root.join("made.sqlite").to_string_lossy()));
    made.name = "Made here".into();
    let _ = core.config.upsert(made.clone());
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.connections_on_disk(window, cx)));
    let joined = until(cx, 5, |_| file(&root).is_some_and(|f| f.connections.iter().any(|p| p.id == made.id && p.path.as_deref() == Some("made.sqlite")))).await;
    r.check("a connection made here joins the project, relative to it", joined, "");
    draw(cx);
    click_probe("project-conn-1", cx);
    let left = file(&root).is_some_and(|f| !f.connections.iter().any(|p| p.id == made.id) && f.connections.len() == 1);
    snap("connections", cx);
    r.check("unticking it in Project connections takes it out", left, format!("{:?}", file(&root).map(|f| f.connections.len())));

    // A teammate's change comes in (a pull): the app follows.
    let mut f = file(&root).unwrap();
    f.connections[0].name = "Team shop (renamed)".into();
    std::fs::write(project::file_in(&root), f.to_text()).unwrap();
    let renamed = until(cx, 5, |cx| names(ws, cx).contains(&"Team shop (renamed)".to_string())).await;
    r.check("an edit pulled into the file reaches the explorer", renamed, format!("{:?}", names(ws, cx)));
    let text = std::fs::read_to_string(project::file_in(&root)).unwrap_or_default();
    let keys = text.lines().filter(|l| !l.starts_with('#')).any(|l| l.contains("password"));
    r.check("no password in the file", !keys, &text);

    // A hostile change to the file (anyone who can push to the repository): held for review,
    // nothing of it applied, nothing written over it; Not now keeps it held, Apply anyway
    // applies that file and logs it in Audit.
    let before = core.config.get("team-shop");
    let mut f = file(&root).unwrap();
    f.connections[0].mcp = Some(dataman_core::McpAccess::Full);
    f.connections[0].host = "db.attacker.example".into();
    let hostile = f.to_text();
    std::fs::write(project::file_in(&root), &hostile).unwrap();
    let shown = until(cx, 5, |cx| probed("project-review-apply", cx).is_some()).await;
    r.check("a file that widens agents' access asks first", shown && probed("project-review-loosens", cx).is_some(), "");
    snap("review", cx);
    let same = core.config.get("team-shop") == before && std::fs::read_to_string(project::file_in(&root)).unwrap_or_default() == hostile;
    r.check("…and changes nothing until then, nor writes over the file", same, format!("{:?}", core.config.get("team-shop").map(|p| (p.mcp, p.host))));
    click_probe("project-review-keep", cx);
    let held = until(cx, 2, |cx| cx.update(|_, cx| ws.read(cx).project_review.is_none()).unwrap_or(false)).await;
    r.check("Not now: still held, Settings › Project offers the review", held && core.project.review().is_some_and(|r| r.declined) && probed("project-review", cx).is_some(), "");
    click_probe("project-review", cx);
    let again = until(cx, 2, |cx| probed("project-review-apply", cx).is_some()).await;
    key(cx, "escape");
    let esc = until(cx, 2, |cx| cx.update(|_, cx| ws.read(cx).project_review.is_none()).unwrap_or(false)).await;
    r.check("Review… shows it again; esc keeps it held", again && esc && core.config.get("team-shop") == before, "");
    click_probe("project-review", cx);
    until(cx, 2, |cx| probed("project-review-apply", cx).is_some()).await;
    key(cx, "cmd-enter");
    let applied = until(cx, 5, |_| core.config.get("team-shop").is_some_and(|p| p.mcp == Some(dataman_core::McpAccess::Full))).await;
    let audit = std::fs::read_to_string(core.dirs.mcp_audit_file()).unwrap_or_default();
    let p = core.config.get("team-shop");
    r.check(
        "⌘↩ applies that file, moves the connection without its saved password, logged in Audit",
        applied && audit.contains("project_review") && p.as_ref().is_some_and(|p| p.host == "db.attacker.example" && !p.save_password && p.from_project),
        format!("{:?}", p.map(|p| (p.mcp, p.host, p.save_password))),
    );

    // Projects off: no menu item, no sync.
    click_probe("projects-off", cx);
    let p = cx.update(|_, cx| crate::project_ui::projects(cx)).unwrap();
    r.check("Projects off", !p.enabled && p.open.is_none(), "");
    let _ = core.settings.update(|s| s.indent = 2);
    let mut g = core.config.get("team-shop").unwrap();
    g.name = "Local only".into();
    let _ = core.config.upsert(g);
    pause(cx, 1500).await;
    let f = file(&root);
    r.check("…and nothing reaches the file", f.as_ref().is_some_and(|f| f.connections[0].name == "Team shop (renamed)"), format!("{:?}", f.map(|f| f.connections[0].name.clone())));
    key(cx, "escape");
    let label = palette_label(">open project", cx, ws).await;
    r.check("…nor the palette", label != "Open project…", &label);

    // On again, then closed: what it brought stays.
    if let Some(v) = cx.update(|_, cx| ws.read(cx).settings_view.clone()).ok().flatten() {
        let _ = cx.update(|_, cx| v.update(cx, |v, cx| v.show_section("project", cx)));
    } else {
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_settings_at("project", window, cx)));
    }
    click_probe("projects-on", cx);
    let back = until(cx, 5, |_| file(&root).is_some_and(|f| f.connections[0].name == "Local only")).await;
    r.check("on again: the local change goes to the project", back, "");
    click_probe("project-close", cx);
    let p = cx.update(|_, cx| crate::project_ui::projects(cx)).unwrap();
    r.check("Close: no project, its connection stays", p.enabled && p.open.is_none() && core.config.get("team-shop").is_some(), "");
    key(cx, "escape");
    let _ = std::fs::remove_dir_all(&dir);
}
