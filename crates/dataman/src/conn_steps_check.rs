//! `conn-steps-check`: the stepped New connection and Edit connection, with no server and no real
//! cloud CLI: fake `az` and `aws` come first on the PATH and record every run, and the home folder
//! is a temporary one.

use std::path::PathBuf;

use gpui::{AsyncWindowContext, Bounds, Entity, Pixels};
use dataman_core::{Auth, AuthKind, Engine, Environment, McpAccess};

use crate::app::Workspace;
use crate::check::*;
use crate::conn_dialog::{ConnDialog, Step};
use crate::conns_check::{fill, saved};

const STAGE: &str = "conn-steps-check";
const AZURE: &str = "orders.postgres.database.azure.com";
const RDS: &str = "orders.abc123.us-east-1.rds.amazonaws.com";
const STRING: &str = "postgres://ana:hunter22@db.internal:6432/orders?sslmode=require";

fn root() -> PathBuf {
    std::env::temp_dir().join(format!("dataman-conn-steps-{}", std::process::id()))
}

fn runs() -> String {
    std::fs::read_to_string(root().join("runs")).unwrap_or_default()
}

/// Before the core starts, which reads the PATH once.
pub fn prepare() {
    if std::env::var("DATAMAN_SHOT_STAGE").as_deref() != Ok(STAGE) || cfg!(windows) {
        return;
    }
    let root = root();
    let (bin, home) = (root.join("bin"), root.join("home"));
    std::fs::create_dir_all(&bin).expect("a folder for the fake CLIs");
    std::fs::create_dir_all(&home).expect("a temporary home");
    let token = "eyJhbGciOiJSUzI1NiJ9.eyJ1cG4iOiJhbmFAY29udG9zby5jb20ifQ.c2ln";
    let r = root.display();
    let az = format!(
        "echo \"az $*\" >> '{r}/runs'\ncase \"$1\" in\naccount)\n  [ -f '{r}/signed-in' ] || {{ echo \"ERROR: Please run 'az login' to setup account.\" >&2; exit 1; }}\n  echo '{{\"accessToken\":\"{token}\",\"expires_on\":4102444800}}' ;;\nlogin)\n  if [ -f '{r}/hold' ]; then sleep 60 & echo $! > '{r}/login.pid'; wait; fi\n  while [ -f '{r}/gate' ]; do sleep 0.1; done\n  touch '{r}/signed-in' ;;\nesac\n"
    );
    let aws = format!("echo \"aws $*\" >> '{r}/runs'\ncase \"$1\" in\nrds) echo 'aws-iam-token' ;;\nsts) echo '{{\"Arn\":\"arn:aws:iam::1:user/aws-identity\"}}' ;;\nesac\n");
    let scripts = [("az", az), ("aws", aws)];
    #[cfg(unix)]
    for (name, script) in scripts {
        use std::os::unix::fs::PermissionsExt as _;
        let f = bin.join(name);
        std::fs::write(&f, format!("#!/bin/sh\n{script}")).expect("a fake CLI");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).expect("a fake CLI that runs");
    }
    #[cfg(not(unix))]
    let _ = scripts;
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::var_os("PATH").iter().flat_map(std::env::split_paths))).expect("the PATH");
    // SAFETY: first thing in main, before any other thread exists.
    unsafe {
        std::env::set_var("PATH", path);
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    }
}

fn dialog(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Entity<ConnDialog>> {
    cx.update(|_, cx| ws.read(cx).dialog.clone()).ok().flatten()
}

fn step(d: &Entity<ConnDialog>, cx: &mut AsyncWindowContext) -> Option<Step> {
    cx.update(|_, cx| d.read(cx).step()).ok()
}

fn method(d: &Entity<ConnDialog>, cx: &mut AsyncWindowContext) -> (AuthKind, &'static str, bool, String) {
    cx.update(|_, cx| d.read(cx).demo_method(cx)).unwrap()
}

fn string(d: &Entity<ConnDialog>, cx: &mut AsyncWindowContext) -> String {
    cx.update(|_, cx| d.read(cx).demo_conn_str(cx)).unwrap_or_default()
}

/// ⌘N and a database picked by typing: the sheet on Connection.
async fn sheet(ws: &Entity<Workspace>, search: &str, cx: &mut AsyncWindowContext) -> Option<Entity<ConnDialog>> {
    key(cx, "cmd-n");
    pause(cx, 100).await;
    let d = dialog(ws, cx)?;
    typ(cx, search);
    key(cx, "enter");
    pause(cx, 50).await;
    Some(d)
}

async fn shut(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) {
    key(cx, "escape");
    until(cx, 3, |cx| dialog(ws, cx).is_none()).await;
}

async fn cont(cx: &mut AsyncWindowContext) {
    click_probe("dlg-save", cx);
    pause(cx, 50).await;
}

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    if cfg!(windows) {
        r.check("the fake CLIs are shell scripts: this stage runs on Unix", true, "");
        return;
    }
    database_step(ws, r, cx).await;
    strings(ws, r, cx).await;
    hosts_pick_methods(ws, r, cx).await;
    sign_in(ws, r, cx).await;
    colors(ws, r, cx).await;
    breadcrumb(ws, r, cx).await;
    scrolling(ws, r, cx).await;
    files(ws, r, cx).await;
    saving(ws, r, cx).await;
    a_password_stays_with_its_string(ws, r, cx).await;
    a_changed_host_goes_through_sign_in(ws, r, cx).await;
    a_login_answers_for_its_method(ws, r, cx).await;
    a_hidden_region_says_nothing(ws, r, cx).await;
    engine_changes_land_where_they_can_be_fixed(ws, r, cx).await;
    mongo_options_follow_the_string(ws, r, cx).await;
    saved_means_saved(ws, r, cx).await;
    let _ = std::fs::remove_dir_all(root());
}

