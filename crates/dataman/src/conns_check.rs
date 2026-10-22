//! `connections-check`: every engine through the connection sheet (picker search, form, Test,
//! Save & connect with environment, MCP tier and color, edit, duplicate, delete and undo), URL
//! entry, SSH through the compose bastion, a connection from the clipboard, and importing
//! pgpass/service entries.

use gpui::{AsyncWindowContext, Entity, Focusable, Modifiers, MouseButton};
use dataman_core::config::{Engine, Environment, McpAccess};

use crate::app::Workspace;
use crate::check::*;
use crate::conn_dialog::{ConnDialog, Step};
use crate::tree_menu::Act;

fn dialog(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Entity<ConnDialog>> {
    cx.update(|_, cx| ws.read(cx).dialog.clone()).ok().flatten()
}

fn status(d: &Entity<ConnDialog>, cx: &mut AsyncWindowContext) -> String {
    cx.update(|_, cx| d.read(cx).demo_status(cx).0).unwrap_or_default()
}

/// Clicks Test and waits for its verdict.
async fn test(d: &Entity<ConnDialog>, cx: &mut AsyncWindowContext) -> String {
    click_probe("dlg-test", cx);
    until(cx, 20, |cx| status(d, cx) != "testing" && status(d, cx) != "untested").await;
    status(d, cx)
}

/// Clicks a text field of the sheet and types over what it holds.
fn fill(probe: &str, text: &str, cx: &mut AsyncWindowContext) -> bool {
    if !click_probe(&format!("field-{probe}"), cx) {
        return false;
    }
    key(cx, "cmd-a");
    if text.is_empty() {
        key(cx, "backspace");
    } else {
        typ(cx, text);
    }
    true
}

fn saved(ws: &Entity<Workspace>, id: &str, cx: &mut AsyncWindowContext) -> Option<dataman_core::ConnectionProfile> {
    core(ws, cx).config.connections().into_iter().find(|p| p.id == id)
}

fn focus_tree(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        let tree = ws.read(cx).tree.clone();
        window.focus(&tree.focus_handle(cx), cx);
    });
    draw(cx);
}

/// Opens a row's context menu (a right click on it, or ⇧F10 on the selected row when it is
/// scrolled away) and clicks `act` in it.
async fn menu(ws: &Entity<Workspace>, row: &str, act: Act, cx: &mut AsyncWindowContext) -> bool {
    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    draw(cx);
    match cx.update(|_, cx| tree.read(cx).demo_row_point(row)).ok().flatten() {
        Some(p) => click_with(cx, p, MouseButton::Right, Modifiers::default(), 1),
        None => {
            let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.select_id(row, cx)));
            focus_tree(ws, cx);
            key(cx, "shift-f10");
        }
    }
    pause(cx, 250).await;
    draw(cx);
    match cx.update(|_, cx| tree.read(cx).demo_menu_item_point(act)).ok().flatten() {
        Some(p) => {
            click(cx, p);
            pause(cx, 150).await;
            true
        }
        None => {
            eprintln!("check: no {act:?} in the menu of {row}");
            key(cx, "escape");
            false
        }
    }
}

struct Case {
    engine: Engine,
    search: &'static str,
}

const CASES: [Case; 7] = [
    Case { engine: Engine::Postgres, search: "pg" },
    Case { engine: Engine::MySql, search: "maria" },
    Case { engine: Engine::SqlServer, search: "mssql" },
    Case { engine: Engine::MongoDb, search: "mongo" },
    Case { engine: Engine::Elasticsearch, search: "elastic" },
    Case { engine: Engine::OpenSearch, search: "opensearch" },
    Case { engine: Engine::Redis, search: "valkey" },
];

/// File databases: no server, so the form is a file (Browse…, New…) instead of host and user.
const FILE_CASES: [(Engine, &str); 1] = [(Engine::Sqlite, "sqlite")];

