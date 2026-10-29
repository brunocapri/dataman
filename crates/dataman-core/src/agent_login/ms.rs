//! SQL Server: a login and its user in the connection's database (on Azure SQL Database a
//! contained user), granted per table; a table with hidden or masked columns is read through a
//! view in a schema of its own, owned by dbo so the ownership chain reaches the table (column
//! grants would show every column's name). In one transaction.
//!
//! SQL Server hides what a principal has no permission on from its catalog. It has no time
//! limit nor connection cap per login (Resource Governor aside), so those stay Dataman's. EXECUTE
//! is never granted: a procedure the user allows runs on the user's login.

use anyhow::{Result, anyhow};

use super::*;
use crate::backend::{Db, text_rows};

pub fn q(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

pub fn lit(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

fn qual(schema: &str, name: &str) -> String {
    format!("{}.{}", q(schema), q(name))
}

pub fn text(g: &Grant) -> String {
    match g {
        Grant::Table { schema, name, privilege } => format!("{privilege} ON OBJECT::{}", qual(schema, name)),
        Grant::Column { schema, name, column, privilege } => format!("{privilege} ({}) ON OBJECT::{}", q(column), qual(schema, name)),
        Grant::Member(r) => format!("MEMBER OF {}", q(r)),
        Grant::Global(p) => p.clone(),
        Grant::Database(p) => p.clone(),
        Grant::View { name, hash } => format!("VIEW {} -- {hash}", q(name)),
        other => format!("{other:?}"),
    }
}

fn grant_sql(g: &Grant, user: &str) -> Option<String> {
    let u = q(user);
    Some(match g {
        Grant::Member(r) => format!("ALTER ROLE {} ADD MEMBER {u}", q(r)),
        // Server permissions are granted in master.
        Grant::Global(p) => format!("EXEC master.sys.sp_executesql N'GRANT {p} TO {}'", u.replace('\'', "''")),
        Grant::View { .. } | Grant::Schema { .. } | Grant::Sequence { .. } | Grant::Setting { .. } | Grant::Limit(_) => return None,
        g => format!("GRANT {} TO {u}", text(g)),
    })
}

fn revoke_sql(g: &Grant, user: &str) -> Option<String> {
    let u = q(user);
    Some(match g {
        Grant::Member(r) => format!("ALTER ROLE {} DROP MEMBER {u}", q(r)),
        Grant::Global(p) => format!("EXEC master.sys.sp_executesql N'REVOKE {p} FROM {}'", u.replace('\'', "''")),
        Grant::View { .. } | Grant::Schema { .. } | Grant::Sequence { .. } | Grant::Setting { .. } | Grant::Limit(_) => return None,
        g => format!("REVOKE {} FROM {u}", text(g)),
    })
}

pub fn mask_expr(col: &str, m: Mask, space: &str) -> String {
    let c = format!("CAST({} AS NVARCHAR(4000))", q(col));
    match m {
        Mask::Hidden => "N'‹hidden›'".into(),
        Mask::Partial => format!(
            "CASE WHEN {c} IS NULL THEN NULL WHEN CHARINDEX(N'@', {c}) > 0 THEN LEFT({c}, 1) + N'•••@' + SUBSTRING({c}, CHARINDEX(N'@', {c}) + 1, 4000) WHEN LEN({c}) >= 8 THEN N'•••• ' + RIGHT({c}, 4) ELSE N'••••' END"
        ),
        Mask::Hash => format!(
            "CASE WHEN {c} IS NULL THEN NULL ELSE N'‹h:' + LOWER(LEFT(CONVERT(VARCHAR(64), HASHBYTES('SHA2_256', CONCAT({c}, (SELECT v FROM {}.[salt]))), 2), 12)) + N'›' END",
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
    pub exposed: Vec<NotGranted>,
}

/// What closes a table (or columns) to every user of the database.
fn revoke_public(o: &Open, cols: &[String]) -> String {
    if cols.is_empty() {
        format!("REVOKE SELECT ON OBJECT::{} FROM [public];", qual(&o.schema, &o.name))
    } else {
        format!("REVOKE SELECT ({}) ON OBJECT::{} FROM [public];", cols.iter().map(|c| q(c)).collect::<Vec<_>>().join(", "), qual(&o.schema, &o.name))
    }
}

pub const DEFAULT_SCHEMA: &str = "dbo";

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
                reason: format!("{} can't grant on it", probe.login),
                sql: vec![format!("GRANT {} ON OBJECT::{} TO {};", verbs.join(", "), qual(&a.schema, &a.name), q(&spec.user))],
            });
            continue;
        }
        if a.whole {
            for v in verbs {
                grants.push(Grant::Table { schema: a.schema.clone(), name: a.name.clone(), privilege: v.to_string() });
            }
            ok.push(a);
            continue;
        }
        // The view's ownership chain reaches the table only when dbo owns both.
        if !a.owner.is_empty() && !a.owner.eq_ignore_ascii_case("dbo") {
            not_granted.push(NotGranted {
                object: a.label(),
                reason: format!("owned by {}: a view owned by dbo can't reach it (hidden or masked columns)", a.owner),
                sql: Vec::new(),
            });
            continue;
        }
        salt |= a.masked.iter().any(|(_, m)| *m == Mask::Hash);
        let name = view_name(&a.schema, &a.name, DEFAULT_SCHEMA, 128, &views);
        let cols: Vec<String> = a.plain.iter().map(|c| q(c)).chain(a.masked.iter().map(|(c, m)| format!("{} AS {}", mask_expr(c, *m, &spec.space), q(c)))).collect();
        let body = format!("SELECT {} FROM {}", cols.join(", "), qual(&a.schema, &a.name));
        let hash = hash::sha256_hex(body.as_bytes())[..12].to_string();
        let create = format!("CREATE VIEW {} AS {body}", qual(&spec.space, &name));
        grants.push(Grant::Table { schema: spec.space.clone(), name: name.clone(), privilege: "SELECT".into() });
        grants.push(Grant::View { name: name.clone(), hash: hash.clone() });
        if a.write {
            // Writes go to the table, on the visible columns that are not masked. SQL Server has
            // no INSERT per column: no INSERT at all here (fail closed), UPDATE and DELETE only.
            for p in ["UPDATE", "SELECT"] {
                for c in &a.plain {
                    grants.push(Grant::Column { schema: a.schema.clone(), name: a.name.clone(), column: c.clone(), privilege: p.into() });
                }
            }
            grants.push(Grant::Table { schema: a.schema.clone(), name: a.name.clone(), privilege: "DELETE".into() });
        }
        views.push(ViewDef { name: name.clone(), create, hash, map: ViewMap { schema: a.schema.clone(), table: a.name.clone(), view: qual(&spec.space, &name), masked: a.masked.clone() } });
        ok.push(a);
    }
    if !spec.write {
        grants.push(Grant::Member("db_denydatawriter".into()));
    }
    if spec.activity {
        grants.push(if probe.flavor.azure { Grant::Database("VIEW DATABASE STATE".into()) } else { Grant::Global("VIEW SERVER STATE".into()) });
    }
    grants.sort();
    grants.dedup();
    let exposed = exposed(spec, inv, &revoke_public);
    Desired { grants, views, access: ok, hidden, not_granted, salt, exposed }
}