async fn database_step(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    key(cx, "cmd-n");
    pause(cx, 100).await;
    let Some(d) = dialog(ws, cx) else {
        r.check("⌘N opens the sheet", false, "");
        return;
    };
    r.check("the sheet opens on Database, with no footer", step(&d, cx) == Some(Step::Database) && probed("dlg-save", cx).is_none(), format!("{:?}", step(&d, cx)));
    let card: Option<Bounds<Pixels>> = probed("dlg-card", cx);
    typ(cx, "oracle");
    draw(cx);
    r.check("a name not found leaves no tile", probed("engine-PostgreSQL", cx).is_none() && step(&d, cx) == Some(Step::Database), "");
    key(cx, "cmd-a");
    typ(cx, "oracle://db.internal/orders");
    key(cx, "enter");
    pause(cx, 50).await;
    r.check("a string that can't be read stays, with no tile", probed("engine-PostgreSQL", cx).is_none() && step(&d, cx) == Some(Step::Database), "");
    key(cx, "cmd-a");
    typ(cx, "pg");
    key(cx, "enter");
    pause(cx, 50).await;
    r.check("Enter takes the first result on to Connection", step(&d, cx) == Some(Step::Connection), format!("{:?}", step(&d, cx)));
    let blank = cx.update(|_, cx| [1, 3, 5].map(|i| d.read(cx).demo_field(i, cx))).unwrap();
    r.check("a blank sheet starts with no host, user or database", blank.iter().all(String::is_empty), format!("{blank:?}"));
    cont(cx).await;
    r.check("Continue waits for a host", step(&d, cx) == Some(Step::Connection), format!("{:?}", step(&d, cx)));
    let mut same = probed("dlg-card", cx) == card;
    fill("HOST", "db.internal", cx);
    cont(cx).await;
    r.check("with a host it goes to Sign-in", step(&d, cx) == Some(Step::SignIn), format!("{:?}", step(&d, cx)));
    same &= probed("dlg-card", cx) == card;
    r.check("Test shows on Sign-in", probed("dlg-test", cx).is_some(), "");
    cont(cx).await;
    let (status, _) = cx.update(|_, cx| d.read(cx).demo_status(cx)).unwrap();
    r.check("Sign-in asks for the user before going on", step(&d, cx) == Some(Step::SignIn) && status.contains("user"), &status);
    fill("USER", "ana", cx);
    cont(cx).await;
    r.check("then MCP, with Skip and no Test", step(&d, cx) == Some(Step::Mcp) && probed("dlg-skip", cx).is_some() && probed("dlg-test", cx).is_none(), format!("{:?}", step(&d, cx)));
    same &= probed("dlg-card", cx) == card;
    r.check("the modal keeps its one size through the steps", same && card.is_some(), format!("{card:?}"));
    shut(ws, cx).await;
}