async fn file_engine(ws: &Entity<Workspace>, e: Engine, search: &str, r: &mut Report, cx: &mut AsyncWindowContext) {
    let label = e.label();
    key(cx, "cmd-n");
    pause(cx, 150).await;
    let Some(d) = dialog(ws, cx) else {
        r.check(&format!("{label}: ⌘N opens the connection sheet"), false, "no sheet");
        return;
    };
    typ(cx, search);
    key(cx, "enter");
    pause(cx, 100).await;
    let (step, picked) = cx.update(|_, cx| (d.read(cx).step(), d.read(cx).current(cx).0.engine)).unwrap();
    let shown = |p: &str, cx: &mut AsyncWindowContext| probed(&format!("field-{p}"), cx).is_some();
    let shape = shown("FILE", cx) && !shown("HOST", cx) && !shown("USER", cx) && !shown("PASSWORD", cx) && probed("file-new", cx).is_some();
    r.check(&format!("{label}: typing \"{search}\" picks it, and its form is a file"), step == Step::Form && picked == e && shape, format!("{step:?} {picked:?} {shape}"));
    let dir = std::env::temp_dir().join(format!("dataman-conns-{}-{}", e.slug(), std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let missing = dir.join(format!("missing.{}", e.file_extensions()[0]));
    fill("FILE", &missing.to_string_lossy(), cx);
    let s = test(&d, cx).await;
    r.check(&format!("{label}: a missing file is refused, not created"), s.contains("does not exist") && !missing.exists(), &s);
    let new = dir.join(format!("fresh.{}", e.file_extensions()[0]));
    crate::sql_picker::set_test_save_path(new.clone());
    click_probe("file-new", cx);
    pause(cx, 150).await;
    let name = cx.update(|_, cx| d.read(cx).current(cx).0.name).unwrap_or_default();
    let s = test(&d, cx).await;
    r.check(&format!("{label}: New… names it after the file and Test creates it"), s == "ok" && new.is_file() && name == "fresh", format!("{s} {name}"));
    let id = cx.update(|_, cx| d.read(cx).profile_id().to_string()).unwrap();
    click_probe("dlg-save", cx);
    let up = connected(ws, &id, 20, cx).await;
    let p = saved(ws, &id, cx);
    r.check(
        &format!("{label}: Save & connect keeps the file, no host or user"),
        up.is_ok() && p.as_ref().is_some_and(|p| p.engine == e && p.path.is_some() && p.host.is_empty() && p.user.is_empty()),
        format!("{up:?} {:?}", p.map(|p| (p.path, p.host))),
    );
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dialog(Some(id.clone()), window, cx)));
    pause(cx, 100).await;
    click_probe("dlg-delete", cx);
    click_probe("dlg-delete", cx);
    pause(cx, 100).await;
    let gone = saved(ws, &id, cx).is_none();
    r.check(&format!("{label}: Delete forgets the connection, the file stays"), gone && new.is_file(), format!("gone {gone}"));
    let _ = std::fs::remove_dir_all(&dir);
}

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    for case in &CASES {
        engine(ws, case, r, cx).await;
    }
    for (e, search) in FILE_CASES {
        file_engine(ws, e, search, r, cx).await;
    }
    urls(ws, r, cx).await;
    ssh(ws, r, cx).await;
    clipboard_and_import(ws, r, cx).await;
}