fn rules(spec: &Spec, d: &Desired, probe: &Probe, prod: bool) -> Vec<RuleLine> {
    let who = if probe.flavor.azure { "CREATE USER WITH PASSWORD (contained)" } else { "CREATE LOGIN and USER, its own password" };
    let mut v = vec![line("A login apart from yours", who, Who::Database)];
    if !spec.rows {
        v.push(line("Structure only", "no table granted (SQL Server then hides them too)", Who::Database));
    } else if spec.write {
        v.push(line("Rows: read and write, no DDL", "only SELECT/INSERT/UPDATE/DELETE granted", Who::Database));
    } else {
        v.push(line("Rows: read only", "only SELECT granted, and db_denydatawriter", Who::Database));
    }
    if !d.hidden.is_empty() {
        v.push(line(hidden_label(&d.hidden), "no grant; SQL Server hides the names too", Who::Database));
    }
    let writes_cols = spec.write && d.access.iter().any(|a| !a.whole && a.write);
    if writes_cols {
        v.push(line("No INSERT into tables with hidden or masked columns", "SQL Server has no INSERT per column", Who::Database));
    }
    v.extend(column_lines(
        &d.access,
        (if writes_cols { "views; writes need column grants, which show the names" } else { "views in their own schema" }, if writes_cols { Who::Partly } else { Who::Database }),
        (&format!("views in schema {}", spec.space), Who::Database),
        spec.rows,
    ));
    v.push(line(if spec.code { "Procedures after your Allow" } else { "Run code off" }, "EXECUTE is never granted", Who::Database));
    v.push(line(if spec.activity { "Server activity" } else { "No server activity" }, if spec.activity { "VIEW SERVER STATE" } else { "no VIEW SERVER STATE" }, Who::Database));
    v.push(line(time_label(spec.time_limit_s), "no time limit per login (Resource Governor only)", Who::Dataman));
    if spec.conn_limit.is_some() {
        v.push(line("Connections", "no cap per login", Who::Dataman));
    }
    v.extend(dataman_lines(spec, prod));
    v
}