async fn strings(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let Some(d) = sheet(ws, "mysql", cx).await else { return };
    fill("CONNECTION STRING", STRING, cx);
    let (p, pw) = cx.update(|_, cx| d.read(cx).current(cx)).unwrap();
    let got = (p.engine, p.host.as_str(), p.port, p.user.as_str(), p.database.as_str(), pw.as_str(), p.sslmode);
    r.check(
        "a connection string fills every field, user and password too, and changes the database",
        got == (Engine::Postgres, "db.internal", 6432, "ana", "orders", "hunter22", dataman_core::SslMode::Require),
        format!("{got:?}"),
    );
    r.check("the password stays visible only in the string", string(&d, cx).contains("hunter22"), string(&d, cx));
    cont(cx).await;
    let m = method(&d, cx);
    r.check("from a string: Keep user & password, with the note", m.0 == AuthKind::Password && m.1 == "Keep user & password" && m.2 && probed("from-string", cx).is_some(), format!("{m:?}"));
    click_probe("crumb-Connection", cx);
    pause(cx, 50).await;
    fill("HOST", "other.internal", cx);
    let text = string(&d, cx);
    r.check("editing a field rewrites the string, without the password", text.contains("other.internal") && text.contains("ana@") && !text.contains("hunter22"), &text);
    let (p, pw) = cx.update(|_, cx| d.read(cx).current(cx)).unwrap();
    r.check("the password stays in its field, never in what is saved as text", pw == "hunter22" && !p.url().contains("hunter22"), p.url());
    cont(cx).await;
    r.check("the note stays: user and password are still the string's", method(&d, cx).2, format!("{:?}", method(&d, cx)));
    fill("USER", "bob", cx);
    let m = method(&d, cx);
    r.check("typed over, the note goes and the name stays", !m.2 && m.1 == "Keep user & password" && probed("from-string", cx).is_none(), format!("{m:?}"));
    fill("PASSWORD", "", cx);
    r.check("no password, and it is User & password", method(&d, cx).1 == "User & password", format!("{:?}", method(&d, cx)));
    click_probe("crumb-Connection", cx);
    pause(cx, 50).await;
    fill("CONNECTION STRING", "orders.internal://", cx);
    draw(cx);
    let bad = cx.update(|_, cx| d.read(cx).conn_str_invalid(cx)).unwrap_or(false);
    cont(cx).await;
    r.check("what can't be read says so under the field, and Continue stays", bad && probed("string-bad", cx).is_some() && step(&d, cx) == Some(Step::Connection), bad);
    shut(ws, cx).await;

    // Pasted in the search (typed all at once, as a paste comes): straight to Connection.
    key(cx, "cmd-n");
    pause(cx, 100).await;
    let Some(d) = dialog(ws, cx) else { return };
    typ(cx, STRING);
    key(cx, "enter");
    pause(cx, 50).await;
    let (p, pw) = cx.update(|_, cx| d.read(cx).current(cx)).unwrap();
    r.check("a string in the search goes to Connection, filled", step(&d, cx) == Some(Step::Connection) && p.host == "db.internal" && pw == "hunter22", format!("{:?} {}", step(&d, cx), p.host));
    shut(ws, cx).await;
}

async fn hosts_pick_methods(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    for (host, want) in [(AZURE, AuthKind::EntraId), (RDS, AuthKind::AwsIam), ("orders.europe-west1.sql.goog", AuthKind::GoogleIam), ("db.internal", AuthKind::Password)] {
        let Some(d) = sheet(ws, "pg", cx).await else { return };
        fill("HOST", host, cx);
        cont(cx).await;
        let m = method(&d, cx);
        r.check(&format!("{host} preselects {want:?}"), m.0 == want, format!("{m:?}"));
        if want != AuthKind::Password {
            let tls = cx.update(|_, cx| d.read(cx).current(cx).0.sslmode).unwrap();
            r.check(&format!("{want:?} raises SSL to Require or Verify"), matches!(tls, dataman_core::SslMode::Require | dataman_core::SslMode::VerifyFull), format!("{tls:?}"));
        }
        shut(ws, cx).await;
    }
    let Some(d) = sheet(ws, "pg", cx).await else { return };
    fill("CONNECTION STRING", &format!("postgres://ana:hunter22@{AZURE}/orders"), cx);
    cont(cx).await;
    r.check("a password in the string picks it, even on an Azure host", method(&d, cx).0 == AuthKind::Password, format!("{:?}", method(&d, cx)));
    shut(ws, cx).await;

    let Some(d) = sheet(ws, "pg", cx).await else { return };
    fill("HOST", AZURE, cx);
    cont(cx).await;
    click_probe("auth-password", cx);
    pause(cx, 50).await;
    click_probe("crumb-Connection", cx);
    pause(cx, 50).await;
    cont(cx).await;
    r.check("once picked, going back does not undo the choice", method(&d, cx).0 == AuthKind::Password, format!("{:?}", method(&d, cx)));
    shut(ws, cx).await;

    let Some(d) = sheet(ws, "redis", cx).await else { return };
    fill("HOST", "db.internal", cx);
    cont(cx).await;
    r.check("an engine with one method shows its fields, no list", probed("auth-password", cx).is_none() && probed("field-USER (OPTIONAL)", cx).is_some() && probed("field-PASSWORD", cx).is_some(), format!("{:?}", step(&d, cx)));
    shut(ws, cx).await;
    let Some(_) = sheet(ws, "mssql", cx).await else { return };
    fill("HOST", "orders.database.windows.net", cx);
    cont(cx).await;
    r.check("SQL Server with Entra asks for no user", probed("auth-entra-id", cx).is_some() && probed("field-USER", cx).is_none() && probed("field-TENANT", cx).is_some(), "");
    shut(ws, cx).await;
}

