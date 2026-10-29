use super::*;
use crate::config::{ConnectionProfile, Engine, Environment, McpAccess};
use crate::privacy::{ItemKind, SensitiveCol, Visible, VisibleItem, VisibleMode};

fn conn(engine: Engine) -> ConnectionProfile {
    let mut p = ConnectionProfile::new("shop");
    p.id = "a1b2c3d4e5f6".into();
    p.engine = engine;
    p.database = "shop".into();
    p.host = "db.example.com".into();
    p.mcp = Some(McpAccess::Write);
    p
}

fn table(schema: &str, name: &str, cols: &[&str]) -> Obj {
    Obj { schema: schema.into(), name: name.into(), kind: ObjKind::Table, columns: cols.iter().map(|c| Col::new(c)).collect(), grantable: true, owner: "app".into(), sequences: Vec::new() }
}

fn inv_sql(schema: &str) -> Inventory {
    let mut orders = table(schema, "orders", &["id", "total", "customer_id"]);
    orders.sequences = vec!["orders_id_seq".into()];
    Inventory {
        objects: vec![
            orders,
            table(schema, "users", &["id", "name", "email", "password_hash", "created_at"]),
            table(schema, "audit_log", &["id", "what"]),
            Obj { grantable: false, owner: "app_owner".into(), ..table(schema, "ledger", &["id", "amount"]) },
        ],
    }
}

/// The rules of the prototype: write, Masked, audit_log hidden, users.password_hash hidden,
/// users.email masked (Partial), no structure, no code.
fn guarded(engine: Engine) -> ConnectionProfile {
    let mut p = conn(engine);
    let r = &mut p.mcp_rules;
    r.view = AgentView::Masked;
    r.visible.items.push(VisibleItem { name: "audit_log".into(), kind: ItemKind::Table });
    r.visible.items.push(VisibleItem { name: "users.password_hash".into(), kind: ItemKind::Column });
    r.sensitive.push(SensitiveCol { col: "users.email".into(), how: Mask::Partial, on: true });
    r.structure = false;
    r.code = false;
    r.time_limit_s = Some(30);
    p
}

fn opts() -> Options {
    Options { rotate_days: None, conn_limit: Some(4), verify: true }
}

fn probe_sql() -> Probe {
    Probe { login: "bruno_admin".into(), can_create: true, flavor: Flavor { major: 16, activity: true, ..Default::default() }, ..Default::default() }
}

