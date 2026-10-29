//! MySQL and MariaDB: a user at `%`, granted per table (per column where columns are hidden),
//! masked columns read through DEFINER views in a database of its own. MySQL's DDL commits as it
//! goes: the user is created locked and unlocked last, so a half-made login cannot be used.
//!
//! MySQL hides the names of what a user has no privilege on. It has no time limit per user
//! (MariaDB has MAX_STATEMENT_TIME). Dataman's mark is the user's ATTRIBUTE on MySQL, the view
//! database's COMMENT on MariaDB (which has no user attributes).

use anyhow::{Result, anyhow};

use super::*;
use crate::backend::{Db, text_rows};

pub fn q(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// A string literal; backslashes doubled too (the default sql_mode reads them as escapes).
pub fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

pub fn account(user: &str) -> String {
    format!("{}@'%'", lit(user))
}

fn qual(schema: &str, name: &str) -> String {
    format!("{}.{}", q(schema), q(name))
}

pub fn text(g: &Grant) -> String {
    match g {
        Grant::Table { schema, name, privilege } => format!("{privilege} ON {}", qual(schema, name)),
        Grant::Column { schema, name, column, privilege } => format!("{privilege} ({}) ON {}", q(column), qual(schema, name)),
        Grant::Global(p) => format!("{p} ON *.*"),
        Grant::Limit(n) => format!("MAX_USER_CONNECTIONS {n}"),
        Grant::View { name, hash } => format!("VIEW {} -- {hash}", q(name)),
        Grant::Setting { name, value } => format!("{name} {value}"),
        other => format!("{other:?}"),
    }
}

fn grant_sql(g: &Grant, user: &str) -> Option<String> {
    Some(match g {
        Grant::Limit(n) => format!("ALTER USER {} WITH MAX_USER_CONNECTIONS {n}", account(user)),
        Grant::Setting { name, value } => format!("ALTER USER {} WITH {name} {value}", account(user)),
        Grant::View { .. } | Grant::Database(_) | Grant::Schema { .. } | Grant::Sequence { .. } | Grant::Member(_) => return None,
        g => format!("GRANT {} TO {}", text(g), account(user)),
    })
}

fn revoke_sql(g: &Grant, user: &str) -> Option<String> {
    Some(match g {
        Grant::Limit(_) => format!("ALTER USER {} WITH MAX_USER_CONNECTIONS 0", account(user)),
        Grant::Setting { name, .. } => format!("ALTER USER {} WITH {name} 0", account(user)),
        Grant::View { .. } | Grant::Database(_) | Grant::Schema { .. } | Grant::Sequence { .. } | Grant::Member(_) => return None,
        g => format!("REVOKE {} FROM {}", text(g), account(user)),
    })
}

pub fn mask_expr(col: &str, m: Mask, space: &str) -> String {
    let c = format!("CAST({} AS CHAR)", q(col));
    match m {
        Mask::Hidden => "'‹hidden›'".into(),
        Mask::Partial => format!(
            "CASE WHEN {c} IS NULL THEN NULL WHEN LOCATE('@', {c}) > 0 THEN CONCAT(LEFT({c}, 1), '•••@', SUBSTRING_INDEX({c}, '@', -1)) WHEN CHAR_LENGTH({c}) >= 8 THEN CONCAT('•••• ', RIGHT({c}, 4)) ELSE '••••' END"
        ),
        Mask::Hash => format!("CASE WHEN {c} IS NULL THEN NULL ELSE CONCAT('‹h:', LEFT(SHA2(CONCAT({c}, (SELECT v FROM {}.`salt`)), 256), 12), '›') END", q(space)),
    }
}

pub struct Desired {
    pub grants: Vec<Grant>,
    pub views: Vec<ViewDef>,
    pub access: Vec<Access>,
    pub hidden: Vec<String>,
    pub not_granted: Vec<NotGranted>,
    pub salt: bool,
    pub exposed: Vec<NotGranted>,
}

pub fn desired(spec: &Spec, inv: &Inventory, probe: &Probe) -> Desired {
    let (all, hidden) = access(spec, inv);
    let (mut grants, mut views, mut not_granted, mut ok) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut salt = false;
    for a in all {
        if !spec.rows {
            continue;
        }
        let verbs: &[&str] = if a.write { &["SELECT", "INSERT", "UPDATE", "DELETE"] } else { &["SELECT"] };
        if !a.grantable {
            not_granted.push(NotGranted {
                object: a.label(),
                reason: format!("{} can't grant on it (no GRANT OPTION there)", probe.login),
                sql: vec![format!("GRANT {} ON {} TO {};", verbs.join(", "), qual(&a.schema, &a.name), account(&spec.user))],
            });
            continue;
        }
        if a.whole {
            for v in verbs {
                grants.push(Grant::Table { schema: a.schema.clone(), name: a.name.clone(), privilege: v.to_string() });
            }
        } else {
            let mut privs = vec!["SELECT"];
            if a.write {
                privs.extend(["INSERT", "UPDATE"]);
                grants.push(Grant::Table { schema: a.schema.clone(), name: a.name.clone(), privilege: "DELETE".into() });
            }
            for p in privs {
                for c in &a.plain {
                    grants.push(Grant::Column { schema: a.schema.clone(), name: a.name.clone(), column: c.clone(), privilege: p.into() });
                }
            }
        }
        if !a.masked.is_empty() {
            salt |= a.masked.iter().any(|(_, m)| *m == Mask::Hash);
            let name = view_name(&a.schema, &a.name, "", 64, &views);
            let cols: Vec<String> = a.plain.iter().map(|c| q(c)).chain(a.masked.iter().map(|(c, m)| format!("{} AS {}", mask_expr(c, *m, &spec.space), q(c)))).collect();
            let body = format!("SELECT {} FROM {}", cols.join(", "), qual(&a.schema, &a.name));
            let hash = hash::sha256_hex(body.as_bytes())[..12].to_string();
            let create = format!("CREATE SQL SECURITY DEFINER VIEW {} AS {body}", qual(&spec.space, &name));
            grants.push(Grant::Table { schema: spec.space.clone(), name: name.clone(), privilege: "SELECT".into() });
            grants.push(Grant::View { name: name.clone(), hash: hash.clone() });
            views.push(ViewDef { name: name.clone(), create, hash, map: ViewMap { schema: a.schema.clone(), table: a.name.clone(), view: qual(&spec.space, &name), masked: a.masked.clone() } });
        }
        ok.push(a);
    }
    if spec.activity {
        grants.push(Grant::Global("PROCESS".into()));
    }
    if let Some(n) = spec.conn_limit {
        grants.push(Grant::Limit(n));
    }
    if probe.flavor.mariadb {
        grants.push(Grant::Setting { name: "MAX_STATEMENT_TIME".into(), value: spec.time_limit_s.to_string() });
    }
    grants.sort();
    grants.dedup();
    let exposed = exposed(spec, inv, &|_, _| String::new());
    Desired { grants, views, access: ok, hidden, not_granted, salt, exposed }
}

fn rules(spec: &Spec, d: &Desired, probe: &Probe, prod: bool) -> Vec<RuleLine> {
    let mut v = vec![line("A login apart from yours", "CREATE USER, its own password", Who::Database)];
    if !spec.rows {
        v.push(line("Structure only", "no table granted (MySQL then hides the tables too)", Who::Database));
    } else if spec.write {
        v.push(line("Rows: read and write, no DDL", "only SELECT/INSERT/UPDATE/DELETE granted", Who::Database));
    } else {
        v.push(line("Rows: read only", "only SELECT granted", Who::Database));
    }
    if !d.hidden.is_empty() {
        v.push(line(hidden_label(&d.hidden), "no grant; MySQL hides the names too", Who::Database));
    }
    v.extend(column_lines(&d.access, ("column grants", Who::Database), (&format!("DEFINER views in database {}", spec.space), Who::Database), spec.rows));
    if !spec.code {
        v.push(line("Run code off", "no EXECUTE granted", Who::Database));
    }
    v.push(line(if spec.activity { "Server activity" } else { "No server activity" }, if spec.activity { "PROCESS" } else { "no PROCESS: only its own threads" }, Who::Database));
    if probe.flavor.mariadb {
        v.push(line(time_label(spec.time_limit_s), "MAX_STATEMENT_TIME on the user", Who::Database));
    } else {
        v.push(line(time_label(spec.time_limit_s), "MySQL has no time limit per user", Who::Dataman));
    }
    if spec.conn_limit.is_some() {
        v.push(line("Connections", "MAX_USER_CONNECTIONS", Who::Database));
    }
    v.extend(dataman_lines(spec, prod));
    v
}

fn group_of(g: &Grant, spec: &Spec, d: &Desired) -> String {
    match g {
        Grant::Table { schema, .. } if *schema == spec.space => "the views".into(),
        Grant::Table { .. } => format!("tables the agent may use ({})", d.access.len()),
        Grant::Column { .. } => "tables with hidden or masked columns: only the others".into(),
        Grant::Global(_) => "server activity".into(),
        _ => "limits".into(),
    }
}

fn view_steps(spec: &Spec, d: &Desired, salt_hex: &str, create_db: bool, mariadb: bool) -> Vec<Step> {
    let g = format!("masked columns: views the agent reads instead ({})", d.views.len());
    let mut steps = Vec::new();
    if create_db {
        let comment = if mariadb { format!(" COMMENT {}", lit(&mark_for(&spec.conn))) } else { String::new() };
        steps.push(Step::sql(&g, format!("CREATE DATABASE {}{comment}", q(&spec.space))));
    }
    if d.salt {
        steps.push(Step::sql(&g, format!("CREATE TABLE IF NOT EXISTS {}.`salt` (v VARBINARY(64) NOT NULL)", q(&spec.space))));
        steps.push(Step::secret_sql(
            &g,
            format!("INSERT INTO {0}.`salt` SELECT UNHEX('{salt_hex}') FROM DUAL WHERE NOT EXISTS (SELECT 1 FROM {0}.`salt`)", q(&spec.space)),
            format!("INSERT INTO {}.`salt` VALUES (<random salt>)", q(&spec.space)),
        ));
    }
    for v in &d.views {
        steps.push(Step::sql(&g, v.create.clone()));
    }
    steps
}

fn create_user(spec: &Spec, password: &str, probe: &Probe) -> Step {
    let mut tail = String::new();
    if probe.tls {
        tail.push_str(" REQUIRE SSL");
    }
    let mut with = Vec::new();
    if let Some(n) = spec.conn_limit {
        with.push(format!("MAX_USER_CONNECTIONS {n}"));
    }
    if probe.flavor.mariadb {
        with.push(format!("MAX_STATEMENT_TIME {}", spec.time_limit_s));
    }
    if !with.is_empty() {
        tail.push_str(&format!(" WITH {}", with.join(" ")));
    }
    if let Some(d) = spec.expires_days {
        tail.push_str(&format!(" PASSWORD EXPIRE INTERVAL {d} DAY"));
    }
    // Its own password can't be changed by itself (it never knows the current one): only an
    // admin's ALTER USER sets it (MariaDB lets a user change its own, said in the rules).
    if !probe.flavor.mariadb {
        tail.push_str(" PASSWORD REQUIRE CURRENT");
    }
    tail.push_str(" ACCOUNT LOCK");
    if !probe.flavor.mariadb {
        tail.push_str(&format!(" ATTRIBUTE {}", lit(&serde_json::json!({"dataman": MARK, "conn": spec.conn}).to_string())));
    }
    let g = "the login, locked until the end (password sent as its hash, never in clear)";
    let (auth, shown) = if probe.flavor.mariadb {
        (format!("IDENTIFIED BY PASSWORD {}", lit(&hash::mariadb_native(password))), "IDENTIFIED BY PASSWORD '*…'".to_string())
    } else {
        (format!("IDENTIFIED WITH caching_sha2_password AS {}", lit(&hash::mysql_caching_sha2(password))), "IDENTIFIED WITH caching_sha2_password AS '$A$005$…'".to_string())
    };
    Step::secret_sql(g, format!("CREATE USER {} {auth}{tail}", account(&spec.user)), format!("CREATE USER {} {shown}{tail}", account(&spec.user)))
}

/// MariaDB has no user attributes: the views' hashes are kept in a table of Dataman's database
/// (the agent is granted the views only).
pub const MARIADB_MARKS: &str = "dataman_views";

fn view_marks(spec: &Spec, d: &Desired, gone: &[String], probe: &Probe) -> Vec<Step> {
    if d.views.is_empty() && gone.is_empty() {
        return Vec::new();
    }
    if probe.flavor.mariadb {
        let t = format!("{}.{}", q(&spec.space), q(MARIADB_MARKS));
        let mut steps = vec![Step::sql("the views", format!("CREATE TABLE IF NOT EXISTS {t} (name VARCHAR(64) NOT NULL PRIMARY KEY, hash VARCHAR(64) NOT NULL)"))];
        for g in gone {
            steps.push(Step::sql("the views", format!("DELETE FROM {t} WHERE name = {}", lit(g))));
        }
        for v in &d.views {
            steps.push(Step::sql("the views", format!("REPLACE INTO {t} (name, hash) VALUES ({}, {})", lit(&v.name), lit(&v.hash))));
        }
        return steps;
    }
    let mut m = serde_json::Map::new();
    for v in &d.views {
        m.insert(v.name.clone(), serde_json::Value::String(v.hash.clone()));
    }
    for g in gone {
        m.entry(g.clone()).or_insert(serde_json::Value::Null);
    }
    let attr = serde_json::json!({ "views": m }).to_string();
    vec![Step::sql("the views", format!("ALTER USER {} ATTRIBUTE {}", account(&spec.user), lit(&attr)))]
}

pub fn create_plan(spec: &Spec, inv: &Inventory, probe: &Probe, password: &str, prod: bool) -> Plan {
    let d = desired(spec, inv, probe);
    let mut steps = vec![create_user(spec, password, probe)];
    let salt = hash::hex(&hash::random_bytes(32));
    // MariaDB's mark is its database's COMMENT: made even without a view to put there.
    let make_db = !d.views.is_empty() || probe.flavor.mariadb;
    if make_db {
        steps.extend(view_steps(spec, &d, &salt, true, probe.flavor.mariadb));
    }
    for (s, n, cols) in granted_objects(&d.grants) {
        let group = if s == spec.space { "the views".to_string() } else if cols { "tables with hidden or masked columns: only the others".to_string() } else { format!("tables the agent may use ({})", d.access.len()) };
        steps.push(Step::sql(&group, format!("GRANT {} ON {} TO {}", privilege_list(&d.grants, &s, &n, &q), qual(&s, &n), account(&spec.user))));
    }
    for g in d.grants.iter().filter(|g| !matches!(g, Grant::Limit(_) | Grant::Setting { .. } | Grant::Table { .. } | Grant::Column { .. })) {
        if let Some(sql) = grant_sql(g, &spec.user) {
            steps.push(Step::sql(&group_of(g, spec, &d), sql));
        }
    }
    steps.extend(view_marks(spec, &d, &[], probe));
    if !d.hidden.is_empty() {
        steps.push(Step::note("", format!("hidden: {} → no grant at all", short_list(&d.hidden))));
    }
    steps.push(Step::sql("ready: unlocked last", format!("ALTER USER {} ACCOUNT UNLOCK", account(&spec.user))));
    let made = if make_db { vec![format!("database:{}", spec.space)] } else { Vec::new() };
    seal(spec, d, probe, steps, made, prod)
}

fn seal(spec: &Spec, d: Desired, probe: &Probe, steps: Vec<Step>, made: Vec<String>, prod: bool) -> Plan {
    let rules = rules(spec, &d, probe, prod);
    let notes = vec!["MySQL commits each statement as it runs: the login stays locked until the last one, so a half-made login can't be used.".to_string()];
    let exposed = d.exposed;
    let plan = Plan {
        engine: Some(Engine::MySql),
        user: spec.user.clone(),
        space: spec.space.clone(),
        steps,
        one_tx: false,
        rules,
        lines: d.grants.iter().map(text).collect(),
        not_granted: d.not_granted,
        exposed: Vec::new(),
        lacks: probe.lacks.clone(),
        notes,
        made,
        granted: d.access.len(),
        views: d.views.into_iter().map(|v| v.map).collect(),
        hidden: d.hidden,
        manual: None,
        hash: String::new(),
    }
    .seal();
    with_exposed(plan, exposed)
}

pub fn sync_plan(spec: &Spec, inv: &Inventory, probe: &Probe, have: &[Grant], have_views: &[(String, String)], db_exists: bool, prod: bool) -> Plan {
    let d = desired(spec, inv, probe);
    let mut steps = Vec::new();
    for g in have.iter().filter(|g| !d.grants.contains(g)) {
        if let Some(sql) = revoke_sql(g, &spec.user) {
            steps.push(Step::sql("revoked: the rules no longer allow it", sql));
        }
    }
    let mut gone = Vec::new();
    for (name, hash) in have_views {
        if !d.views.iter().any(|v| v.name == *name && v.hash == *hash) {
            steps.push(Step::sql("views", format!("DROP VIEW IF EXISTS {}", qual(&spec.space, name))));
            if !d.views.iter().any(|v| v.name == *name) {
                gone.push(name.clone());
            }
        }
    }
    let fresh: Vec<&ViewDef> = d.views.iter().filter(|v| !have_views.iter().any(|(n, h)| *n == v.name && *h == v.hash)).collect();
    if !fresh.is_empty() || (probe.flavor.mariadb && !db_exists) {
        if !db_exists {
            let comment = if probe.flavor.mariadb { format!(" COMMENT {}", lit(&mark_for(&spec.conn))) } else { String::new() };
            steps.push(Step::sql("views", format!("CREATE DATABASE {}{comment}", q(&spec.space))));
        }
        if d.salt {
            let salt = hash::hex(&hash::random_bytes(32));
            steps.push(Step::sql("views", format!("CREATE TABLE IF NOT EXISTS {}.`salt` (v VARBINARY(64) NOT NULL)", q(&spec.space))));
            steps.push(Step::secret_sql(
                "views",
                format!("INSERT INTO {0}.`salt` SELECT UNHEX('{salt}') FROM DUAL WHERE NOT EXISTS (SELECT 1 FROM {0}.`salt`)", q(&spec.space)),
                format!("INSERT INTO {}.`salt` VALUES (<random salt>) -- if empty", q(&spec.space)),
            ));
        }
        for v in &fresh {
            steps.push(Step::sql("views", v.create.replacen("CREATE ", "CREATE OR REPLACE ", 1)));
        }
    }
    for g in d.grants.iter().filter(|g| !have.contains(g)) {
        if let Some(sql) = grant_sql(g, &spec.user) {
            steps.push(Step::sql("granted: the rules allow it now", sql));
        }
    }
    if !fresh.is_empty() || !gone.is_empty() {
        steps.extend(view_marks(spec, &d, &gone, probe));
    }
    let made = if d.views.is_empty() && !db_exists && !probe.flavor.mariadb { Vec::new() } else { vec![format!("database:{}", spec.space)] };
    seal(spec, d, probe, steps, made, prod)
}

/// Removing it: Dataman's views and their database (only if nothing else is in it), then the
/// user, which takes every privilege it holds with it.
pub fn remove_plan(user: &str, space: &str, views: &[String], salt: bool, drop_db: bool) -> Plan {
    let mut steps = Vec::new();
    for v in views {
        steps.push(Step::sql("Dataman's views", format!("DROP VIEW IF EXISTS {}", qual(space, v))));
    }
    if salt {
        steps.push(Step::sql("Dataman's views", format!("DROP TABLE IF EXISTS {}.`salt`", q(space))));
        steps.push(Step::sql("Dataman's views", format!("DROP TABLE IF EXISTS {}.{}", q(space), q(MARIADB_MARKS))));
    }
    if drop_db {
        steps.push(Step::sql("Dataman's views", format!("DROP DATABASE IF EXISTS {}", q(space))));
    }
    steps.push(Step::sql("the login and every privilege it holds", format!("DROP USER {}", account(user))));
    Plan { engine: Some(Engine::MySql), user: user.into(), space: space.into(), steps, ..Default::default() }
}

pub fn rotate_step(user: &str, password: &str, mariadb: bool) -> Step {
    if mariadb {
        let sql = format!("ALTER USER {} IDENTIFIED BY PASSWORD {}", account(user), lit(&hash::mariadb_native(password)));
        return Step::secret_sql("a new password", sql, format!("ALTER USER {} IDENTIFIED BY PASSWORD '*…'", account(user)));
    }
    let sql = format!("ALTER USER {} IDENTIFIED WITH caching_sha2_password AS {}", account(user), lit(&hash::mysql_caching_sha2(password)));
    Step::secret_sql("a new password", sql, format!("ALTER USER {} IDENTIFIED WITH caching_sha2_password AS '$A$005$…'", account(user)))
}

// ---- Live ----

fn s(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

const SYSTEM: &str = "('mysql', 'sys', 'performance_schema', 'information_schema')";

pub(crate) async fn probe(db: &Db, user: &str, conn: &str) -> Result<Probe> {
    let r = text_rows(db, "SELECT CURRENT_USER(), VERSION(), @@version_comment, @@read_only").await?;
    let row = r.first().ok_or_else(|| anyhow!("the server did not say who this login is"))?;
    let (login, version) = (s(&row[0]), s(&row[1]));
    let mariadb = version.to_ascii_lowercase().contains("mariadb") || s(&row[2]).to_ascii_lowercase().contains("mariadb");
    let major = version.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let read_only = s(&row[3]) == "1";
    let grants: Vec<String> = text_rows(db, "SHOW GRANTS").await?.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
    let all = |g: &str| g.contains(" ON *.* ") && g.contains("WITH GRANT OPTION") && (g.contains("ALL PRIVILEGES") || g.contains("GRANT SELECT"));
    let superuser = grants.iter().any(|g| all(g) && g.contains("ALL PRIVILEGES"));
    let create_user = grants.iter().any(|g| g.contains(" ON *.* ") && (g.contains("ALL PRIVILEGES") || g.contains("CREATE USER")));
    let tls = text_rows(db, "SHOW SESSION STATUS LIKE 'Ssl_cipher'").await.ok().and_then(|r| r.first().map(|x| !s(&x[1]).is_empty())).unwrap_or(false);
    let mut p = Probe { login: login.clone(), version: format!("{} {version}", if mariadb { "MariaDB" } else { "MySQL" }), tls, ..Default::default() };
    p.flavor = Flavor { major, mariadb, superuser, activity: true, ..Default::default() };
    p.can_create = create_user && !read_only;
    if read_only {
        p.lacks.push("this server is read-only (a replica?): create the login on the primary".into());
    } else if !create_user {
        p.lacks.push(format!("{login} can't create users: it needs CREATE USER and GRANT OPTION"));
    }
    if !mariadb && major < 8 {
        p.can_create = false;
        p.lacks.push("MySQL 8.0 or later is needed (user attributes and caching_sha2_password)".into());
    }
    p.checks.push((p.can_create, if p.can_create { format!("{login} can create users") } else { format!("{login} can't create users") }));
    p.checks.push((tls, if tls { "TLS on".into() } else { "TLS off".into() }));
    p.exists = exists(db, user, conn, mariadb).await?;
    Ok(p)
}

pub(crate) async fn exists(db: &Db, user: &str, conn: &str, mariadb: bool) -> Result<Option<bool>> {
    let there = !text_rows(db, &format!("SELECT 1 FROM mysql.user WHERE User = {} AND Host = '%'", lit(user))).await.unwrap_or_default().is_empty()
        || text_rows(db, &format!("SHOW GRANTS FOR {}", account(user))).await.is_ok();
    if !there {
        return Ok(None);
    }
    let marked = if mariadb {
        text_rows(db, &format!("SELECT SCHEMA_COMMENT FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = {}", lit(&space_of(user))))
            .await
            .ok()
            .and_then(|r| r.first().map(|x| s(&x[0])))
            .is_some_and(|c| marked_for(&c, conn))
    } else {
        text_rows(db, &format!("SELECT ATTRIBUTE FROM information_schema.USER_ATTRIBUTES WHERE USER = {} AND HOST = '%'", lit(user)))
            .await
            .ok()
            .and_then(|r| r.first().map(|x| s(&x[0])))
            .and_then(|a| serde_json::from_str::<serde_json::Value>(&a).ok())
            .is_some_and(|v| v["dataman"] == MARK && v["conn"] == conn)
    };
    Ok(Some(marked))
}

/// Tables and views of every database but MySQL's own and Dataman's, as the user's login can
/// grant them.
pub(crate) async fn inventory(db: &Db, login: &str) -> Result<Inventory> {
    let grantee = grantee_of(login);
    let cols = text_rows(
        db,
        &format!(
            "SELECT c.TABLE_SCHEMA, c.TABLE_NAME, t.TABLE_TYPE, c.COLUMN_NAME, c.EXTRA FROM information_schema.COLUMNS c \
             JOIN information_schema.TABLES t ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME \
             WHERE c.TABLE_SCHEMA NOT IN {SYSTEM} AND c.TABLE_SCHEMA NOT LIKE 'dataman\\_agent\\_%' ORDER BY c.TABLE_SCHEMA, c.TABLE_NAME, c.ORDINAL_POSITION"
        ),
    )
    .await?;
    let global: Vec<(String, bool)> = text_rows(db, &format!("SELECT PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.USER_PRIVILEGES WHERE GRANTEE = {}", lit(&grantee))).await?.iter().map(|r| (s(&r[0]), s(&r[1]) == "YES")).collect();
    let schema: Vec<(String, String, bool)> = text_rows(db, &format!("SELECT TABLE_SCHEMA, PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.SCHEMA_PRIVILEGES WHERE GRANTEE = {}", lit(&grantee))).await?.iter().map(|r| (s(&r[0]), s(&r[1]), s(&r[2]) == "YES")).collect();
    let table: Vec<(String, String, String, bool)> =
        text_rows(db, &format!("SELECT TABLE_SCHEMA, TABLE_NAME, PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.TABLE_PRIVILEGES WHERE GRANTEE = {}", lit(&grantee))).await?.iter().map(|r| (s(&r[0]), s(&r[1]), s(&r[2]), s(&r[3]) == "YES")).collect();
    let holds = |sch: &str, t: &str, p: &str| {
        global.iter().any(|(x, g)| x == p && *g)
            || schema.iter().any(|(s, x, g)| s.eq_ignore_ascii_case(sch) && x == p && *g)
            || table.iter().any(|(s, n, x, g)| s.eq_ignore_ascii_case(sch) && n == t && x == p && *g)
    };
    let mut out = Inventory::default();
    for r in cols {
        let (sch, name, ty, col, extra) = (s(&r[0]), s(&r[1]), s(&r[2]), s(&r[3]), s(&r[4]));
        if out.objects.last().is_none_or(|o: &Obj| o.schema != sch || o.name != name) {
            let view = ty.contains("VIEW");
            let need: &[&str] = if view { &["SELECT"] } else { &["SELECT", "INSERT", "UPDATE", "DELETE"] };
            let grantable = need.iter().all(|p| holds(&sch, &name, p));
            out.objects.push(Obj { schema: sch.clone(), name: name.clone(), kind: if view { ObjKind::View } else { ObjKind::Table }, columns: Vec::new(), grantable, owner: String::new(), sequences: Vec::new() });
        }
        let o = out.objects.last_mut().expect("pushed");
        o.columns.push(Col { name: col, generated: extra.to_ascii_uppercase().contains("GENERATED") });
    }
    // Roles MySQL gives every account (the agent may SET ROLE them).
    if let Ok(r) = text_rows(db, "SELECT @@global.mandatory_roles").await {
        let roles = r.first().and_then(|x| x[0].clone()).unwrap_or_default();
        out.open_roles = roles.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect();
    }
    Ok(out)
}

/// `'user'@'host'` as information_schema writes grantees.
fn grantee_of(current_user: &str) -> String {
    let (u, h) = current_user.rsplit_once('@').unwrap_or((current_user, "%"));
    format!("'{u}'@'{h}'")
}

pub(crate) async fn have(db: &Db, user: &str, space: &str, mariadb: bool) -> Result<(Vec<Grant>, Vec<(String, String)>, bool)> {
    let g = lit(&format!("'{user}'@'%'"));
    let mut out = Vec::new();
    for r in text_rows(db, &format!("SELECT TABLE_SCHEMA, TABLE_NAME, PRIVILEGE_TYPE FROM information_schema.TABLE_PRIVILEGES WHERE GRANTEE = {g}")).await? {
        out.push(Grant::Table { schema: s(&r[0]), name: s(&r[1]), privilege: s(&r[2]) });
    }
    for r in text_rows(db, &format!("SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, PRIVILEGE_TYPE FROM information_schema.COLUMN_PRIVILEGES WHERE GRANTEE = {g}")).await? {
        out.push(Grant::Column { schema: s(&r[0]), name: s(&r[1]), column: s(&r[2]), privilege: s(&r[3]) });
    }
    for r in text_rows(db, &format!("SELECT PRIVILEGE_TYPE FROM information_schema.USER_PRIVILEGES WHERE GRANTEE = {g}")).await? {
        let p = s(&r[0]);
        if p != "USAGE" {
            out.push(Grant::Global(p));
        }
    }
    let create = text_rows(db, &format!("SHOW CREATE USER {}", account(user))).await?.first().map(|r| s(&r[0])).unwrap_or_default();
    let num_after = |k: &str| create.split(k).nth(1).and_then(|r| r.split_whitespace().next()).and_then(|n| n.trim_matches(|c: char| !c.is_ascii_digit()).parse::<u32>().ok());
    if let Some(n) = num_after("MAX_USER_CONNECTIONS").filter(|n| *n > 0) {
        out.push(Grant::Limit(n));
    }
    if mariadb && let Some(n) = num_after("MAX_STATEMENT_TIME").filter(|n| *n > 0) {
        out.push(Grant::Setting { name: "MAX_STATEMENT_TIME".into(), value: n.to_string() });
    }
    let mut views = Vec::new();
    if mariadb {
        let t = format!("{}.{}", q(space), q(MARIADB_MARKS));
        for r in text_rows(db, &format!("SELECT name, hash FROM {t}")).await.unwrap_or_default() {
            views.push((s(&r[0]), s(&r[1])));
        }
    } else {
        let attr = text_rows(db, &format!("SELECT ATTRIBUTE FROM information_schema.USER_ATTRIBUTES WHERE USER = {} AND HOST = '%'", lit(user))).await?;
        if let Some(v) = attr.first().map(|r| s(&r[0])).and_then(|a| serde_json::from_str::<serde_json::Value>(&a).ok())
            && let Some(m) = v["views"].as_object()
        {
            for (name, h) in m {
                if let Some(h) = h.as_str() {
                    views.push((name.clone(), h.to_string()));
                }
            }
        }
    }
    let existing: Vec<String> = text_rows(db, &format!("SELECT TABLE_NAME FROM information_schema.VIEWS WHERE TABLE_SCHEMA = {}", lit(space))).await?.iter().map(|r| s(&r[0])).collect();
    views.retain(|(n, _)| existing.contains(n));
    for (n, h) in &views {
        out.push(Grant::View { name: n.clone(), hash: h.clone() });
    }
    let db_exists = !text_rows(db, &format!("SELECT 1 FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = {}", lit(space))).await?.is_empty();
    out.sort();
    out.dedup();
    Ok((out, views, db_exists))
}

/// Before Remove: the views in Dataman's database, and whether only Dataman's things are there.
pub(crate) async fn made_here(db: &Db, space: &str) -> Result<(Vec<String>, bool, bool)> {
    let rows = text_rows(db, &format!("SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = {}", lit(space))).await?;
    let mut views = Vec::new();
    let (mut salt, mut other) = (false, false);
    for r in rows {
        match (s(&r[1]).as_str(), s(&r[0]).as_str()) {
            (t, n) if t.contains("VIEW") => views.push(n.to_string()),
            (_, "salt") => salt = true,
            (_, MARIADB_MARKS) => salt = true,
            _ => other = true,
        }
    }
    let exists = !text_rows(db, &format!("SELECT 1 FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = {}", lit(space))).await?.is_empty();
    Ok((views, salt, exists && !other))
}