async fn engine(ws: &Entity<Workspace>, case: &Case, r: &mut Report, cx: &mut AsyncWindowContext) {
    let e = case.engine;
    let label = e.label();
    let (prof, pw) = profile(e);
    key(cx, "cmd-n");
    pause(cx, 150).await;
    let Some(d) = dialog(ws, cx) else {
        r.check(&format!("{label}: ⌘N opens the connection sheet"), false, "no sheet");
        return;
    };
    let step0 = cx.update(|_, cx| d.read(cx).step()).ok();
    typ(cx, case.search);
    key(cx, "enter");
    pause(cx, 100).await;
    let (step, picked) = cx.update(|_, cx| (d.read(cx).step(), d.read(cx).current(cx).0.engine)).unwrap();
    r.check(
        &format!("{label}: ⌘N, typing \"{}\" and ↩ picks it", case.search),
        step0 == Some(Step::Engine) && step == Step::Form && picked == e,
        format!("{step0:?} {step:?} {picked:?}"),
    );

    let db_field = match e {
        Engine::Elasticsearch | Engine::OpenSearch => None,
        Engine::Redis => Some("DB"),
        _ => Some("DATABASE"),
    };
    let shown = |p: &str, cx: &mut AsyncWindowContext| probed(&format!("field-{p}"), cx).is_some();
    let user_field = if e.is_sql() { "USER" } else { "USER (OPTIONAL)" };
    let port = cx.update(|_, cx| d.read(cx).current(cx).0.port).unwrap_or(0);
    let shape = shown(user_field, cx)
        && db_field.is_none_or(|f| shown(f, cx))
        && !(db_field.is_none() && (shown("DATABASE", cx) || shown("DB", cx)))
        && shown("OPTIONS", cx) == (e == Engine::MongoDb)
        && port == e.default_port();
    r.check(&format!("{label}: its own form (user, database, options, port {})", e.default_port()), shape, format!("port {port}"));

    let name = format!("e2e {label}");
    typ(cx, &name);
    fill("HOST", &prof.host, cx);
    fill("PORT", &prof.port.to_string(), cx);
    fill(user_field, &prof.user, cx);
    if let Some(f) = db_field {
        fill(f, &prof.database, cx);
    }
    if !pw.is_empty() {
        fill("PASSWORD", "not-the-password", cx);
        let s = test(&d, cx).await;
        r.check(&format!("{label}: a wrong password fails the test"), s.starts_with("failed"), &s);
        fill("PASSWORD", &pw, cx);
    }
    let s = test(&d, cx).await;
    r.check(&format!("{label}: Test reaches the server"), s == "ok", &s);

    click_probe("env-staging", cx);
    click_probe("mcp-read", cx);
    click_probe("swatch-3", cx);
    let id = cx.update(|_, cx| d.read(cx).profile_id().to_string()).unwrap();
    click_probe("dlg-save", cx);
    let up = connected(ws, &id, 30, cx).await;
    let gone = dialog(ws, cx).is_none();
    let p = saved(ws, &id, cx);
    let in_tree = until(cx, 5, |cx| cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_conn_row(&id)).ok().flatten().is_some()).await;
    r.check(
        &format!("{label}: Save & connect keeps its environment, MCP tier and color, and lists it"),
        gone && up.is_ok()
            && in_tree
            && p.as_ref().is_some_and(|p| p.name == name && p.engine == e && p.env == Environment::Staging && p.mcp == Some(McpAccess::Read) && p.color.as_deref() == Some(crate::theme::SWATCHES[3])),
        format!("{up:?} sheet gone {gone} in tree {in_tree} {:?}", p.map(|p| (p.name, p.env, p.mcp, p.color))),
    );

    let row = format!("c:{id}");
    let edited = menu(ws, &row, Act::EditConnection, cx).await;
    let d2 = dialog(ws, cx);
    let prefilled = d2.as_ref().map(|d| cx.update(|_, cx| (d.read(cx).is_new(), d.read(cx).current(cx).0.host)).unwrap());
    if d2.is_some() {
        fill("NAME", &format!("{name} renamed"), cx);
        key(cx, "cmd-enter");
    }
    let up = connected(ws, &id, 30, cx).await;
    let p = saved(ws, &id, cx);
    r.check(
        &format!("{label}: Edit… from the explorer's menu: prefilled, renamed, reconnects"),
        edited && prefilled == Some((false, prof.host.clone())) && up.is_ok() && p.as_ref().is_some_and(|p| p.name == format!("{name} renamed")),
        format!("{prefilled:?} {up:?} {:?}", p.map(|p| p.name)),
    );

    let duplicated = menu(ws, &row, Act::DuplicateConnection, cx).await;
    let copy = dialog(ws, cx).map(|d| cx.update(|_, cx| (d.read(cx).profile_id().to_string(), d.read(cx).current(cx))).unwrap());
    if copy.is_some() {
        key(cx, "cmd-enter");
    }
    let copy_id = copy.as_ref().map(|c| c.0.clone()).unwrap_or_default();
    let up = connected(ws, &copy_id, 30, cx).await;
    r.check(
        &format!("{label}: Duplicate: a copy with the password, connects"),
        duplicated && copy.as_ref().is_some_and(|(cid, (p, cpw))| *cid != id && p.name == format!("copy of {name} renamed") && *cpw == pw) && up.is_ok(),
        format!("{:?} {up:?}", copy.map(|(i, (p, _))| (i, p.name))),
    );

    let asked = menu(ws, &format!("c:{copy_id}"), Act::DeleteConnection, cx).await && cx.update(|_, cx| ws.read(cx).confirm_delete.is_some()).unwrap_or(false);
    key(cx, "cmd-backspace");
    pause(cx, 100).await;
    let left = saved(ws, &copy_id, cx).is_some();
    r.check(&format!("{label}: Delete from the menu asks first; ⌘⌫ deletes"), asked && !left, format!("asked {asked} still there {left}"));

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dialog(Some(id.clone()), window, cx)));
    pause(cx, 100).await;
    click_probe("dlg-delete", cx);
    let first = saved(ws, &id, cx).is_some();
    click_probe("dlg-delete", cx);
    pause(cx, 100).await;
    let second = saved(ws, &id, cx).is_some();
    focus_tree(ws, cx);
    key(cx, "cmd-z");
    pause(cx, 150).await;
    let back = saved(ws, &id, cx).is_some();
    r.check(&format!("{label}: Delete in the sheet takes a second click; ⌘Z brings it back"), first && !second && back, format!("after one click {first}, after two {second}, after ⌘Z {back}"));
}