/// Running, not a zombie its parent has yet to reap (Linux's `/proc` tells them apart).
fn alive(pid: &str) -> bool {
    let pid = pid.trim();
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat.rsplit(')').next().is_some_and(|rest| rest.trim_start().starts_with('Z')),
        Err(_) if std::path::Path::new("/proc/self").exists() => false,
        Err(_) => std::process::Command::new("kill").args(["-0", pid]).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success()),
    }
}

async fn sign_in(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let before = runs();
    let Some(d) = sheet(ws, "pg", cx).await else { return };
    fill("HOST", AZURE, cx);
    cont(cx).await;
    for k in ["aws-iam", "google-iam", "password", "entra-id"] {
        click_probe(&format!("auth-{k}"), cx);
        pause(cx, 30).await;
    }
    fill("USER", "ana@contoso.com", cx);
    r.check("no CLI runs before Sign in is clicked: not on the steps, the methods or the fields", runs() == before, runs());

    click_probe("auth-aws-iam", cx);
    pause(cx, 30).await;
    fill("USER", "", cx);
    click_probe("auth-action-aws-iam", cx);
    pause(cx, 100).await;
    r.check("AWS IAM with no user: Sign in does nothing", runs() == before && method(&d, cx).3.is_empty(), format!("{:?}", method(&d, cx)));
    shut(ws, cx).await;

    let Some(_) = sheet(ws, "pg", cx).await else { return };
    fill("HOST", "db.internal", cx);
    cont(cx).await;
    click_probe("auth-entra-id", cx);
    pause(cx, 30).await;
    r.check("a host outside the method's cloud is said, under it", probed("foreign-host", cx).is_some(), "");
    click_probe("auth-aws-iam", cx);
    pause(cx, 30).await;
    r.check("off RDS, AWS IAM asks for the region", probed("field-REGION", cx).is_some(), "");
    shut(ws, cx).await;

    let Some(d) = sheet(ws, "pg", cx).await else { return };
    fill("HOST", AZURE, cx);
    cont(cx).await;
    fill("USER", "ana@contoso.com", cx);
    std::fs::write(root().join("hold"), "").expect("the fake login held");
    click_probe("auth-action-entra-id", cx);
    let waiting = until(cx, 10, |cx| method(&d, cx).3 == "Waiting for the browser").await;
    until(cx, 5, |_| runs().contains("az login")).await;
    let log = runs();
    r.check("Sign in asks for a token, and with none runs the CLI's login", waiting && log.contains("az account get-access-token") && log.contains("az login"), &log);
    until(cx, 5, |_| root().join("login.pid").is_file()).await;
    let pid = std::fs::read_to_string(root().join("login.pid")).unwrap_or_default();
    r.check("the login is running, and the button is Cancel", alive(&pid) && probed("auth-action-entra-id", cx).is_some(), &pid);
    click_probe("auth-action-entra-id", cx);
    let stopped = until(cx, 10, |_| !alive(&pid)).await;
    r.check("Cancel stops the CLI and says it is signed out", stopped && method(&d, cx).3 == "Not signed in to Azure", format!("{:?}", method(&d, cx)));
    let _ = std::fs::remove_file(root().join("hold"));
    let at = runs().lines().count();
    click_probe("auth-action-entra-id", cx);
    let signed = until(cx, 10, |cx| method(&d, cx).3 == "ana@contoso.com").await;
    let log = runs();
    let after: Vec<&str> = log.lines().skip(at).collect();
    r.check("signed out, Sign in goes straight to the login, then says who", signed && after.first().is_some_and(|l| l.starts_with("az login")) && after.iter().any(|l| l.contains("get-access-token")), format!("{:?} {after:?}", method(&d, cx)));
    r.check("signed in, the button goes", probed("auth-action-entra-id", cx).is_none(), "");
    shut(ws, cx).await;
}

async fn colors(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let Some(d) = sheet(ws, "pg", cx).await else { return };
    click_probe("swatch-3", cx);
    let color = |cx: &mut AsyncWindowContext| cx.update(|_, cx| d.read(cx).demo_color()).unwrap();
    r.check("a swatch picks the color", color(cx).0 == 3, format!("{:?}", color(cx)));
    click_probe("env-production", cx);
    click_probe("swatch-5", cx);
    r.check("on Production the colors take no click", color(cx).0 == 3, format!("{:?}", color(cx)));
    fill("HOST", "db.internal", cx);
    let id = cx.update(|_, cx| d.read(cx).profile_id().to_string()).unwrap();
    click_probe("env-none", cx);
    click_probe("env-production", cx);
    let picked = cx.update(|_, cx| { let p = d.read(cx); (p.demo_color().1, p.demo_tier()) }).unwrap();
    r.check("Production's tier defaults to Off", picked.1 == McpAccess::Off, format!("{picked:?}"));
    click_probe("env-none", cx);
    r.check("leaving Production brings the color back", color(cx).0 == 3, format!("{:?}", color(cx)));
    r.check("the default tier elsewhere is Read", cx.update(|_, cx| d.read(cx).demo_tier()).unwrap() == McpAccess::Read, "");
    click_probe("env-production", cx);
    cont(cx).await;
    fill("USER", "ana", cx);
    cont(cx).await;
    click_probe("dlg-skip", cx);
    until(cx, 5, |cx| saved(ws, &id, cx).is_some()).await;
    let p = saved(ws, &id, cx);
    r.check("saved on Production, it is production's red", p.as_ref().is_some_and(|p| p.env == Environment::Production && p.prod_red() && p.color.as_deref() == Some(crate::theme::SWATCHES[3])), format!("{:?}", p.map(|p| (p.env, p.color, p.color_picked))));
    until(cx, 5, |cx| dialog(ws, cx).is_none()).await;
}