fn group_of(g: &Grant, spec: &Spec, d: &Desired) -> String {
    match g {
        Grant::Table { schema, .. } if *schema == spec.space => "the views".into(),
        Grant::Table { .. } => format!("tables the agent may use ({})", d.access.len()),
        Grant::Column { .. } => "writes on tables read through a view: only the visible columns".into(),
        Grant::Member(_) => "read only: writes denied".into(),
        _ => "server activity".into(),
    }
}

fn view_steps(spec: &Spec, d: &Desired, fresh: &[&ViewDef], salt_hex: &str, create_schema: bool) -> Vec<Step> {
    let g = format!("hidden or masked columns: views the agent reads instead ({})", d.views.len());
    let mut steps = Vec::new();
    if create_schema {
        steps.push(Step::sql(&g, format!("CREATE SCHEMA {} AUTHORIZATION [dbo]", q(&spec.space))));
        steps.push(Step::sql(&g, format!("EXEC sys.sp_addextendedproperty @name = N'dataman', @value = {}, @level0type = N'SCHEMA', @level0name = {}", lit(MARK), lit(&spec.space))));
    }
    if d.salt {
        steps.push(Step::sql(&g, format!("IF OBJECT_ID({}) IS NULL CREATE TABLE {}.[salt] (v NVARCHAR(128) NOT NULL)", lit(&format!("{}.[salt]", q(&spec.space))), q(&spec.space))));
        steps.push(Step::secret_sql(
            &g,
            format!("IF NOT EXISTS (SELECT 1 FROM {0}.[salt]) INSERT INTO {0}.[salt] VALUES (N'{salt_hex}')", q(&spec.space)),
            format!("INSERT INTO {}.[salt] VALUES (<random salt>)", q(&spec.space)),
        ));
    }
    for v in fresh {
        steps.push(Step::sql(&g, v.create.clone()));
        steps.push(Step::sql(&g, format!("EXEC sys.sp_addextendedproperty @name = N'dataman', @value = {}, @level0type = N'SCHEMA', @level0name = {}, @level1type = N'VIEW', @level1name = {}", lit(&format!("{MARK} {}", v.hash)), lit(&spec.space), lit(&v.name))));
    }
    steps
}