fn golden(name: &str, text: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/agent_login_golden").join(name);
    if std::env::var_os("DATAMAN_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("no golden file {} (DATAMAN_UPDATE_GOLDEN=1 writes it)", path.display()));
    assert_eq!(text, want, "{name} changed (DATAMAN_UPDATE_GOLDEN=1 rewrites it)");
}

fn whos(plan: &Plan) -> Vec<(String, Who)> {
    plan.rules.iter().map(|r| (r.rule.clone(), r.who)).collect()
}

#[test]
fn names() {
    assert_eq!(login_name("a1b2c3d4e5f6"), "dataman_agent_a1b2c3d4");
    assert_eq!(login_name("A1-B2_c3D4e5"), "dataman_agent_a1b2c3d4");
    // A short or odd id (a project's): a hash, still plain.
    let n = login_name("x");
    assert!(n.starts_with(PREFIX) && n.len() == PREFIX.len() + 8 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'), "{n}");
    assert_eq!(secret_account("abc"), "agent:abc");
    assert!(!supported(Engine::Sqlite) && !supported(Engine::DuckDb) && supported(Engine::Redis));
}

#[test]
fn compile_follows_the_tier_and_read_only() {
    let mut p = conn(Engine::Postgres);
    p.mcp = Some(McpAccess::Read);
    assert!(!compile(&p, "", &opts()).write);
    p.mcp = Some(McpAccess::Full);
    assert!(compile(&p, "", &opts()).write);
    p.read_only = true;
    assert!(!compile(&p, "", &opts()).write, "read-only beats the tier");
    p.read_only = false;
    p.set_read_only_for("shop", true);
    assert!(!compile(&p, "shop", &opts()).write);
    // Off compiles as read (the login then holds the least).
    p.mcp = Some(McpAccess::Off);
    assert_eq!(compile(&p, "", &opts()).tier, McpAccess::Read);
    p.mcp_rules.view = AgentView::Structure;
    p.mcp = Some(McpAccess::Write);
    let s = compile(&p, "other", &opts());
    assert!(!s.rows && !s.write);
    // Activity is off whenever something is kept.
    let g = compile(&guarded(Engine::Postgres), "", &opts());
    assert!(!g.activity && !g.kill);
}

#[test]
fn rules_hash_moves_with_what_is_granted_only() {
    let p = guarded(Engine::Postgres);
    let h = rules_hash(&p, "");
    let mut q = p.clone();
    q.mcp_rules.read_rows = Some(20);
    q.name = "renamed".into();
    assert_eq!(rules_hash(&q, ""), h, "Dataman's own limits don't change the grants");
    q.mcp_rules.sensitive[0].how = Mask::Hidden;
    assert_ne!(rules_hash(&q, ""), h);
    let mut q = p.clone();
    q.mcp = Some(McpAccess::Read);
    assert_ne!(rules_hash(&q, ""), h);
}

#[test]
fn access_over_globs_and_generated_columns() {
    let mut p = conn(Engine::Postgres);
    p.mcp_rules.visible.items = vec![VisibleItem { name: "audit_*".into(), kind: ItemKind::Table }, VisibleItem { name: "*.secret*".into(), kind: ItemKind::Column }];
    let mut inv = inv_sql("public");
    inv.objects.push(table("public", "AUDIT_2024", &["id"]));
    inv.objects.push(table("public", "audit", &["id"]));
    inv.objects[1].columns.push(Col::new("secret_key"));
    inv.objects[1].columns.push(Col { name: "secret_len".into(), generated: true });
    inv.objects[1].columns.push(Col { name: "name_upper".into(), generated: true });
    inv.objects.push(table("dataman_agent_x", "users", &["id"]));
    let s = compile(&p, "", &opts());
    let (acc, hidden) = access(&s, &inv);
    assert!(hidden.contains(&"public.audit_log".to_string()) && hidden.contains(&"public.AUDIT_2024".to_string()), "globs ignore case: {hidden:?}");
    assert!(acc.iter().any(|a| a.name == "audit"), "audit_* needs the underscore");
    let users = acc.iter().find(|a| a.name == "users").unwrap();
    assert_eq!(users.hidden, ["secret_key", "secret_len", "name_upper"], "a generated column in a table that keeps something is kept too");
    assert!(!acc.iter().any(|a| a.schema.starts_with(PREFIX)), "Dataman's own views are never granted as tables");
    // Only these: the rest is hidden.
    p.mcp_rules.visible = Visible { mode: VisibleMode::Only, items: vec![VisibleItem { name: "orders".into(), kind: ItemKind::Table }] };
    let (acc, hidden) = access(&compile(&p, "", &opts()), &inv_sql("public"));
    assert_eq!(acc.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["orders"]);
    assert_eq!(hidden.len(), 3);
    // Every column hidden: the table is hidden whole.
    p.mcp_rules.visible = Visible { mode: VisibleMode::Except, items: vec![VisibleItem { name: "orders.*".into(), kind: ItemKind::Column }] };
    let (acc, hidden) = access(&compile(&p, "", &opts()), &inv_sql("public"));
    assert!(!acc.iter().any(|a| a.name == "orders") && hidden.iter().any(|h| h.contains("every column hidden")), "{hidden:?}");
}

#[test]
fn postgres_plan() {
    let mut p = guarded(Engine::Postgres);
    p.env = Environment::Production;
    let s = compile(&p, "", &opts());
    let plan = pg::create_plan(&s, &inv_sql("public"), &probe_sql(), "pw-not-shown", true);
    let shown = plan.shown();
    golden("pg_guarded.sql", &shown);
    assert!(!shown.contains("pw-not-shown") && !plan.copy().contains("pw-not-shown"), "the password never travels in clear");
    assert!(shown.contains("'SCRAM-SHA-256$4096:…'") && plan.copy().contains("SCRAM-SHA-256$4096:"));
    assert!(shown.contains("GRANT SELECT, INSERT, UPDATE, DELETE ON \"public\".\"orders\""), "{shown}");
    assert!(shown.contains("GRANT USAGE ON SEQUENCE \"public\".\"orders_id_seq\""));
    assert!(shown.contains("GRANT SELECT (\"created_at\", \"id\", \"name\"), INSERT (\"created_at\", \"id\", \"name\"), UPDATE (\"created_at\", \"id\", \"name\"), DELETE ON \"public\".\"users\""), "{shown}");
    assert!(!shown.contains("(\"password_hash\")") && !shown.contains("(\"email\")"), "hidden and masked columns are not granted on the table");
    assert!(shown.contains("CREATE VIEW \"dataman_agent_a1b2c3d4_views\".\"users\" WITH (security_barrier)"));
    let run: String = shown.lines().filter(|l| !l.starts_with("--")).collect::<Vec<_>>().join("\n");
    assert!(!run.contains("audit_log") && !run.contains("ledger"), "hidden and not grantable: no grant");
    assert_eq!(plan.not_granted.len(), 1);
    assert!(plan.copy().contains("GRANT SELECT, INSERT, UPDATE, DELETE ON \"public\".\"ledger\""), "the owner's lines, commented");
    assert!(!shown.contains("TRUNCATE") && !shown.contains("CREATE ON") && !shown.contains("ALTER DEFAULT PRIVILEGES"));
    assert!(shown.starts_with("BEGIN;\n") && shown.contains("COMMIT;"));
    assert_eq!(plan.views.len(), 1);
    assert_eq!(plan.views[0].masked, vec![("email".to_string(), Mask::Partial)]);
    let w = whos(&plan);
    assert!(w.contains(&("public.audit_log hidden".into(), Who::Partly)), "{w:?}");
    assert!(w.iter().any(|(r, who)| r.contains("password_hash hidden") && *who == Who::Database));
    assert!(w.iter().any(|(r, who)| r.contains("email masked") && *who == Who::Database));
    assert!(w.iter().any(|(r, who)| r.contains("Run code off") && *who == Who::Dataman));
    assert!(w.iter().any(|(r, who)| r.contains("production") && *who == Who::Dataman));
}

#[test]
fn postgres_read_tier_activity_and_structure() {
    let mut p = conn(Engine::Postgres);
    p.mcp = Some(McpAccess::Read);
    let plan = pg::create_plan(&compile(&p, "", &opts()), &inv_sql("public"), &probe_sql(), "x", false);
    let shown = plan.shown();
    golden("pg_read_open.sql", &shown);
    assert!(!shown.contains("INSERT") && !shown.contains("SEQUENCE"), "{shown}");
    assert!(shown.contains("default_transaction_read_only = 'on'"));
    assert!(shown.contains("GRANT \"pg_read_all_stats\""), "activity on, nothing kept");
    assert!(!shown.contains("pg_signal_backend"), "cancel needs Write");
    // A login that can't grant the predefined role: activity stays Dataman's.
    let mut pr = probe_sql();
    pr.flavor.activity = false;
    let plan = pg::create_plan(&compile(&p, "", &opts()), &inv_sql("public"), &pr, "x", false);
    assert!(!plan.shown().contains("pg_read_all_stats\" TO") && whos(&plan).iter().any(|(r, w)| r == "Server activity" && *w == Who::Dataman));
    p.mcp_rules.view = AgentView::Structure;
    let plan = pg::create_plan(&compile(&p, "", &opts()), &inv_sql("public"), &probe_sql(), "x", false);
    assert!(!plan.shown().contains("ON \"public\"."), "structure only: no table");
}

#[test]
fn hostile_names_are_quoted_everywhere() {
    let evil = ["we\"ird; DROP ROLE x; --", "back`tick", "br]ack[et", "ünï'cødé", "semi;colon"];
    for engine in [Engine::Postgres, Engine::MySql, Engine::SqlServer] {
        let mut p = conn(engine);
        p.mcp_rules.view = AgentView::Masked;
        p.mcp_rules.sensitive.push(SensitiveCol { col: "*".into(), how: Mask::Hash, on: true });
        let schema = if engine == Engine::MySql { "sh\"op`db" } else { "pub]lic\"" };
        let mut inv = Inventory { objects: evil.iter().map(|n| table(schema, n, &[n, "id"])).collect() };
        for o in &mut inv.objects {
            o.owner = "dbo".into();
        }
        let s = compile(&p, "", &opts());
        let plan = match engine {
            Engine::Postgres => pg::create_plan(&s, &inv, &probe_sql(), "x", false),
            Engine::MySql => my::create_plan(&s, &inv, &probe_sql(), "x", false),
            _ => ms::create_plan(&s, &inv, &probe_sql(), "x", false),
        };
        let text = plan.copy();
        let q = match engine {
            Engine::Postgres => pg::q("we\"ird; DROP ROLE x; --"),
            Engine::MySql => my::q("back`tick"),
            _ => ms::q("br]ack[et"),
        };
        assert!(text.contains(&q), "{engine:?}: {q} in\n{text}");
        // No statement is cut by a name: each step is one statement in its dialect.
        for st in plan.steps_run() {
            let Action::Sql(sql) = &st.action else { continue };
            let n = crate::sql::split_in(sql, engine.dialect()).len();
            assert_eq!(n, 1, "{engine:?}: {sql}");
        }
    }
    assert_eq!(pg::q("a\"b"), "\"a\"\"b\"");
    assert_eq!(my::q("a`b"), "`a``b`");
    assert_eq!(ms::q("a]b"), "[a]]b]");
    assert_eq!(my::lit("a\\'b"), "'a\\\\''b'");
    assert_eq!(ms::lit("a'b"), "N'a''b'");
    assert_eq!(redis::key_pattern("user:[1]?\\*"), "user:\\[1\\]\\?\\\\*");
}

#[test]
fn mysql_plan() {
    let p = guarded(Engine::MySql);
    let s = compile(&p, "", &opts());
    let mut probe = probe_sql();
    probe.flavor.major = 8;
    let plan = my::create_plan(&s, &inv_sql("shop"), &probe, "pw-not-shown", false);
    let shown = plan.shown();
    golden("mysql_guarded.sql", &shown);
    assert!(!plan.copy().contains("pw-not-shown"));
    assert!(plan.copy().contains("IDENTIFIED WITH caching_sha2_password AS '$A$005$"));
    assert!(shown.contains("ACCOUNT LOCK") && plan.steps_run().last().unwrap().shown.ends_with("ACCOUNT UNLOCK;"), "unlocked last");
    assert!(shown.contains("ATTRIBUTE '{\"dataman\":\"dataman-managed v1\",\"conn\":\"a1b2c3d4e5f6\"}'"));
    assert!(shown.contains("CREATE SQL SECURITY DEFINER VIEW `dataman_agent_a1b2c3d4_views`.`shop__users`"));
    assert!(!plan.one_tx);
    assert!(whos(&plan).iter().any(|(r, w)| r.contains("per call") && *w == Who::Dataman), "MySQL: no time limit per user");
    // MariaDB: its hash, MAX_STATEMENT_TIME, the mark on the view database.
    probe.flavor.mariadb = true;
    let plan = my::create_plan(&s, &inv_sql("shop"), &probe, "x", false);
    let shown = plan.shown();
    golden("mariadb_guarded.sql", &shown);
    assert!(plan.copy().contains("IDENTIFIED BY PASSWORD '*") && shown.contains("MAX_STATEMENT_TIME 30") && !shown.contains("ATTRIBUTE"));
    assert!(shown.contains("CREATE DATABASE `dataman_agent_a1b2c3d4_views` COMMENT 'dataman-managed v1 conn=a1b2c3d4e5f6'"));
    assert!(whos(&plan).iter().any(|(r, w)| r.contains("per call") && *w == Who::Database));
}

#[test]
fn sql_server_plan() {
    let p = guarded(Engine::SqlServer);
    let s = compile(&p, "", &opts());
    let mut inv = inv_sql("dbo");
    for o in &mut inv.objects {
        o.owner = "dbo".into();
    }
    let plan = ms::create_plan(&s, &inv, &probe_sql(), "pw-not-shown", false);
    let shown = plan.shown();
    golden("mssql_guarded.sql", &shown);
    assert!(!plan.copy().contains("pw-not-shown") && plan.copy().contains("HASHED"));
    assert!(shown.contains("CREATE VIEW [dataman_agent_a1b2c3d4_views].[users]"), "{shown}");
    assert!(shown.contains("GRANT SELECT ([created_at], [id], [name]), UPDATE ([created_at], [id], [name]) ON OBJECT::[dbo].[users]") && shown.contains("GRANT DELETE ON OBJECT::[dbo].[users]"), "writes on the visible columns: {shown}");
    assert!(!shown.contains("INSERT ([") && !shown.contains("INSERT, UPDATE, DELETE ON OBJECT::[dbo].[users]"), "no INSERT per column on SQL Server");
    assert!(!shown.contains("[email]),") && !shown.contains("([password_hash"), "never the masked or hidden ones");
    assert!(shown.contains("ALTER LOGIN [dataman_agent_a1b2c3d4] DISABLE") && shown.contains("ENABLE"));
    assert!(!shown.contains("EXECUTE ON") && !shown.contains("db_datareader"));
    // Azure SQL Database: a contained user, its password in the copy only.
    let mut az = probe_sql();
    az.flavor.azure = true;
    let plan = ms::create_plan(&s, &inv, &az, "pw-not-shown", false);
    assert!(plan.shown().contains("CREATE USER [dataman_agent_a1b2c3d4] WITH PASSWORD = N'<password from the Keychain>'") && !plan.shown().contains("pw-not-shown"));
    assert!(!plan.shown().contains("CREATE LOGIN"));
    // Read tier: denied writes on top.
    let mut r = conn(Engine::SqlServer);
    r.mcp = Some(McpAccess::Read);
    let plan = ms::create_plan(&compile(&r, "", &opts()), &inv, &probe_sql(), "x", false);
    assert!(plan.shown().contains("ALTER ROLE [db_denydatawriter] ADD MEMBER"));
}

fn inv_docs() -> Inventory {
    Inventory {
        objects: vec![
            Obj { kind: ObjKind::Collection, ..table("shop", "orders", &["total", "status"]) },
            Obj { kind: ObjKind::Collection, ..table("shop", "users", &["name", "email", "address.street"]) },
            Obj { kind: ObjKind::Collection, ..table("shop", "audit_log", &["what"]) },
        ],
    }
}

#[test]
fn mongo_plan() {
    let p = guarded(Engine::MongoDb);
    let s = compile(&p, "", &opts());
    let plan = mongo::create_plan(&s, &inv_docs(), &Probe { can_create: true, ..Default::default() }, "pw-not-shown", false);
    let shown = plan.shown();
    golden("mongo_guarded.js", &shown);
    assert!(!shown.contains("pw-not-shown") && plan.copy().contains("pw-not-shown"), "MongoDB takes the password: the copy alone has it");
    assert!(shown.contains("\"createRole\":\"dataman_agent_a1b2c3d4\""));
    assert!(shown.contains("\"create\":\"dataman_agent_a1b2c3d4_views.users\"") && shown.contains("\"password_hash\":0"), "the hidden field (named by the rules, not sampled) is projected out: {shown}");
    assert!(!shown.contains("listCollections") && !shown.contains("\"collection\":\"audit_log\""));
    assert!(shown.find("createUser").unwrap() > shown.find("createRole").unwrap(), "the user last");
    // Atlas: nothing to run, the role to paste.
    let mut atlas = conn(Engine::MongoDb);
    atlas.host = "cluster0.ab12c.mongodb.net".into();
    assert!(mongo::atlas(&atlas));
    let text = mongo::atlas_text(&s, &inv_docs());
    assert!(text.contains("\"action\": \"FIND\"") && text.contains("\"roleName\": \"dataman_agent_a1b2c3d4\""), "{text}");
}

#[test]
fn redis_rules() {
    let mut p = conn(Engine::Redis);
    p.mcp = Some(McpAccess::Read);
    let s = compile(&p, "", &opts());
    let (t, held) = redis::rules_for(&s);
    assert!(held && t.contains(&"~*".to_string()) && t.contains(&"+@read".to_string()) && !t.contains(&"+@write".to_string()));
    let d = t.iter().position(|x| x == "-@dangerous").unwrap();
    assert!(t.iter().position(|x| x == "+info").unwrap() > d, "INFO added back after @dangerous goes");
    assert!(!t.iter().any(|x| x == "+@scripting" || x == "+@dangerous" || x == "+@all" || x == "allcommands"));
    let plan = redis::create_plan(&s, &Probe::default(), redis::Persist::AclFile, "pw-not-shown", false);
    golden("redis_read.txt", &plan.shown());
    assert!(!plan.copy().contains("pw-not-shown") && plan.copy().contains(&format!("#{}", hash::redis_sha("pw-not-shown"))));
    assert!(plan.shown().contains("ACL SAVE") && plan.shown().contains("(%R~dataman-managed:a1b2c3d4e5f6 -@all)"));
    // Only these: patterns. Everything except: the ACL can't, Dataman holds it.
    p.mcp = Some(McpAccess::Write);
    p.mcp_rules.visible = Visible { mode: VisibleMode::Only, items: vec![VisibleItem { name: "cache:*".into(), kind: ItemKind::Table }] };
    let (t, held) = redis::rules_for(&compile(&p, "", &opts()));
    assert!(held && t.contains(&"~cache:*".to_string()) && !t.contains(&"~*".to_string()) && t.contains(&"+@write".to_string()));
    p.mcp_rules.visible = Visible { mode: VisibleMode::Except, items: vec![VisibleItem { name: "secret:*".into(), kind: ItemKind::Table }] };
    let s = compile(&p, "", &opts());
    let (_, held) = redis::rules_for(&s);
    assert!(!held);
    let plan = redis::create_plan(&s, &Probe::default(), redis::Persist::None, "x", false);
    assert!(whos(&plan).iter().any(|(r, w)| r == "Hidden keys" && *w == Who::Dataman));
    assert!(whos(&plan).iter().any(|(r, w)| r == "Kept across restarts" && *w == Who::Dataman));
    p.mcp_rules.redis.write = false;
    let (t, _) = redis::rules_for(&compile(&p, "", &opts()));
    assert!(!t.contains(&"+@write".to_string()), "a category off stays off");
}

#[test]
fn search_roles() {
    let p = guarded(Engine::Elasticsearch);
    let s = compile(&p, "", &opts());
    let mut inv = inv_docs();
    for o in &mut inv.objects {
        o.schema.clear();
        o.kind = ObjKind::Index;
    }
    let es = search::desired(&s, &inv, false, true);
    let role = es.role.to_string();
    assert!(role.contains("\"password_hash\",\"password_hash.*\"") && role.contains("\"email\",\"email.*\""), "Elasticsearch can't mask: left out: {role}");
    assert!(!role.contains("audit_log"));
    let plan = search::create_plan(&s, &inv, &Probe { flavor: Flavor { superuser: true, ..Default::default() }, ..Default::default() }, "pw-not-shown", false);
    golden("es_guarded.txt", &plan.shown());
    assert!(!plan.copy().contains("pw-not-shown") && plan.copy().contains("$2a$10$"));
    let os = search::desired(&s, &inv, true, true);
    let role = os.role.to_string();
    assert!(role.contains("\"masked_fields\":[\"email\"]") && role.contains("\"~password_hash\""), "{role}");
    // Basic license: no FLS, so the index that needs it is not granted at all.
    let basic = search::desired(&s, &inv, false, false);
    assert!(basic.not_granted.iter().any(|n| n.object == "users") && !basic.role.to_string().contains("\"users\""));
}

#[test]
fn diff_and_sync() {
    let have = vec!["SELECT ON a".to_string(), "SELECT ON b".into()];
    let want = vec!["SELECT ON b".to_string(), "SELECT ON c".into()];
    let d = diff(&have, &want);
    assert_eq!(d, vec![Change { add: false, line: "SELECT ON a".into() }, Change { add: true, line: "SELECT ON c".into() }]);
    assert!(diff(&want, &want).is_empty());

    // PostgreSQL: a table that gains a masked column loses its table grant and gets a view.
    let mut p = conn(Engine::Postgres);
    let probe = probe_sql();
    let inv = inv_sql("public");
    let before = pg::desired(&compile(&p, "", &opts()), &inv, &probe).grants;
    p.mcp_rules.view = AgentView::Masked;
    p.mcp_rules.sensitive.push(SensitiveCol { col: "orders.total".into(), how: Mask::Hidden, on: true });
    let s = compile(&p, "", &opts());
    let plan = pg::sync_plan(&s, &inv, &probe, &before, &[], false, false);
    let text = plan.shown();
    assert!(text.contains("REVOKE SELECT ON \"public\".\"orders\" FROM"), "{text}");
    assert!(text.contains("CREATE VIEW \"dataman_agent_a1b2c3d4_views\".\"orders\""));
    assert!(text.contains("GRANT SELECT (\"id\") ON \"public\".\"orders\""), "the columns granted again after the table's revoke");
    assert!(text.find("REVOKE").unwrap() < text.find("GRANT").unwrap(), "revoked first: never wider in between");
    // In sync: nothing to run.
    let now = pg::desired(&s, &inv, &probe);
    let views: Vec<(String, String)> = now.views.iter().map(|v| (v.name.clone(), v.hash.clone())).collect();
    assert!(pg::sync_plan(&s, &inv, &probe, &now.grants, &views, true, false).steps_run().next().is_none());
    // A new table appears: granted at the next sync only (fail closed until then).
    let mut more = inv.clone();
    more.objects.push(table("public", "invoices", &["id"]));
    let plan = pg::sync_plan(&s, &more, &probe, &now.grants, &views, true, false);
    assert!(plan.shown().contains("GRANT SELECT ON \"public\".\"invoices\""));
}

#[test]
fn remove_drops_only_what_dataman_made() {
    let plan = pg::remove_plan("dataman_agent_x", "dataman_agent_x", "shop", &[], &["users".into()], true, true, true);
    let t = plan.shown();
    assert!(t.contains("DROP VIEW IF EXISTS \"dataman_agent_x\".\"users\"") && t.contains("DROP OWNED BY \"dataman_agent_x\"") && t.contains("DROP ROLE \"dataman_agent_x\""));
    assert!(!t.contains("CASCADE"), "never CASCADE: what depends on it is not Dataman's");
    let plan = pg::remove_plan("dataman_agent_x", "dataman_agent_x", "shop", &[Grant::Table { schema: "public".into(), name: "t".into(), privilege: "SELECT".into() }], &[], false, false, false);
    assert!(plan.shown().contains("REVOKE SELECT ON \"public\".\"t\" FROM") && !plan.shown().contains("DROP OWNED") && !plan.shown().contains("DROP SCHEMA"));
    let my = my::remove_plan("u", "u", &[], false, false).shown();
    assert!(my.contains("DROP USER 'u'@'%'") && !my.contains("DROP DATABASE"));
    let ms = ms::remove_plan("u", "u", &[], false, false, false).shown();
    assert!(ms.contains("DROP USER IF EXISTS [u]") && ms.contains("DROP LOGIN [u]") && !ms.contains("DROP SCHEMA"));
    assert!(mongo::remove_plan("u", &[]).shown().contains("\"dropRole\":\"u\""));
    assert!(redis::remove_plan("u", redis::Persist::Config).shown().contains("ACL DELUSER u"));
}

#[test]
fn profile_back_compat_and_secrets() {
    // A file of an older Dataman (no agent login) reads as before and writes nothing new.
    let p = conn(Engine::Postgres);
    let text = toml::to_string(&p).unwrap();
    assert!(!text.contains("agent_login") && !text.contains("agents_refused"));
    let back: ConnectionProfile = toml::from_str(&text).unwrap();
    assert_eq!(back, p);
    let mut q = p.clone();
    let plan = pg::create_plan(&compile(&q, "", &opts()), &inv_sql("public"), &probe_sql(), "pw-not-shown", false);
    q.agent_login = Some(live::record(&q, &plan, &opts()));
    let text = toml::to_string(&q).unwrap();
    assert!(text.contains("[agent_login]") && !text.contains("pw-not-shown") && !text.contains("SCRAM"), "{text}");
    assert_eq!(toml::from_str::<ConnectionProfile>(&text).unwrap(), q);
}

#[test]
fn agent_profile_never_takes_the_users_password() {
    let mut p = conn(Engine::MongoDb);
    p.save_password = true;
    p.options.insert("authMechanism".into(), "MONGODB-X509".into());
    p.options.insert("authSource".into(), "$external".into());
    let a = live::agent_profile(&p, "dataman_agent_x");
    assert_eq!((a.user.as_str(), a.save_password, a.agent_made), ("dataman_agent_x", false, true));
    assert_eq!(a.options.get("authSource").map(String::as_str), Some("admin"));
    assert!(!a.options.contains_key("authMechanism"));
    let secrets = crate::MemorySecrets::default();
    crate::SecretStore::set(&secrets, &p.id, "the-users-password").unwrap();
    let e = live::password_of(&secrets, &p.id, "dataman_agent_x").unwrap_err().to_string();
    assert!(e.contains("not in the") && e.contains("refused"), "{e}");
    crate::SecretStore::set(&secrets, &secret_account(&p.id), "agent-pw").unwrap();
    assert_eq!(live::password_of(&secrets, &p.id, "x").unwrap(), "agent-pw");
}

#[test]
fn plan_hash_is_stable_and_moves_with_the_grants() {
    let p = guarded(Engine::Postgres);
    let s = compile(&p, "", &opts());
    let a = pg::create_plan(&s, &inv_sql("public"), &probe_sql(), "x", false);
    let b = pg::create_plan(&s, &inv_sql("public"), &probe_sql(), "y", false);
    assert_eq!(a.hash, b.hash, "another password, the same grants");
    let mut inv = inv_sql("public");
    inv.objects.push(table("public", "invoices", &["id"]));
    assert_ne!(pg::create_plan(&s, &inv, &probe_sql(), "x", false).hash, a.hash);
}