async fn breadcrumb(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let Some(d) = sheet(ws, "pg", cx).await else { return };
    click_probe("crumb-MCP", cx);
    pause(cx, 30).await;
    r.check("a step not reached takes no click", step(&d, cx) == Some(Step::Connection), format!("{:?}", step(&d, cx)));
    fill("HOST", "db.internal", cx);
    cont(cx).await;
    click_probe("crumb-Database", cx);
    pause(cx, 50).await;
    r.check("the breadcrumb goes back to a step seen", step(&d, cx) == Some(Step::Database), format!("{:?}", step(&d, cx)));
    click_probe("engine-MySQL", cx);
    pause(cx, 50).await;
    let (s, p) = cx.update(|_, cx| (d.read(cx).step(), d.read(cx).current(cx).0)).unwrap();
    r.check("changing the database goes back to the step it was changed from", s == Step::SignIn && p.engine == Engine::MySql && p.port == 3306, format!("{s:?} {:?} {}", p.engine, p.port));
    shut(ws, cx).await;
}

/// With a tunnel the form outgrows the modal, which keeps its size: the body scrolls to the focus.
async fn scrolling(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let Some(_) = sheet(ws, "pg", cx).await else { return };
    let card = probed("dlg-card", cx);
    click_probe("ssh-toggle", cx);
    pause(cx, 50).await;
    click_probe("ssh-auth-key", cx);
    pause(cx, 50).await;
    r.check("a tunnel's fields don't change the modal's size", probed("dlg-card", cx) == card, "");
    let whole = |id: &str, cx: &mut AsyncWindowContext| probed(id, cx).zip(crate::ui::probed_shown(id)).is_some_and(|(b, s)| s.size.height >= b.size.height);
    click_probe("field-NAME", cx);
    let hidden = !whole("field-KEY FILE", cx);
    for _ in 0..8 {
        key(cx, "tab");
        draw(cx);
        pause(cx, 20).await;
    }
    r.check("Tab scrolls the body to the field it lands on", hidden && whole("field-KEY FILE", cx), format!("hidden before: {hidden}"));
    shut(ws, cx).await;
}

async fn files(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let Some(d) = sheet(ws, "sqlite", cx).await else { return };
    r.check("a file database's Connection has Test", probed("dlg-test", cx).is_some() && probed("field-FILE", cx).is_some() && probed("field-CONNECTION STRING", cx).is_none(), "");
    let file = root().join("shop.sqlite");
    dataman_core::import::create_database(Engine::Sqlite, &file).expect("a database file");
    fill("FILE", &file.to_string_lossy(), cx);
    click_probe("crumb-Sign-in", cx);
    pause(cx, 30).await;
    r.check("its Sign-in is skipped and takes no click", step(&d, cx) == Some(Step::Connection), format!("{:?}", step(&d, cx)));
    cont(cx).await;
    r.check("Continue goes on to MCP", step(&d, cx) == Some(Step::Mcp), format!("{:?}", step(&d, cx)));
    shut(ws, cx).await;
}