async fn urls(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    for e in [Engine::Postgres, Engine::MySql, Engine::SqlServer, Engine::MongoDb, Engine::Elasticsearch, Engine::OpenSearch, Engine::Redis] {
        let (p, pw) = profile(e);
        let url = match e {
            Engine::Postgres => format!("postgresql://{}:{pw}@{}:{}/{}", p.user, p.host, p.port, p.database),
            Engine::MySql => format!("mysql://{}:{pw}@{}:{}/{}", p.user, p.host, p.port, p.database),
            Engine::SqlServer => format!("Server=tcp:{},{};Database={};User Id={};Password={pw};TrustServerCertificate=True", p.host, p.port, p.database, p.user),
            Engine::MongoDb => format!("mongodb://{}:{pw}@{}:{}/{}", p.user, p.host, p.port, p.database),
            Engine::Elasticsearch => format!("http://{}:{pw}@{}:{}", p.user, p.host, p.port),
            Engine::OpenSearch => format!("http://{}:{}", p.host, p.port),
            Engine::Redis => format!("redis://:{pw}@{}:{}/{}", p.host, p.port, p.database),
            Engine::Sqlite | Engine::DuckDb => p.url(),
        };
        key(cx, "cmd-n");
        pause(cx, 150).await;
        let Some(d) = dialog(ws, cx) else { continue };
        click_probe(&format!("engine-{}", e.label()), cx);
        click_probe("entry-URL", cx);
        key(cx, "cmd-a");
        typ(cx, &url);
        pause(cx, 50).await;
        let (got, gpw) = cx.update(|_, cx| d.read(cx).current(cx)).unwrap();
        let typed = cx.update(|_, cx| d.read(cx).demo_conn_str(cx)).unwrap_or_default();
        let s = test(&d, cx).await;
        r.check(
            &format!("{}: a URL typed in URL entry fills the connection and tests", e.label()),
            got.engine == e && got.host == p.host && got.port == p.port && gpw == pw && s == "ok",
            format!("{:?} {}:{} pw {} → {s} (URL field {typed:?})", got.engine, got.host, got.port, gpw.len()),
        );
        click_probe("dlg-cancel", cx);
        pause(cx, 100).await;
    }

    key(cx, "cmd-n");
    pause(cx, 150).await;
    if let Some(d) = dialog(ws, cx) {
        let (p, pw) = profile(Engine::Elasticsearch);
        click_probe("engine-Elasticsearch", cx);
        fill("HOST", &p.host, cx);
        fill("PORT", &p.port.to_string(), cx);
        fill("USER (OPTIONAL)", &p.user, cx);
        fill("PASSWORD", &pw, cx);
        click_probe("entry-URL", cx);
        let bad = cx.update(|_, cx| d.read(cx).conn_str_invalid(cx)).unwrap_or(true);
        let s = test(&d, cx).await;
        r.check("a filled Elasticsearch form switched to URL keeps working (its own http URL)", !bad && s == "ok", format!("flagged {bad} → {s}"));
        click_probe("dlg-cancel", cx);
    }

    key(cx, "cmd-n");
    pause(cx, 150).await;
    if let Some(d) = dialog(ws, cx) {
        click_probe("engine-PostgreSQL", cx);
        click_probe("entry-URL", cx);
        key(cx, "cmd-a");
        typ(cx, "postgres//app@db.example.com/shop");
        click_probe("dlg-save", cx);
        pause(cx, 100).await;
        let (bad, s) = cx.update(|_, cx| (d.read(cx).conn_str_invalid(cx), d.read(cx).demo_status(cx).0)).unwrap();
        r.check("a malformed URL is flagged and not saved", bad && s.starts_with("invalid") && dialog(ws, cx).is_some(), format!("flagged {bad} {s}"));
        click_probe("dlg-cancel", cx);
    }
}