pub fn create_plan(spec: &Spec, inv: &Inventory, probe: &Probe, password: &str, prod: bool) -> Plan {
    let d = desired(spec, inv, probe);
    let g = "the login (password sent as its hash, never in clear)";
    let u = q(&spec.user);
    let mut steps = Vec::new();
    if probe.flavor.azure {
        let sql = format!("CREATE USER {u} WITH PASSWORD = {{}}, DEFAULT_SCHEMA = [dbo]");
        steps.push(Step::secret_sql("the login (a contained user: Azure SQL Database takes its password in clear)", sql.replace("{}", &lit(password)), sql.replace("{}", "N'<password from the Keychain>'")));
    } else {
        let sql = format!("CREATE LOGIN {u} WITH PASSWORD = {{}} HASHED, CHECK_POLICY = OFF, CHECK_EXPIRATION = OFF, DEFAULT_DATABASE = {}", q(&spec.database));
        steps.push(Step::secret_sql(g, sql.replace("{}", &hash::mssql_hash(password)), sql.replace("{}", "0x0200…")));
        steps.push(Step::sql(g, format!("ALTER LOGIN {u} DISABLE")));
        steps.push(Step::sql(g, format!("CREATE USER {u} FOR LOGIN {u} WITH DEFAULT_SCHEMA = [dbo]")));
    }
    steps.push(Step::sql(g, format!("EXEC sys.sp_addextendedproperty @name = N'dataman', @value = {}, @level0type = N'USER', @level0name = {}", lit(&mark_for(&spec.conn)), lit(&spec.user))));
    let fresh: Vec<&ViewDef> = d.views.iter().collect();
    if !fresh.is_empty() {
        steps.extend(view_steps(spec, &d, &fresh, &hash::hex(&hash::random_bytes(32)), true));
    }
    // SQL Server takes column lists and whole-table permissions in separate GRANTs.
    let (cols, whole): (Vec<Grant>, Vec<Grant>) = d.grants.iter().filter(|g| matches!(g, Grant::Table { .. } | Grant::Column { .. })).cloned().partition(|g| matches!(g, Grant::Column { .. }));
    for (s, n, has_cols) in granted_objects(&d.grants) {
        let group = if s == spec.space { "the views".to_string() } else if has_cols { "writes on tables read through a view: only the visible columns".to_string() } else { format!("tables the agent may use ({})", d.access.len()) };
        for list in [&cols, &whole] {
            let privs = privilege_list(list, &s, &n, &q);
            if !privs.is_empty() {
                steps.push(Step::sql(&group, format!("GRANT {privs} ON OBJECT::{} TO {}", qual(&s, &n), q(&spec.user))));
            }
        }
    }
    for gr in d.grants.iter().filter(|g| !matches!(g, Grant::Table { .. } | Grant::Column { .. })) {
        if let Some(sql) = grant_sql(gr, &spec.user) {
            steps.push(Step::sql(&group_of(gr, spec, &d), sql));
        }
    }
    if !d.hidden.is_empty() {
        steps.push(Step::note("", format!("hidden: {} → no grant at all", short_list(&d.hidden))));
    }
    if !probe.flavor.azure {
        steps.push(Step::sql("ready: enabled last", format!("ALTER LOGIN {u} ENABLE")));
    }
    let made = if d.views.is_empty() { Vec::new() } else { vec![format!("schema:{}", spec.space)] };
    seal(spec, d, probe, steps, made, prod)
}