async fn saving(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    // Nothing listens on this port: as `save_now` does, a connect that fails leaves it saved.
    let sheet_to_mcp = async |cx: &mut AsyncWindowContext| {
        let d = sheet(ws, "pg", cx).await?;
        fill("HOST", "127.0.0.1", cx);
        fill("PORT", "9", cx);
        cont(cx).await;
        fill("USER", "ana", cx);
        cont(cx).await;
        Some(d)
    };
    let Some(d) = sheet_to_mcp(cx).await else { return };
    let id = cx.update(|_, cx| d.read(cx).profile_id().to_string()).unwrap();
    click_probe("mcp-write", cx);
    click_probe("dlg-skip", cx);
    until(cx, 5, |cx| saved(ws, &id, cx).is_some()).await;
    let p = saved(ws, &id, cx);
    r.check("Skip saves with no tier, even after picking one, and a failed connect stays saved", p.as_ref().is_some_and(|p| p.mcp.is_none()), format!("{:?}", p.map(|p| p.mcp)));
    until(cx, 5, |cx| dialog(ws, cx).is_none()).await;
    r.check("an unnamed connection is called after its host", saved(ws, &id, cx).is_some_and(|p| p.name == "127.0.0.1"), format!("{:?}", saved(ws, &id, cx).map(|p| p.name)));

    let Some(d) = sheet_to_mcp(cx).await else { return };
    let id2 = cx.update(|_, cx| d.read(cx).profile_id().to_string()).unwrap();
    click_probe("mcp-write", cx);
    key(cx, "cmd-enter");
    until(cx, 5, |cx| saved(ws, &id2, cx).is_some()).await;
    let p = saved(ws, &id2, cx);
    r.check("⌘↵ on the last step saves with the tier picked", p.as_ref().is_some_and(|p| p.mcp == Some(McpAccess::Write)), format!("{:?}", p.map(|p| p.mcp)));
    until(cx, 5, |cx| dialog(ws, cx).is_none()).await;

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dialog(Some(id.clone()), window, cx)));
    pause(cx, 100).await;
    r.check("Edit opens", dialog(ws, cx).is_some(), "");
    draw(cx);
    let form = probed("field-HOST", cx).is_some() && probed("field-USER", cx).is_some() && probed("mcp-read", cx).is_some();
    let extras = ["crumb-Database", "dlg-skip"].iter().any(|p| probed(p, cx).is_some());
    r.check("Edit is one form: connection, sign-in and tiers, no breadcrumb or Skip", form && !extras && probed("dlg-delete", cx).is_some() && probed("dlg-test", cx).is_some(), format!("{form} {extras}"));
    fill("NAME", "renamed", cx);
    click_probe("dlg-save", cx);
    until(cx, 5, |cx| saved(ws, &id, cx).is_some_and(|p| p.name == "renamed")).await;
    r.check("Save saves it", saved(ws, &id, cx).is_some_and(|p| p.name == "renamed"), "");
    until(cx, 5, |cx| dialog(ws, cx).is_none()).await;
}

async fn paste(probe: &str, text: &str, cx: &mut AsyncWindowContext) {
    set_clipboard(text, cx);
    click_probe(&format!("field-{probe}"), cx);
    key(cx, "cmd-a");
    key(cx, "cmd-v");
    pause(cx, 50).await;
}

fn password(d: &Entity<ConnDialog>, cx: &mut AsyncWindowContext) -> String {
    cx.update(|_, cx| d.read(cx).demo_field(4, cx)).unwrap_or_default()
}

/// A second string with no password never sends the first one's to its host.
async fn a_password_stays_with_its_string(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    const OTHER: &str = "postgres://bob@other.internal/sales";
    let Some(d) = sheet(ws, "pg", cx).await else { return };
    paste("CONNECTION STRING", STRING, cx).await;
    fill("DATABASE", "edited", cx);
    pause(cx, 50).await;
    r.check("an edit doesn't end the password's link to its string", password(&d, cx) == "hunter22", password(&d, cx));
    paste("CONNECTION STRING", OTHER, cx).await;
    r.check("another string with none, after an edit, drops it", password(&d, cx).is_empty(), password(&d, cx));
    cont(cx).await;
    let m = method(&d, cx);
    r.check("and Sign-in doesn't offer to keep it", m.1 == "User & password" && !m.2, format!("{m:?}"));
    shut(ws, cx).await;

    let Some(d) = sheet(ws, "pg", cx).await else { return };
    paste("CONNECTION STRING", STRING, cx).await;
    paste("HOST", OTHER, cx).await;
    r.check("a string pasted in HOST drops it too", password(&d, cx).is_empty() && cx.update(|_, cx| d.read(cx).demo_field(1, cx)).unwrap_or_default() == "other.internal", password(&d, cx));
    shut(ws, cx).await;

    let Some(d) = sheet(ws, "pg", cx).await else { return };
    paste("CONNECTION STRING", STRING, cx).await;
    fill("PASSWORD", "", cx);
    cont(cx).await;
    fill("PASSWORD", "typed", cx);
    click_probe("crumb-Connection", cx);
    pause(cx, 50).await;
    paste("CONNECTION STRING", OTHER, cx).await;
    r.check("a password typed over is the user's, and stays", password(&d, cx) == "typed", password(&d, cx));
    shut(ws, cx).await;
}