async fn ssh(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (pg, pw) = profile(Engine::Postgres);
    let s = spec("DATAMAN_BENCH_SSH", "127.0.0.1:2299:dataman");
    let key_file = std::env::var("DATAMAN_BENCH_SSH_KEY").unwrap_or_default();
    if key_file.is_empty() || !std::path::Path::new(&key_file).exists() {
        r.check("SSH: the bastion's key (DATAMAN_BENCH_SSH_KEY, made by xtask e2e)", false, &key_file);
        return;
    }
    key(cx, "cmd-n");
    pause(cx, 150).await;
    let Some(d) = dialog(ws, cx) else { return };
    click_probe("engine-PostgreSQL", cx);
    typ(cx, "e2e via ssh");
    // The database as the bastion sees it: the host's published port.
    fill("HOST", "host.docker.internal", cx);
    fill("PORT", &pg.port.to_string(), cx);
    fill("USER", &pg.user, cx);
    fill("DATABASE", &pg.database, cx);
    fill("PASSWORD", &pw, cx);
    click_probe("ssh-toggle", cx);
    fill("SSH HOST", &s[0], cx);
    fill("SSH PORT", &s[1], cx);
    fill("SSH USER", "nobody", cx);
    click_probe("ssh-auth-key", cx);
    fill("KEY FILE", &key_file, cx);
    let first = test(&d, cx).await;
    r.check("SSH: an unknown bastion asks to trust its host key", first == "hostkey", &first);
    click_probe("dlg-trust", cx);
    until(cx, 5, |cx| status(&d, cx) != "hostkey").await;
    let wrong = test(&d, cx).await;
    r.check("SSH: a user the bastion refuses fails the test", wrong.starts_with("failed"), &wrong);
    fill("SSH USER", s.get(2).map_or("dataman", String::as_str), cx);
    let ok = test(&d, cx).await;
    r.check("SSH: trusted, with the key, the test goes through the tunnel", ok == "ok", &ok);
    let id = cx.update(|_, cx| d.read(cx).profile_id().to_string()).unwrap();
    click_probe("dlg-save", cx);
    let up = connected(ws, &id, 30, cx).await;
    let _ = cx.update(|window, cx| {
        ws.update(cx, |w, cx| {
            w.current_conn = Some(id.clone());
            w.new_tab(window, cx);
        })
    });
    let tab = run_keys(ws, "SELECT current_database(), inet_server_port()", "cmd-enter", cx).await;
    let got = tab.as_ref().map(|t| (tab_conn(t, cx), cell(t, 0, 0, cx), cell(t, 0, 1, cx)));
    r.check(
        "SSH: saved, connected, a query runs on the server behind the bastion",
        up.is_ok() && got == Some((Some(id.clone()), Some(pg.database.clone()), Some("5432".into()))),
        format!("{up:?} {got:?}"),
    );
}

