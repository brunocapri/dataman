//! PostgreSQL: a role with LOGIN, granted per table (per column where columns are hidden),
//! masked columns read through `security_barrier` views in a schema of its own, defaults for
//! its sessions, and the predefined roles for server activity. All in one transaction.
//!
//! PostgreSQL shows every table's and column's name to every role (`pg_catalog`): a hidden table
//! is not granted, so its rows stay out of reach, but its name is not hidden. Functions are
//! executable by PUBLIC, so "Run code" off stays Dataman's.

use std::collections::BTreeSet;

use anyhow::{Result, anyhow};

use super::*;
use crate::backend::{Db, text_rows};

/// Always quoted: names are data here, never SQL.
pub fn q(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn qual(schema: &str, name: &str) -> String {
    format!("{}.{}", q(schema), q(name))
}

/// The grant as the statement says it, without `GRANT`/`TO`.
pub fn text(g: &Grant) -> String {
    match g {
        Grant::Database(p) => format!("{p}"),
        Grant::Schema { schema, privilege } => format!("{privilege} ON SCHEMA {}", q(schema)),
        Grant::Table { schema, name, privilege } => format!("{privilege} ON {}", qual(schema, name)),
        Grant::Column { schema, name, column, privilege } => format!("{privilege} ({}) ON {}", q(column), qual(schema, name)),
        Grant::Sequence { schema, name } => format!("USAGE ON SEQUENCE {}", qual(schema, name)),
        Grant::Member(r) => q(r),
        Grant::Global(p) => p.clone(),
        Grant::Setting { name, value } => format!("SET {name} = {}", lit(value)),
        Grant::Limit(n) => format!("CONNECTION LIMIT {n}"),
        Grant::View { name, hash } => format!("VIEW {} -- {hash}", q(name)),
    }
}

fn grant_sql(g: &Grant, user: &str, db: &str) -> Option<String> {
    let u = q(user);
    Some(match g {
        Grant::Setting { name, value } => format!("ALTER ROLE {u} IN DATABASE {} SET {name} = {}", q(db), lit(value)),
        Grant::Limit(n) => format!("ALTER ROLE {u} CONNECTION LIMIT {n}"),
        Grant::View { .. } => return None,
        g => format!("GRANT {} TO {u}", text(g)),
    })
}

fn revoke_sql(g: &Grant, user: &str, db: &str) -> Option<String> {
    let u = q(user);
    Some(match g {
        Grant::Setting { name, .. } => format!("ALTER ROLE {u} IN DATABASE {} RESET {name}", q(db)),
        Grant::Limit(_) => format!("ALTER ROLE {u} CONNECTION LIMIT -1"),
        Grant::View { .. } => return None,
        g => format!("REVOKE {} FROM {u}", text(g)),
    })
}

/// A masked column, as the view shows it. `salt`: reads the secret salt only the view's owner
/// can read.
pub fn mask_expr(col: &str, m: Mask, space: &str) -> String {
    let c = format!("{}::text", q(col));
    match m {
        Mask::Hidden => "'‹hidden›'::text".into(),
        Mask::Partial => format!(
            "CASE WHEN {c} IS NULL THEN NULL WHEN strpos({c}, '@') > 0 THEN left({c}, 1) || '•••@' || split_part({c}, '@', 2) WHEN length({c}) >= 8 THEN '•••• ' || right({c}, 4) ELSE '••••' END"
        ),
        Mask::Hash => format!(
            "CASE WHEN {c} IS NULL THEN NULL ELSE '‹h:' || left(encode(sha256(convert_to({c}, 'UTF8') || (SELECT v FROM {}.salt)), 'hex'), 12) || '›' END",
            q(space)
        ),
    }
}

pub struct Desired {
    pub grants: Vec<Grant>,
    pub views: Vec<ViewDef>,
    pub access: Vec<Access>,
    pub hidden: Vec<String>,
    pub not_granted: Vec<NotGranted>,
    pub salt: bool,
}

pub const DEFAULT_SCHEMA: &str = "public";

pub fn desired(spec: &Spec, inv: &Inventory, probe: &Probe) -> Desired {
    let (all, hidden) = access(spec, inv);
    let mut grants = vec![Grant::Database(format!("CONNECT ON DATABASE {}", q(&spec.database)))];
    let (mut views, mut not_granted, mut access_ok) = (Vec::new(), Vec::new(), Vec::new());
    let mut schemas = BTreeSet::new();
    let mut salt = false;
    let verbs: &[&str] = if spec.write { &["SELECT", "INSERT", "UPDATE", "DELETE"] } else { &["SELECT"] };
    for a in all {
        if !spec.rows {
            // Structure only: no table is granted; the catalog shows the structure.
            continue;
        }
        if !a.grantable && !probe.flavor.superuser {
            let list = verbs.join(", ");
            not_granted.push(NotGranted {
                object: a.label(),
                reason: format!("owned by {}", if a.owner.is_empty() { "another role" } else { &a.owner }),
                sql: vec![format!("GRANT {list} ON {} TO {};", qual(&a.schema, &a.name), q(&spec.user))],
            });
            continue;
        }
        schemas.insert(a.schema.clone());
        let write = a.write;
        if a.whole {
            for v in verbs.iter().filter(|v| write || **v == "SELECT") {
                grants.push(Grant::Table { schema: a.schema.clone(), name: a.name.clone(), privilege: v.to_string() });
            }
        } else {
            let mut privs = vec!["SELECT"];
            if write {
                privs.extend(["INSERT", "UPDATE"]);
                grants.push(Grant::Table { schema: a.schema.clone(), name: a.name.clone(), privilege: "DELETE".into() });
            }
            for p in privs {
                for c in &a.plain {
                    grants.push(Grant::Column { schema: a.schema.clone(), name: a.name.clone(), column: c.clone(), privilege: p.into() });
                }
            }
        }
        if write {
            for s in &a.sequences {
                grants.push(Grant::Sequence { schema: a.schema.clone(), name: s.clone() });
            }
        }
        if !a.masked.is_empty() {
            salt |= a.masked.iter().any(|(_, m)| *m == Mask::Hash);
            let name = view_name(&a.schema, &a.name, DEFAULT_SCHEMA, 63);
            let cols: Vec<String> = a.plain.iter().map(|c| q(c)).chain(a.masked.iter().map(|(c, m)| format!("{} AS {}", mask_expr(c, *m, &spec.space), q(c)))).collect();
            let body = format!("SELECT {} FROM {}", cols.join(", "), qual(&a.schema, &a.name));
            let hash = hash::sha256_hex(body.as_bytes())[..12].to_string();
            let create = format!("CREATE VIEW {} WITH (security_barrier) AS {body}", qual(&spec.space, &name));
            grants.push(Grant::Table { schema: spec.space.clone(), name: name.clone(), privilege: "SELECT".into() });
            grants.push(Grant::View { name: name.clone(), hash: hash.clone() });
            views.push(ViewDef { name: name.clone(), create, hash, map: ViewMap { schema: a.schema.clone(), table: a.name.clone(), view: qual(&spec.space, &name), masked: a.masked.clone() } });
        }
        access_ok.push(a);
    }
    if !views.is_empty() {
        schemas.insert(spec.space.clone());
    }
    for s in schemas {
        grants.push(Grant::Schema { schema: s, privilege: "USAGE".into() });
    }
    if spec.activity && probe.flavor.activity {
        grants.push(Grant::Member("pg_read_all_stats".into()));
        if spec.kill {
            grants.push(Grant::Member("pg_signal_backend".into()));
        }
    }
    grants.push(Grant::Setting { name: "statement_timeout".into(), value: format!("{}s", spec.time_limit_s) });
    grants.push(Grant::Setting { name: "idle_in_transaction_session_timeout".into(), value: "60s".into() });
    if !spec.write {
        grants.push(Grant::Setting { name: "default_transaction_read_only".into(), value: "on".into() });
    }
    if let Some(n) = spec.conn_limit {
        grants.push(Grant::Limit(n));
    }
    grants.sort();
    grants.dedup();
    Desired { grants, views, access: access_ok, hidden, not_granted, salt }
}

fn rules(spec: &Spec, d: &Desired, probe: &Probe, prod: bool) -> Vec<RuleLine> {
    let mut v = vec![line("A login apart from yours", "CREATE ROLE, its own password", Who::Database)];
    if !spec.rows {
        v.push(line("Structure only", "no table granted; plans of queries are refused too", Who::Database));
    } else if spec.write {
        v.push(line("Rows: read and write, no DDL", "only SELECT/INSERT/UPDATE/DELETE granted", Who::Database));
    } else {
        v.push(line("Rows: read only", "only SELECT granted", Who::Database));
    }
    if !d.hidden.is_empty() {
        v.push(line(hidden_label(&d.hidden), "no grant; Postgres still lists the names", Who::Partly));
    }
    v.extend(column_lines(&d.access, ("column grants (names still in pg_catalog)", Who::Database), (&format!("views in schema {}", spec.space), Who::Database), spec.rows));
    if probe.flavor.major > 0 && probe.flavor.major < 15 {
        v.push(line("No new tables", "PUBLIC may still create in schema public (before PostgreSQL 15)", Who::Partly));
    }
    if !spec.code {
        v.push(line("Run code off", "functions are executable by PUBLIC", Who::Dataman));
    }
    if spec.activity {
        let who = if probe.flavor.activity { Who::Database } else { Who::Dataman };
        v.push(line("Server activity", if probe.flavor.activity { "pg_read_all_stats" } else { "your login can't grant pg_read_all_stats" }, who));
    } else {
        v.push(line("No server activity", "pg_stat_activity hides others' statements", Who::Database));
    }
    v.push(line(time_label(spec.time_limit_s), "statement_timeout is only a default", Who::Partly));
    if spec.conn_limit.is_some() {
        v.push(line("Connections", "CONNECTION LIMIT", Who::Database));
    }
    v.extend(dataman_lines(spec, prod));
    v
}

fn space_steps(spec: &Spec, d: &Desired, salt_hex: &str, create_schema: bool) -> Vec<Step> {
    let mut steps = Vec::new();
    let g = format!("masked columns: views the agent reads instead ({})", d.views.len());
    if create_schema {
        steps.push(Step::sql(&g, format!("CREATE SCHEMA {}", q(&spec.space))));
        steps.push(Step::sql(&g, format!("COMMENT ON SCHEMA {} IS {}", q(&spec.space), lit(MARK))));
    }
    if d.salt {
        steps.push(Step::sql(&g, format!("CREATE TABLE IF NOT EXISTS {}.salt (v bytea NOT NULL)", q(&spec.space))));
        steps.push(Step::secret_sql(
            &g,
            format!("INSERT INTO {0}.salt SELECT decode('{salt_hex}', 'hex') WHERE NOT EXISTS (SELECT 1 FROM {0}.salt)", q(&spec.space)),
            format!("INSERT INTO {}.salt VALUES (<random salt>)", q(&spec.space)),
        ));
    }
    for v in &d.views {
        steps.extend(view_steps(spec, v, &g));
    }
    steps
}

fn view_steps(spec: &Spec, v: &ViewDef, group: &str) -> Vec<Step> {
    vec![Step::sql(group, v.create.clone()), Step::sql(group, format!("COMMENT ON VIEW {} IS {}", qual(&spec.space, &v.name), lit(&format!("{MARK} {}", v.hash))))]
}

fn grant_steps(spec: &Spec, d: &Desired, grants: &[Grant]) -> Vec<Step> {
    let mut steps = Vec::new();
    let tables = d.access.len();
    for g in grants {
        let group = match g {
            Grant::Database(_) | Grant::Schema { .. } => "where it may connect".to_string(),
            Grant::Table { schema, .. } if *schema == spec.space => "the views".into(),
            Grant::Table { .. } => format!("tables the agent may {} ({tables})", if spec.write { "read and write" } else { "read" }),
            Grant::Sequence { .. } => "the sequences its inserts take ids from".into(),
            Grant::Column { .. } => "tables with hidden or masked columns: only the others".into(),
            Grant::Member(_) => "server activity".into(),
            Grant::Setting { .. } | Grant::Limit(_) => format!("defaults (Dataman still enforces its own {})", time_label(spec.time_limit_s).trim_end_matches(" per call")),
            Grant::View { .. } | Grant::Global(_) => continue,
        };
        if let Some(sql) = grant_sql(g, &spec.user, &spec.database) {
            steps.push(Step::sql(&group, sql));
        }
    }
    steps
}

/// The order statements read best in: where it connects, the tables, the views, the rest.
fn order(g: &Grant) -> u8 {
    match g {
        Grant::Database(_) | Grant::Schema { .. } => 0,
        Grant::Table { .. } | Grant::Column { .. } => 1,
        Grant::Sequence { .. } => 3,
        Grant::Member(_) => 4,
        _ => 5,
    }
}

/// One GRANT per table (or per view, `views`), its privileges and columns together.
fn object_steps(spec: &Spec, d: &Desired, grants: &[Grant], views: bool) -> Vec<Step> {
    let tables = d.access.len();
    granted_objects(grants)
        .into_iter()
        .filter(|(s, _, _)| (*s == spec.space) == views)
        .map(|(s, n, cols)| {
            let group = match (views, cols) {
                (true, _) => "the views".to_string(),
                (false, false) => format!("tables the agent may {} ({tables})", if spec.write { "read and write" } else { "read" }),
                (false, true) => "tables with hidden or masked columns: only the others".to_string(),
            };
            Step::sql(&group, format!("GRANT {} ON {} TO {}", privilege_list(grants, &s, &n, &q), qual(&s, &n), q(&spec.user)))
        })
        .collect()
}

pub fn expiry(days: Option<u32>) -> Option<String> {
    days.map(|d| (Utc::now() + chrono::Duration::days(d as i64)).format("%Y-%m-%d").to_string())
}

/// Creating it: the role (marked), its grants, its views and defaults, in one transaction.
pub fn create_plan(spec: &Spec, inv: &Inventory, probe: &Probe, password: &str, prod: bool) -> Plan {
    let d = desired(spec, inv, probe);
    let verifier = hash::pg_scram(password);
    let mut role = format!("CREATE ROLE {} LOGIN PASSWORD {{}} NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS", q(&spec.user));
    if let Some(e) = expiry(spec.expires_days) {
        role.push_str(&format!(" VALID UNTIL '{e}'"));
    }
    let g = "the login (password sent as a SCRAM verifier, never in clear)";
    let mut steps = vec![
        Step::secret_sql(g, role.replace("{}", &lit(&verifier)), role.replace("{}", "'SCRAM-SHA-256$4096:…'")),
        Step::sql(g, format!("COMMENT ON ROLE {} IS {}", q(&spec.user), lit(&format!("{MARK} conn={}", spec.conn)))),
    ];
    let mut sorted = d.grants.clone();
    sorted.sort_by_key(|g| (order(g), g.clone()));
    let in_space = |g: &Grant| matches!(g, Grant::Table { schema, .. } | Grant::Schema { schema, .. } if *schema == spec.space);
    let first: Vec<Grant> = sorted.iter().filter(|g| matches!(g, Grant::Database(_) | Grant::Schema { .. }) && !in_space(g)).cloned().collect();
    steps.extend(grant_steps(spec, &d, &first));
    steps.extend(object_steps(spec, &d, &sorted, false));
    if !d.views.is_empty() {
        steps.extend(space_steps(spec, &d, &hash::hex(&hash::random_bytes(32)), true));
        let space: Vec<Grant> = sorted.iter().filter(|g| matches!(g, Grant::Schema { .. }) && in_space(g)).cloned().collect();
        steps.extend(grant_steps(spec, &d, &space));
        steps.extend(object_steps(spec, &d, &sorted, true));
    }
    let rest: Vec<Grant> = sorted.iter().filter(|g| order(g) >= 3).cloned().collect();
    steps.extend(grant_steps(spec, &d, &rest));
    if !d.hidden.is_empty() {
        steps.push(Step::note("", format!("hidden: {} → no grant at all", short_list(&d.hidden))));
    }
    let mut made = Vec::new();
    if !d.views.is_empty() {
        made.push(format!("schema:{}", spec.space));
    }
    seal(spec, d, probe, steps, made, prod)
}

fn seal(spec: &Spec, d: Desired, probe: &Probe, steps: Vec<Step>, made: Vec<String>, prod: bool) -> Plan {
    let rules = rules(spec, &d, probe, prod);
    let mut notes = Vec::new();
    if !d.views.is_empty() {
        notes.push(format!("While the views exist, dropping or retyping a masked column of {} fails until Dataman's view goes (Remove login, or a sync after the rules change).", short_list(&d.views.iter().map(|v| v.map.table.clone()).collect::<Vec<_>>())));
    }
    Plan {
        engine: Some(Engine::Postgres),
        user: spec.user.clone(),
        space: spec.space.clone(),
        steps,
        one_tx: true,
        rules,
        lines: d.grants.iter().map(text).collect(),
        not_granted: d.not_granted,
        lacks: probe.lacks.clone(),
        notes,
        made,
        granted: d.access.len(),
        views: d.views.into_iter().map(|v| v.map).collect(),
        hidden: d.hidden,
        manual: None,
        hash: String::new(),
    }
    .seal()
}

/// A sync: what the role has against what the rules want, granted and revoked, and the views
/// that changed made again. `have_views`: Dataman's views there now, by name and hash.
pub fn sync_plan(spec: &Spec, inv: &Inventory, probe: &Probe, have: &[Grant], have_views: &[(String, String)], schema_exists: bool, prod: bool) -> Plan {
    let d = desired(spec, inv, probe);
    let mut steps = Vec::new();
    let g_rev = "revoked: the rules no longer allow it";
    // Narrowed first: a table that becomes column-level loses its table grant before.
    for g in have.iter().filter(|g| !d.grants.contains(g)) {
        if let Some(sql) = revoke_sql(g, &spec.user, &spec.database) {
            steps.push(Step::sql(g_rev, sql));
        }
    }
    let gv = "views";
    for (name, hash) in have_views {
        if !d.views.iter().any(|v| v.name == *name && v.hash == *hash) {
            steps.push(Step::sql(gv, format!("DROP VIEW {}", qual(&spec.space, name))));
        }
    }
    if !d.views.is_empty() && !schema_exists {
        steps.push(Step::sql(gv, format!("CREATE SCHEMA {}", q(&spec.space))));
        steps.push(Step::sql(gv, format!("COMMENT ON SCHEMA {} IS {}", q(&spec.space), lit(MARK))));
    }
    if d.salt {
        steps.push(Step::sql(gv, format!("CREATE TABLE IF NOT EXISTS {}.salt (v bytea NOT NULL)", q(&spec.space))));
        let salt = hash::hex(&hash::random_bytes(32));
        steps.push(Step::secret_sql(
            gv,
            format!("INSERT INTO {0}.salt SELECT decode('{salt}', 'hex') WHERE NOT EXISTS (SELECT 1 FROM {0}.salt)", q(&spec.space)),
            format!("INSERT INTO {}.salt VALUES (<random salt>) -- if empty", q(&spec.space)),
        ));
    }
    for v in d.views.iter().filter(|v| !have_views.iter().any(|(n, h)| *n == v.name && *h == v.hash)) {
        steps.extend(view_steps(spec, v, gv));
    }
    // A view made again lost its grant: granted again with the rest.
    let remade: Vec<&String> = d.views.iter().filter(|v| !have_views.iter().any(|(n, h)| *n == v.name && *h == v.hash)).map(|v| &v.name).collect();
    // Revoking a table's privilege revokes it on each of its columns too: those wanted are
    // granted again.
    let cascaded = |schema: &String, name: &String, privilege: &String| {
        have.iter().any(|h| !d.grants.contains(h) && matches!(h, Grant::Table { schema: s, name: n, privilege: p } if s == schema && n == name && p == privilege))
    };
    let g_add = "granted: the rules allow it now";
    for g in d.grants.iter().filter(|g| {
        !have.contains(g)
            || matches!(g, Grant::Table { schema, name, .. } if *schema == spec.space && remade.contains(&name))
            || matches!(g, Grant::Column { schema, name, privilege, .. } if cascaded(schema, name, privilege))
    }) {
        if let Some(sql) = grant_sql(g, &spec.user, &spec.database) {
            steps.push(Step::sql(g_add, sql));
        }
    }
    let made = if d.views.is_empty() { Vec::new() } else { vec![format!("schema:{}", spec.space)] };
    seal(spec, d, probe, steps, made, prod)
}

/// Removing it: Dataman's views, its schema (only if nothing else is in it), every privilege
/// the role holds in this database, then the role. `superuser`: DROP OWNED also takes what
/// others granted it (it needs the role's privileges, which a CREATEROLE login lacks from 16).
pub fn remove_plan(user: &str, space: &str, database: &str, have: &[Grant], views: &[String], salt: bool, made_schema: bool, superuser: bool) -> Plan {
    let mut steps = Vec::new();
    let g = "Dataman's views";
    for v in views {
        steps.push(Step::sql(g, format!("DROP VIEW IF EXISTS {}", qual(space, v))));
    }
    if salt {
        steps.push(Step::sql(g, format!("DROP TABLE IF EXISTS {}.salt", q(space))));
    }
    let g = "the login: its privileges in this database, then the role";
    if superuser {
        steps.push(Step::sql(g, format!("DROP OWNED BY {}", q(user))));
    } else {
        for x in have.iter().filter(|x| !matches!(x, Grant::View { .. } | Grant::Limit(_) | Grant::Setting { .. })) {
            if let Grant::Table { schema, .. } | Grant::Schema { schema, .. } = x
                && schema == space
            {
                continue;
            }
            if let Some(sql) = revoke_sql(x, user, database) {
                steps.push(Step::sql(g, sql));
            }
        }
    }
    if made_schema {
        steps.push(Step::sql("Dataman's views", format!("DROP SCHEMA IF EXISTS {}", q(space))));
    }
    steps.push(Step::sql(g, format!("DROP ROLE {}", q(user))));
    steps.sort_by_key(|s| !s.group.starts_with("Dataman"));
    Plan { engine: Some(Engine::Postgres), user: user.into(), space: space.into(), steps, one_tx: true, ..Default::default() }
}

pub fn rotate_step(user: &str, password: &str, expires_days: Option<u32>) -> Step {
    let mut sql = format!("ALTER ROLE {} PASSWORD {{}}", q(user));
    if let Some(e) = expiry(expires_days) {
        sql.push_str(&format!(" VALID UNTIL '{e}'"));
    }
    Step::secret_sql("a new password", sql.replace("{}", &lit(&hash::pg_scram(password))), sql.replace("{}", "'SCRAM-SHA-256$4096:…'"))
}

// ---- Live ----

async fn rows(db: &Db, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    text_rows(db, sql).await
}

fn s(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

fn yes(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("t" | "true" | "on" | "1"))
}

pub(crate) async fn probe(db: &Db, user: &str, conn: &str) -> Result<Probe> {
    let r = rows(
        db,
        "SELECT current_user, r.rolsuper, r.rolcreaterole, current_setting('server_version_num'), pg_is_in_recovery(), \
         COALESCE((SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()), false), current_setting('server_version'), \
         EXISTS (SELECT 1 FROM pg_roles c WHERE c.rolname IN ('rds_superuser', 'cloudsqlsuperuser', 'azure_pg_admin') AND pg_has_role(current_user, c.oid, 'MEMBER')) \
         FROM pg_roles r WHERE r.rolname = current_user",
    )
    .await?;
    let row = r.first().ok_or_else(|| anyhow!("the server did not say who this login is"))?;
    let (login, sup, createrole) = (s(&row[0]), yes(&row[1]), yes(&row[2]));
    let major = s(&row[3]).parse::<u32>().unwrap_or(0) / 10000;
    let standby = yes(&row[4]);
    let managed_admin = yes(&row[7]);
    let admin_stats = rows(
        db,
        "SELECT EXISTS (SELECT 1 FROM pg_auth_members m JOIN pg_roles r ON r.oid = m.roleid WHERE r.rolname = 'pg_read_all_stats' AND m.member = (SELECT oid FROM pg_roles WHERE rolname = current_user) AND m.admin_option)",
    )
    .await
    .ok()
    .and_then(|r| r.first().map(|x| yes(&x[0])))
    .unwrap_or(false);
    let mut p = Probe { login: login.clone(), version: format!("PostgreSQL {}", s(&row[6])), tls: yes(&row[5]), ..Default::default() };
    p.flavor = Flavor { major, superuser: sup, activity: sup || admin_stats || (major < 16 && createrole), ..Default::default() };
    p.can_create = (sup || createrole) && !standby;
    if standby {
        p.lacks.push("this server is a standby (read-only): create the login on the primary".into());
    } else if !(sup || createrole) {
        p.lacks.push(format!("{login} can't create roles: it needs CREATEROLE (or a superuser)"));
    }
    p.checks.push((p.can_create, if p.can_create { format!("{login} can create roles") } else { format!("{login} can't create roles") }));
    p.checks.push((p.tls, if p.tls { "TLS on".into() } else { "TLS off".into() }));
    if managed_admin && !sup {
        p.notes.push("A managed PostgreSQL's admin role: it grants on the tables it owns or is a member of their owner.".into());
    }
    let e = rows(db, &format!("SELECT shobj_description(oid, 'pg_authid') FROM pg_roles WHERE rolname = {}", lit(user))).await?;
    p.exists = e.first().map(|r| r[0].as_deref().is_some_and(|c| c.starts_with(MARK) && c.contains(&format!("conn={conn}"))));
    Ok(p)
}

/// Tables, views and their columns in `database` (this connection's), as the user's login can
/// grant them.
pub(crate) async fn inventory(db: &Db) -> Result<Inventory> {
    let objs = rows(
        db,
        "SELECT n.nspname, c.relname, c.relkind::text, pg_get_userbyid(c.relowner), \
         (pg_has_role(current_user, c.relowner, 'USAGE') OR (SELECT rolsuper FROM pg_roles WHERE rolname = current_user) \
          OR (has_table_privilege(c.oid, 'SELECT WITH GRANT OPTION') AND (c.relkind IN ('v','m','f') OR (has_table_privilege(c.oid, 'INSERT WITH GRANT OPTION') AND has_table_privilege(c.oid, 'UPDATE WITH GRANT OPTION') AND has_table_privilege(c.oid, 'DELETE WITH GRANT OPTION'))))), \
         (SELECT string_agg(a.attname || CASE WHEN a.attgenerated <> '' THEN '\u{1}g' ELSE '' END, '\u{2}' ORDER BY a.attnum) FROM pg_attribute a WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped), \
         (SELECT string_agg(s.relname, '\u{2}') FROM pg_depend d JOIN pg_class s ON s.oid = d.objid AND s.relkind = 'S' WHERE d.refobjid = c.oid AND d.deptype IN ('a', 'i')) \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND NOT c.relispartition \
         AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp%' \
         ORDER BY 1, 2",
    )
    .await?;
    let mut out = Inventory::default();
    for r in objs {
        let columns = s(&r[5])
            .split('\u{2}')
            .filter(|c| !c.is_empty())
            .map(|c| match c.strip_suffix("\u{1}g") {
                Some(n) => Col { name: n.into(), generated: true },
                None => Col::new(c),
            })
            .collect();
        let kind = if matches!(s(&r[2]).as_str(), "v" | "m") { ObjKind::View } else { ObjKind::Table };
        out.objects.push(Obj {
            schema: s(&r[0]),
            name: s(&r[1]),
            kind,
            columns,
            grantable: yes(&r[4]),
            owner: s(&r[3]),
            sequences: s(&r[6]).split('\u{2}').filter(|x| !x.is_empty()).map(str::to_string).collect(),
        });
    }
    Ok(out)
}

/// What the role holds now, in the same lines as [`desired`].
pub(crate) async fn have(db: &Db, user: &str, database: &str, space: &str) -> Result<(Vec<Grant>, Vec<(String, String)>, bool)> {
    let u = lit(user);
    let mut out = Vec::new();
    for r in rows(db, &format!("SELECT a.privilege_type FROM pg_database d, aclexplode(d.datacl) a WHERE d.datname = {} AND a.grantee = (SELECT oid FROM pg_roles WHERE rolname = {u})", lit(database))).await? {
        out.push(Grant::Database(format!("{} ON DATABASE {}", s(&r[0]), q(database))));
    }
    for r in rows(db, &format!("SELECT n.nspname, a.privilege_type FROM pg_namespace n, aclexplode(n.nspacl) a WHERE a.grantee = (SELECT oid FROM pg_roles WHERE rolname = {u})")).await? {
        out.push(Grant::Schema { schema: s(&r[0]), privilege: s(&r[1]) });
    }
    for r in rows(db, &format!("SELECT n.nspname, c.relname, c.relkind::text, a.privilege_type FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace, aclexplode(c.relacl) a WHERE a.grantee = (SELECT oid FROM pg_roles WHERE rolname = {u})")).await? {
        if s(&r[2]) == "S" {
            if s(&r[3]) == "USAGE" {
                out.push(Grant::Sequence { schema: s(&r[0]), name: s(&r[1]) });
            }
        } else {
            out.push(Grant::Table { schema: s(&r[0]), name: s(&r[1]), privilege: s(&r[3]) });
        }
    }
    for r in rows(db, &format!("SELECT n.nspname, c.relname, t.attname, a.privilege_type FROM pg_attribute t JOIN pg_class c ON c.oid = t.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace, aclexplode(t.attacl) a WHERE a.grantee = (SELECT oid FROM pg_roles WHERE rolname = {u})")).await? {
        out.push(Grant::Column { schema: s(&r[0]), name: s(&r[1]), column: s(&r[2]), privilege: s(&r[3]) });
    }
    for r in rows(db, &format!("SELECT r.rolname FROM pg_auth_members m JOIN pg_roles r ON r.oid = m.roleid WHERE m.member = (SELECT oid FROM pg_roles WHERE rolname = {u})")).await? {
        out.push(Grant::Member(s(&r[0])));
    }
    for r in rows(db, &format!("SELECT unnest(setconfig) FROM pg_db_role_setting WHERE setrole = (SELECT oid FROM pg_roles WHERE rolname = {u}) AND setdatabase = (SELECT oid FROM pg_database WHERE datname = {})", lit(database))).await? {
        if let Some((k, v)) = s(&r[0]).split_once('=') {
            out.push(Grant::Setting { name: k.into(), value: v.into() });
        }
    }
    for r in rows(db, &format!("SELECT rolconnlimit FROM pg_roles WHERE rolname = {u}")).await? {
        if let Ok(n) = s(&r[0]).parse::<i64>()
            && n >= 0
        {
            out.push(Grant::Limit(n as u32));
        }
    }
    let mut views = Vec::new();
    for r in rows(db, &format!("SELECT c.relname, obj_description(c.oid, 'pg_class') FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = {} AND c.relkind = 'v'", lit(space))).await? {
        let comment = s(&r[1]);
        if let Some(h) = comment.strip_prefix(MARK).map(str::trim) {
            views.push((s(&r[0]), h.to_string()));
            out.push(Grant::View { name: s(&r[0]), hash: h.to_string() });
        }
    }
    let schema = rows(db, &format!("SELECT obj_description(oid, 'pg_namespace') FROM pg_namespace WHERE nspname = {}", lit(space))).await?;
    let schema_exists = !schema.is_empty();
    if let Some(r) = schema.first()
        && !s(&r[0]).starts_with(MARK)
    {
        return Err(anyhow!("a schema named {space} exists and is not Dataman's: Dataman leaves it alone; rename it to create the agent login"));
    }
    out.sort();
    out.dedup();
    Ok((out, views, schema_exists))
}

/// Before Remove: Dataman's views and salt in its schema, and whether anything else is there.
pub(crate) async fn made_here(db: &Db, space: &str) -> Result<(Vec<String>, bool, bool, bool)> {
    let schema = rows(db, &format!("SELECT obj_description(oid, 'pg_namespace') FROM pg_namespace WHERE nspname = {}", lit(space))).await?;
    let Some(r) = schema.first() else { return Ok((Vec::new(), false, false, false)) };
    let ours = s(&r[0]).starts_with(MARK);
    let objs = rows(db, &format!("SELECT c.relname, c.relkind::text, COALESCE(obj_description(c.oid, 'pg_class'), '') FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = {} AND c.relkind IN ('r','v','m','S','f','p')", lit(space))).await?;
    let mut views = Vec::new();
    let mut salt = false;
    let mut other = false;
    for o in objs {
        match (s(&o[1]).as_str(), s(&o[0]).as_str()) {
            ("v", name) if s(&o[2]).starts_with(MARK) => views.push(name.to_string()),
            ("r", "salt") if ours => salt = true,
            _ => other = true,
        }
    }
    Ok((views, salt, ours && !other, ours))
}

/// Whether the role named `user` is there, and marked for this connection.
pub(crate) async fn exists(db: &Db, user: &str, conn: &str) -> Result<Option<bool>> {
    let e = rows(db, &format!("SELECT shobj_description(oid, 'pg_authid') FROM pg_roles WHERE rolname = {}", lit(user))).await?;
    Ok(e.first().map(|r| r[0].as_deref().is_some_and(|c| c.starts_with(MARK) && c.contains(&format!("conn={conn}")))))
}