/// The step that warns where a token goes can't be skipped once the host changed.
async fn a_changed_host_goes_through_sign_in(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    for (how, chosen) in [("typed", false), ("pasted", false), ("typed", true), ("pasted", true)] {
        let Some(d) = sheet(ws, "pg", cx).await else { return };
        fill("HOST", AZURE, cx);
        cont(cx).await;
        if chosen {
            click_probe("auth-entra-id", cx);
            pause(cx, 50).await;
        }
        fill("USER", "ana@contoso.com", cx);
        cont(cx).await;
        r.check(&format!("{how}: at MCP"), step(&d, cx) == Some(Step::Mcp), format!("{:?}", step(&d, cx)));
        click_probe("crumb-Connection", cx);
        pause(cx, 50).await;
        if how == "typed" {
            fill("HOST", "db.internal", cx);
        } else {
            paste("CONNECTION STRING", "postgres://ana@db.internal/orders", cx).await;
        }
        pause(cx, 50).await;
        click_probe("crumb-MCP", cx);
        pause(cx, 50).await;
        r.check(&format!("{how}: a new host can't jump to MCP"), step(&d, cx) == Some(Step::Connection), format!("{:?}", step(&d, cx)));
        cont(cx).await;
        let m = method(&d, cx);
        if chosen {
            r.check(&format!("{how}: Entra picked by hand stays, warning where the token goes"), step(&d, cx) == Some(Step::SignIn) && m.0 == AuthKind::EntraId && probed("foreign-host", cx).is_some(), format!("{:?} {m:?}", step(&d, cx)));
        } else {
            r.check(&format!("{how}: Entra only from the host goes with it"), step(&d, cx) == Some(Step::SignIn) && m.0 == AuthKind::Password, format!("{:?} {m:?}", step(&d, cx)));
        }
        shut(ws, cx).await;
    }
}

async fn a_login_answers_for_its_method(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    for (other, picked) in [("auth-aws-iam", AuthKind::AwsIam), ("auth-password", AuthKind::Password)] {
        let _ = std::fs::remove_file(root().join("signed-in"));
        std::fs::write(root().join("gate"), "").expect("the fake login held");
        std::fs::write(root().join("runs"), "").expect("an empty log");
        let Some(d) = sheet(ws, "pg", cx).await else { return };
        fill("HOST", RDS, cx);
        cont(cx).await;
        click_probe("auth-entra-id", cx);
        pause(cx, 300).await;
        fill("USER", "ana", cx);
        click_probe("auth-action-entra-id", cx);
        until(cx, 8, |_| runs().contains("az login")).await;
        click_probe(other, cx);
        pause(cx, 300).await;
        let _ = std::fs::remove_file(root().join("gate"));
        let again = until(cx, 8, |_| runs().matches("az account get-access-token").count() >= 2).await;
        pause(cx, 300).await;
        r.check(&format!("the login ends with {picked:?} picked: only az ran"), again && !runs().contains("aws "), runs());
        r.check(&format!("{picked:?} stays picked and clean"), method(&d, cx).0 == picked && method(&d, cx).3.is_empty(), format!("{:?}", method(&d, cx)));
        click_probe("auth-entra-id", cx);
        pause(cx, 300).await;
        r.check("the method signed in says who", method(&d, cx).3 == "ana@contoso.com", format!("{:?}", method(&d, cx)));
        shut(ws, cx).await;
    }
    let _ = std::fs::remove_file(root().join("signed-in"));
}

async fn a_hidden_region_says_nothing(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let Some(d) = sheet(ws, "pg", cx).await else { return };
    fill("HOST", "db.internal", cx);
    cont(cx).await;
    click_probe("auth-aws-iam", cx);
    pause(cx, 300).await;
    fill("USER", "iam_ana", cx);
    r.check("a host that doesn't say its region asks for it", fill("REGION", "us-east-1", cx), "");
    pause(cx, 50).await;
    click_probe("field-USER", cx);
    key(cx, "tab");
    key(cx, "tab");
    draw(cx);
    let tabbed = cx.update(|window, cx| d.read(cx).demo_tab_on_region(window, cx)).unwrap_or(false);
    r.check("Tab reaches REGION", tabbed, "");
    let region = |cx: &mut AsyncWindowContext| cx.update(|_, cx| d.read(cx).current(cx).0.auth).ok();
    r.check("it counts while it shows", matches!(region(cx), Some(Auth::AwsIam { region: Some(g), .. }) if g == "us-east-1"), format!("{:?}", region(cx)));
    click_probe("crumb-Connection", cx);
    pause(cx, 50).await;
    fill("HOST", "orders.abc123.eu-west-1.rds.amazonaws.com", cx);
    cont(cx).await;
    r.check("an RDS endpoint doesn't ask", probed("field-REGION", cx).is_none(), "");
    r.check("and the old one no longer overrides it", matches!(region(cx), Some(Auth::AwsIam { region: None, .. })), format!("{:?}", region(cx)));
    shut(ws, cx).await;
}