async fn clipboard_and_import(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (p, pw) = profile(Engine::MySql);
    set_clipboard(&format!("mysql://{}:{pw}@{}:{}/{}", p.user, p.host, p.port, p.database), cx);
    let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::app::NewConnectionFromClipboard), cx));
    pause(cx, 150).await;
    let got = dialog(ws, cx).map(|d| cx.update(|_, cx| d.read(cx).current(cx)).unwrap());
    r.check(
        "New Connection from Clipboard: a URL on the clipboard prefills the sheet",
        got.as_ref().is_some_and(|(g, gpw)| g.engine == Engine::MySql && g.port == p.port && g.database == p.database && *gpw == pw),
        format!("{:?}", got.map(|(g, _)| (g.engine, g.port, g.database))),
    );
    click_probe("dlg-cancel", cx);
    pause(cx, 100).await;
    set_clipboard("just some words", cx);
    let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::app::NewConnectionFromClipboard), cx));
    pause(cx, 100).await;
    let notice = cx.update(|_, cx| ws.read(cx).flash_notice.as_ref().map(|n| n.0.clone())).ok().flatten();
    r.check("…and words that are no connection string only say so", dialog(ws, cx).is_none() && notice.as_deref().is_some_and(|n| n.contains("No connection string")), format!("{notice:?}"));
    set_clipboard("", cx);

    let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::app::ImportConnections), cx));
    let view = cx.update(|_, cx| ws.read(cx).import.clone()).ok().flatten();
    let Some(view) = view else {
        r.check("Import Connections opens its sheet", false, "no sheet");
        return;
    };
    until(cx, 5, |cx| cx.update(|_, cx| view.read(cx).demo_items().is_some()).unwrap_or(false)).await;
    let items = cx.update(|_, cx| view.read(cx).demo_items()).ok().flatten().unwrap_or_default();
    let svc = items.iter().position(|i| i.0 == "e2e-service" && i.1 == dataman_core::ImportSource::Service);
    let pass = items.iter().position(|i| i.1 == dataman_core::ImportSource::Pgpass && i.0.contains("127.0.0.1"));
    r.check("Import Connections lists the service file's and the pgpass file's entries", svc.is_some() && pass.is_some(), format!("{items:?}"));
    let before = core(ws, cx).config.connections().len();
    if let Some(ix) = svc {
        let hi = items.iter().position(|i| !i.2).unwrap_or(0);
        for _ in hi..ix {
            key(cx, "down");
        }
        key(cx, "space");
        key(cx, "cmd-enter");
    }
    pause(cx, 200).await;
    let made = core(ws, cx).config.connections().into_iter().find(|p| p.name == "e2e-service");
    let up = match &made {
        Some(p) => {
            let id = p.id.clone();
            let _ = cx.update(|_, cx| ws.read(cx).store.clone().update(cx, |s, cx| s.connect(&id, None, cx)));
            connected(ws, &id, 20, cx).await
        }
        None => Err("not imported".into()),
    };
    let after = core(ws, cx).config.connections().len();
    let names: Vec<String> = core(ws, cx).config.connections().into_iter().map(|p| p.name).collect();
    r.check("…the picked service is imported and connects with the pgpass password", after == before + 1 && up.is_ok(), format!("{before} → {after} {up:?} {items:?} {names:?}"));
}