fn seal(spec: &Spec, d: Desired, probe: &Probe, steps: Vec<Step>, made: Vec<String>, prod: bool) -> Plan {
    let rules = rules(spec, &d, probe, prod);
    let exposed = d.exposed;
    let plan = Plan {
        engine: Some(Engine::SqlServer),
        user: spec.user.clone(),
        space: spec.space.clone(),
        steps,
        one_tx: true,
        rules,
        lines: d.grants.iter().map(text).collect(),
        not_granted: d.not_granted,
        exposed: Vec::new(),
        lacks: probe.lacks.clone(),
        notes: Vec::new(),
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

pub fn sync_plan(spec: &Spec, inv: &Inventory, probe: &Probe, have: &[Grant], have_views: &[(String, String)], schema_exists: bool, prod: bool) -> Plan {
    let d = desired(spec, inv, probe);
    let mut steps = Vec::new();
    for g in have.iter().filter(|g| !d.grants.contains(g)) {
        if let Some(sql) = revoke_sql(g, &spec.user) {
            steps.push(Step::sql("revoked: the rules no longer allow it", sql));
        }
    }
    for (name, hash) in have_views {
        if !d.views.iter().any(|v| v.name == *name && v.hash == *hash) {
            steps.push(Step::sql("views", format!("DROP VIEW {}", qual(&spec.space, name))));
        }
    }
    let fresh: Vec<&ViewDef> = d.views.iter().filter(|v| !have_views.iter().any(|(n, h)| *n == v.name && *h == v.hash)).collect();
    if !fresh.is_empty() {
        steps.extend(view_steps(spec, &d, &fresh, &hash::hex(&hash::random_bytes(32)), !schema_exists));
    }
    let remade: Vec<&String> = fresh.iter().map(|v| &v.name).collect();
    for g in d.grants.iter().filter(|g| !have.contains(g) || matches!(g, Grant::Table { schema, name, .. } if *schema == spec.space && remade.contains(&name))) {
        if let Some(sql) = grant_sql(g, &spec.user) {
            steps.push(Step::sql("granted: the rules allow it now", sql));
        }
    }
    let made = if d.views.is_empty() && !schema_exists { Vec::new() } else { vec![format!("schema:{}", spec.space)] };
    seal(spec, d, probe, steps, made, prod)
}

/// Removing it: Dataman's views and schema (only if nothing else is in it), the user, then the
/// login (its sessions ended first: a login in use can't be dropped).
pub fn remove_plan(user: &str, space: &str, views: &[String], salt: bool, drop_schema: bool, azure: bool) -> Plan {
    let mut steps = Vec::new();
    let g = "Dataman's views";
    for v in views {
        steps.push(Step::sql(g, format!("DROP VIEW IF EXISTS {}", qual(space, v))));
    }
    if salt {
        steps.push(Step::sql(g, format!("DROP TABLE IF EXISTS {}.[salt]", q(space))));
    }
    if drop_schema {
        steps.push(Step::sql(g, format!("DROP SCHEMA IF EXISTS {}", q(space))));
    }
    let u = q(user);
    steps.push(Step::sql("the user, and every permission it holds here", format!("DROP USER IF EXISTS {u}")));
    if !azure {
        let g = "the login: its sessions ended, then dropped";
        steps.push(Step::sql(g, format!("ALTER LOGIN {u} DISABLE")));
        steps.push(Step::sql(
            g,
            format!("DECLARE @k NVARCHAR(MAX) = N''; SELECT @k += N'KILL ' + CAST(session_id AS NVARCHAR(10)) + N';' FROM sys.dm_exec_sessions WHERE login_name = {} AND session_id <> @@SPID; EXEC (@k)", lit(user)),
        ));
        steps.push(Step::sql(g, format!("DROP LOGIN {u}")));
    }
    Plan { engine: Some(Engine::SqlServer), user: user.into(), space: space.into(), steps, ..Default::default() }
}

pub fn rotate_step(user: &str, password: &str, azure: bool) -> Step {
    if azure {
        let sql = format!("ALTER USER {} WITH PASSWORD = {{}}", q(user));
        return Step::secret_sql("a new password", sql.replace("{}", &lit(password)), sql.replace("{}", "N'<password from the Keychain>'"));
    }
    let sql = format!("ALTER LOGIN {} WITH PASSWORD = {{}} HASHED", q(user));
    Step::secret_sql("a new password", sql.replace("{}", &hash::mssql_hash(password)), sql.replace("{}", "0x0200…"))
}

// ---- Live ----

fn s(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

fn one(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("1" | "true" | "TRUE" | "True"))
}

pub(crate) async fn probe(db: &Db, user: &str, conn: &str) -> Result<Probe> {
    let r = text_rows(
        db,
        "SELECT SUSER_SNAME(), CAST(SERVERPROPERTY('ProductVersion') AS NVARCHAR(64)), CAST(SERVERPROPERTY('EngineEdition') AS INT), \
         IS_SRVROLEMEMBER('sysadmin'), IS_SRVROLEMEMBER('securityadmin'), IS_MEMBER('db_owner'), \
         HAS_PERMS_BY_NAME(NULL, NULL, 'ALTER ANY LOGIN'), HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'ALTER ANY USER'), \
         (SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id = @@SPID), CAST(SERVERPROPERTY('IsExternalAuthenticationOnly') AS INT)",
    )
    .await?;
    let row = r.first().ok_or_else(|| anyhow!("the server did not say who this login is"))?;
    let login = s(&row[0]);
    let azure = s(&row[2]) == "5";
    let (sysadmin, secadmin, dbo) = (one(&row[3]), one(&row[4]), one(&row[5]));
    let (any_login, any_user) = (one(&row[6]) || sysadmin || secadmin, one(&row[7]) || dbo || sysadmin);
    let tls = s(&row[8]).eq_ignore_ascii_case("TRUE");
    let entra_only = one(&row[9]);
    let mut p = Probe { login: login.clone(), version: format!("SQL Server {}", s(&row[1])), tls, ..Default::default() };
    p.flavor = Flavor { major: s(&row[1]).split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0), azure, superuser: sysadmin || dbo, activity: true, ..Default::default() };
    p.can_create = any_user && (azure || any_login) && !entra_only;
    if entra_only {
        p.lacks.push("this server takes Microsoft Entra logins only: a SQL password can't be created".into());
    } else {
        if !any_user {
            p.lacks.push(format!("{login} can't create users in this database: it needs ALTER ANY USER (db_owner)"));
        }
        if !azure && !any_login {
            p.lacks.push(format!("{login} can't create logins: it needs ALTER ANY LOGIN (securityadmin)"));
        }
    }
    p.checks.push((p.can_create, if p.can_create { format!("{login} can create logins and users") } else { format!("{login} can't create logins") }));
    p.checks.push((tls, if tls { "TLS on".into() } else { "TLS off".into() }));
    if azure {
        p.notes.push("Azure SQL Database: the login is a user contained in this database.".into());
    }
    p.exists = exists(db, user, conn).await?;
    Ok(p)
}

pub(crate) async fn exists(db: &Db, user: &str, conn: &str) -> Result<Option<bool>> {
    let r = text_rows(
        db,
        &format!(
            "SELECT CAST(ep.value AS NVARCHAR(400)), (SELECT COUNT(*) FROM sys.server_principals WHERE name = {0}) FROM (SELECT 1 AS x) d \
             LEFT JOIN sys.database_principals u ON u.name = {0} LEFT JOIN sys.extended_properties ep ON ep.class = 4 AND ep.major_id = u.principal_id AND ep.name = N'dataman'",
            lit(user)
        ),
    )
    .await?;
    let Some(row) = r.first() else { return Ok(None) };
    let mark = s(&row[0]);
    let user_there = !text_rows(db, &format!("SELECT 1 FROM sys.database_principals WHERE name = {}", lit(user))).await?.is_empty();
    let login_there = s(&row[1]) != "0";
    if !user_there && !login_there {
        return Ok(None);
    }
    Ok(Some(user_there && marked_for(&mark, conn)))
}

pub(crate) async fn inventory(db: &Db) -> Result<Inventory> {
    let rows = text_rows(
        db,
        "SELECT s.name, o.name, o.type, c.name, c.is_computed, COALESCE(USER_NAME(o.principal_id), USER_NAME(s.principal_id)), \
         CASE WHEN IS_SRVROLEMEMBER('sysadmin') = 1 OR IS_MEMBER('db_owner') = 1 OR HAS_PERMS_BY_NAME(QUOTENAME(s.name) + '.' + QUOTENAME(o.name), 'OBJECT', 'CONTROL') = 1 THEN 1 ELSE 0 END \
         FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id JOIN sys.columns c ON c.object_id = o.object_id \
         WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 AND s.name NOT LIKE 'dataman[_]agent[_]%' ORDER BY s.name, o.name, c.column_id",
    )
    .await?;
    let mut out = Inventory::default();
    for r in rows {
        let (sch, name) = (s(&r[0]), s(&r[1]));
        if out.objects.last().is_none_or(|o: &Obj| o.schema != sch || o.name != name) {
            let view = s(&r[2]).trim() == "V";
            out.objects.push(Obj { schema: sch, name, kind: if view { ObjKind::View } else { ObjKind::Table }, columns: Vec::new(), grantable: one(&r[6]), owner: s(&r[5]), sequences: Vec::new() });
        }
        out.objects.last_mut().expect("pushed").columns.push(Col { name: s(&r[3]), generated: one(&r[4]) });
    }
    // What every user of the database may read (granted to public): the agent's user too.
    let open = text_rows(
        db,
        "SELECT s.name, o.name, c.name FROM sys.database_permissions p JOIN sys.objects o ON o.object_id = p.major_id JOIN sys.schemas s ON s.schema_id = o.schema_id \
         LEFT JOIN sys.columns c ON p.minor_id > 0 AND c.object_id = p.major_id AND c.column_id = p.minor_id \
         WHERE p.class = 1 AND p.grantee_principal_id = 0 AND p.permission_name = 'SELECT' AND p.state IN ('G', 'W') AND o.type IN ('U', 'V') AND o.is_ms_shipped = 0 ORDER BY s.name, o.name",
    )
    .await?;
    for r in open {
        let (sch, name) = (s(&r[0]), s(&r[1]));
        if out.open.last().is_none_or(|o: &Open| o.schema != sch || o.name != name) {
            out.open.push(Open { schema: sch, name, columns: Vec::new(), whole: false });
        }
        let o = out.open.last_mut().expect("pushed");
        match &r[2] {
            Some(c) => o.columns.push(c.clone()),
            None => o.whole = true,
        }
    }
    Ok(out)
}

pub(crate) async fn have(db: &Db, user: &str, space: &str, azure: bool) -> Result<(Vec<Grant>, Vec<(String, String)>, bool)> {
    let u = lit(user);
    let mut out = Vec::new();
    let perms = text_rows(
        db,
        &format!(
            "SELECT p.class, s.name, o.name, c.name, p.permission_name FROM sys.database_permissions p \
             LEFT JOIN sys.objects o ON p.class = 1 AND o.object_id = p.major_id LEFT JOIN sys.schemas s ON s.schema_id = o.schema_id \
             LEFT JOIN sys.columns c ON p.class = 1 AND p.minor_id > 0 AND c.object_id = p.major_id AND c.column_id = p.minor_id \
             WHERE p.grantee_principal_id = DATABASE_PRINCIPAL_ID({u}) AND p.state IN ('G', 'W')"
        ),
    )
    .await?;
    for r in perms {
        match (s(&r[0]).as_str(), &r[3]) {
            ("1", None) => out.push(Grant::Table { schema: s(&r[1]), name: s(&r[2]), privilege: s(&r[4]) }),
            ("1", Some(c)) => out.push(Grant::Column { schema: s(&r[1]), name: s(&r[2]), column: c.clone(), privilege: s(&r[4]) }),
            ("0", _) if s(&r[4]) != "CONNECT" => out.push(Grant::Database(s(&r[4]))),
            _ => {}
        }
    }
    for r in text_rows(db, &format!("SELECT r.name FROM sys.database_role_members m JOIN sys.database_principals r ON r.principal_id = m.role_principal_id WHERE m.member_principal_id = DATABASE_PRINCIPAL_ID({u})")).await? {
        out.push(Grant::Member(s(&r[0])));
    }
    if !azure {
        for r in text_rows(db, &format!("SELECT permission_name FROM sys.server_permissions WHERE grantee_principal_id = SUSER_ID({u}) AND class = 100 AND permission_name <> 'CONNECT SQL' AND state IN ('G', 'W')")).await.unwrap_or_default() {
            out.push(Grant::Global(s(&r[0])));
        }
    }
    let mut views = Vec::new();
    for r in text_rows(
        db,
        &format!(
            "SELECT v.name, CAST(ep.value AS NVARCHAR(400)) FROM sys.views v JOIN sys.schemas s ON s.schema_id = v.schema_id \
             LEFT JOIN sys.extended_properties ep ON ep.class = 1 AND ep.major_id = v.object_id AND ep.minor_id = 0 AND ep.name = N'dataman' WHERE s.name = {}",
            lit(space)
        ),
    )
    .await?
    {
        if let Some(h) = s(&r[1]).strip_prefix(MARK).map(str::trim) {
            views.push((s(&r[0]), h.to_string()));
            out.push(Grant::View { name: s(&r[0]), hash: h.to_string() });
        }
    }
    let schema = text_rows(db, &format!("SELECT CAST(ep.value AS NVARCHAR(400)) FROM sys.schemas s LEFT JOIN sys.extended_properties ep ON ep.class = 3 AND ep.major_id = s.schema_id AND ep.name = N'dataman' WHERE s.name = {}", lit(space))).await?;
    if let Some(r) = schema.first()
        && !s(&r[0]).starts_with(MARK)
    {
        return Err(anyhow!("a schema named {space} exists and is not Dataman's: Dataman leaves it alone; rename it to create the agent login"));
    }
    out.sort();
    out.dedup();
    Ok((out, views, !schema.is_empty()))
}

pub(crate) async fn made_here(db: &Db, space: &str) -> Result<(Vec<String>, bool, bool)> {
    let schema = text_rows(db, &format!("SELECT CAST(ep.value AS NVARCHAR(400)) FROM sys.schemas s LEFT JOIN sys.extended_properties ep ON ep.class = 3 AND ep.major_id = s.schema_id AND ep.name = N'dataman' WHERE s.name = {}", lit(space))).await?;
    let Some(r) = schema.first() else { return Ok((Vec::new(), false, false)) };
    let ours = s(&r[0]).starts_with(MARK);
    let objs = text_rows(
        db,
        &format!(
            "SELECT o.name, o.type, CAST(ep.value AS NVARCHAR(400)) FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
             LEFT JOIN sys.extended_properties ep ON ep.class = 1 AND ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = N'dataman' WHERE s.name = {} AND o.parent_object_id = 0",
            lit(space)
        ),
    )
    .await?;
    let (mut views, mut salt, mut other) = (Vec::new(), false, false);
    for o in objs {
        match (s(&o[1]).trim(), s(&o[0]).as_str()) {
            ("V", n) if s(&o[2]).starts_with(MARK) => views.push(n.to_string()),
            ("U", "salt") if ours => salt = true,
            _ => other = true,
        }
    }
    Ok((views, salt, ours && !other))
}