async fn engine_changes_land_where_they_can_be_fixed(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let file = root().join("steps.sqlite");
    std::fs::write(&file, "").expect("a file to pick");
    let to_mcp = async |cx: &mut AsyncWindowContext| {
        let d = sheet(ws, "sqlite", cx).await?;
        fill("FILE", &file.display().to_string(), cx);
        pause(cx, 50).await;
        cont(cx).await;
        Some(d)
    };

    let Some(d) = sheet(ws, "pg", cx).await else { return };
    paste("CONNECTION STRING", "foo://bar", cx).await;
    click_probe("crumb-Database", cx);
    pause(cx, 50).await;
    click_probe("engine-SQLite", cx);
    pause(cx, 50).await;
    fill("FILE", &file.display().to_string(), cx);
    pause(cx, 50).await;
    cont(cx).await;
    r.check("a bad string on a server doesn't block Continue on a file", step(&d, cx) == Some(Step::Mcp), format!("{:?}", step(&d, cx)));
    shut(ws, cx).await;

    let Some(d) = to_mcp(cx).await else { return };
    click_probe("crumb-Database", cx);
    pause(cx, 50).await;
    click_probe("engine-DuckDB", cx);
    pause(cx, 50).await;
    r.check("another file database, its file cleared: back to Connection", step(&d, cx) == Some(Step::Connection), format!("{:?}", step(&d, cx)));
    shut(ws, cx).await;

    let Some(d) = to_mcp(cx).await else { return };
    click_probe("crumb-Database", cx);
    pause(cx, 50).await;
    click_probe("engine-PostgreSQL", cx);
    pause(cx, 50).await;
    fill("HOST", "db.internal", cx);
    pause(cx, 50).await;
    click_probe("crumb-MCP", cx);
    pause(cx, 50).await;
    r.check("from a file to a server: MCP waits for Sign-in", step(&d, cx) == Some(Step::Connection), format!("{:?}", step(&d, cx)));
    shut(ws, cx).await;

    // Mongo's user is optional and PostgreSQL's isn't, and neither step in between says so.
    let Some(d) = sheet(ws, "mongodb", cx).await else { return };
    fill("HOST", "db.internal", cx);
    cont(cx).await;
    cont(cx).await;
    click_probe("mcp-write", cx);
    pause(cx, 50).await;
    click_probe("crumb-Database", cx);
    pause(cx, 50).await;
    click_probe("engine-PostgreSQL", cx);
    pause(cx, 50).await;
    r.check("to PostgreSQL, back on MCP", step(&d, cx) == Some(Step::Mcp), format!("{:?}", step(&d, cx)));
    click_probe("dlg-skip", cx);
    pause(cx, 100).await;
    let said = cx.update(|_, cx| d.read(cx).demo_status(cx)).map(|s| s.0).unwrap_or_default();
    r.check("a Skip that can't save goes where it is fixed, and says why", step(&d, cx) == Some(Step::SignIn) && said.starts_with("invalid") && probed("field-USER", cx).is_some(), format!("{:?} {said}", step(&d, cx)));
    r.check("and the tier picked is still picked", cx.update(|_, cx| d.read(cx).demo_tier()).ok() == Some(McpAccess::Write), "");
    shut(ws, cx).await;
}

async fn mongo_options_follow_the_string(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let Some(d) = sheet(ws, "mongodb", cx).await else { return };
    paste("CONNECTION STRING", "mongodb+srv://u@a.example.net/x", cx).await;
    let srv = |cx: &mut AsyncWindowContext| cx.update(|_, cx| d.read(cx).current(cx).0.options.contains_key("srv")).unwrap_or(false);
    r.check("an SRV string says SRV", srv(cx), "");
    paste("CONNECTION STRING", "mongodb://b.example.net:27017/x", cx).await;
    r.check("a plain string after it doesn't", !srv(cx), "");
    shut(ws, cx).await;
}

/// Every password connection has `save_password` on: only what is in the store counts as saved.
async fn saved_means_saved(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let Some(d) = sheet(ws, "pg", cx).await else { return };
    fill("HOST", "127.0.0.1", cx);
    fill("PORT", "9", cx);
    cont(cx).await;
    fill("USER", "ana", cx);
    cont(cx).await;
    let id = cx.update(|_, cx| d.read(cx).profile_id().to_string()).unwrap();
    click_probe("dlg-skip", cx);
    until(cx, 5, |cx| saved(ws, &id, cx).is_some()).await;
    until(cx, 5, |cx| dialog(ws, cx).is_none()).await;
    let edit = |id: String, cx: &mut AsyncWindowContext| {
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dialog(Some(id), window, cx)));
    };
    edit(id.clone(), cx);
    pause(cx, 300).await;
    let Some(d) = dialog(ws, cx) else { return };
    r.check("no password saved: not offered to keep", method(&d, cx).1 == "User & password", format!("{:?}", method(&d, cx)));
    shut(ws, cx).await;
    let _ = core(ws, cx).secrets.set(&id, "hunter22");
    edit(id, cx);
    let Some(d) = dialog(ws, cx) else { return };
    let kept = until(cx, 5, |cx| method(&d, cx).1 == "Keep user & password").await;
    r.check("a password in the store: offered to keep", kept, format!("{:?}", method(&d, cx)));
    shut(ws, cx).await;
}
