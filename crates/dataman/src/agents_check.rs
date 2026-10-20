//! `agents-check`: the Agents area through its own controls (the rail's robot, the nav, filters,
//! segments, checkboxes, fields and menus), with what each writes read back from the saved
//! connection and settings. The `agents-*` demo stages show its pages for screenshots.

use std::collections::BTreeMap;

use dataman_core::config::Engine;
use dataman_core::privacy::{AgentView, CustomType, ItemKind, Mask, ReadFrom, SensitiveCol, VisibleItem};
use dataman_core::{ConnectionProfile, Core, Environment, McpAccess};
use dataman_mcp::audit::{ApprovalNote, Audit, Decision, Entry, Outcome, Seen};
use gpui::{AsyncWindowContext, Entity};

use crate::agents::{AgentsPage, Page};
use crate::app::Workspace;
use crate::check::*;

fn profile(name: &str, engine: Engine, env: Environment, tier: McpAccess) -> ConnectionProfile {
    let mut p = ConnectionProfile::new(name);
    p.engine = engine;
    p.port = engine.default_port();
    p.env = env;
    p.mcp = Some(tier);
    p
}

/// The connections, rules and types of the mockups.
fn seed_profiles(core: &Core) {
    let mut pg = profile("prod-pg", Engine::Postgres, Environment::Production, McpAccess::Write);
    let r = &mut pg.mcp_rules;
    r.view = AgentView::Masked;
    for (col, how, on) in [("users.email", Mask::Partial, true), ("customers.cpf", Mask::Hash, true), ("customers.doc", Mask::Hash, true), ("customers.phone", Mask::Partial, true), ("orders.notes", Mask::Hidden, false)] {
        r.sensitive.push(SensitiveCol { col: col.into(), how, on });
    }
    for (name, kind) in [("public.payments", ItemKind::Table), ("audit.*", ItemKind::Schema), ("users.password_hash", ItemKind::Column), ("users.mfa_secret", ItemKind::Column)] {
        r.visible.items.push(VisibleItem { name: name.into(), kind });
    }
    r.structure = false;
    r.activity = false;
    (r.time_limit_s, r.read_rows, r.read_bytes) = (Some(30), Some(100), Some(64 * 1024));
    r.types = BTreeMap::from([("phone".to_string(), false), ("t_cust01".to_string(), true)]);
    pg.mcp_row_limit = Some(100);
    pg.mcp_allowed = vec!["UPDATE orders SET status = ? WHERE id = ?".into(), "DELETE FROM sessions WHERE expires_at < now()".into()];
    let mut billing = profile("billing-mssql", Engine::SqlServer, Environment::Production, McpAccess::Read);
    billing.mcp_rules.view = AgentView::Structure;
    let mut mongo = profile("orders-mongo", Engine::MongoDb, Environment::Production, McpAccess::Write);
    let r = &mut mongo.mcp_rules;
    r.view = AgentView::Masked;
    (r.structure, r.code) = (false, false);
    (r.time_limit_s, r.read_from) = (Some(30), Some(ReadFrom::SecondaryPreferred));
    mongo.mcp_row_limit = Some(1000);
    let mut redis = profile("cache-redis", Engine::Redis, Environment::Dev, McpAccess::Full);
    let r = &mut redis.mcp_rules;
    (r.redis.keyspace, r.redis.scripting, r.redis.pubsub) = (false, false, false);
    (r.time_limit_s, r.read_rows, r.read_bytes) = (Some(10), Some(500), Some(16 * 1024));
    redis.mcp_row_limit = Some(100);
    let all = [
        pg,
        profile("analytics-mysql", Engine::MySql, Environment::Staging, McpAccess::Read),
        billing,
        mongo,
        profile("search-es", Engine::Elasticsearch, Environment::None, McpAccess::Read),
        profile("logs-opensearch", Engine::OpenSearch, Environment::None, McpAccess::Read),
        redis,
    ];
    for p in all {
        if !core.config.connections().iter().any(|x| x.name == p.name) {
            let _ = core.config.upsert(p);
        }
    }
    let _ = core.settings.update(|s| {
        let mut cust = CustomType::new("Customer code", r"\bCUST-[0-9]{6}\b", Mask::Hash);
        (cust.id, cust.on) = ("t_cust01".into(), false);
        let mut badge = CustomType::new("Employee badge", r"\bEMP[0-9]{5}\b", Mask::Hidden);
        badge.id = "t_badge1".into();
        s.agent_types.custom = vec![cust, badge];
    });
}

/// The calls of the mockups, newest first, at their distances from now.
fn seed_audit(core: &Core) {
    let now = chrono::Utc::now();
    let note = |d: Decision, reasons: &[&str]| Some(ApprovalNote { decision: d, by: Some("bruno".into()), reasons: reasons.iter().map(|s| s.to_string()).collect(), rows: None });
    let seen = |columns: Option<u64>, masked: &[(&str, Mask)], values: &[(&str, u64)]| {
        Some(Seen { columns, masked: masked.iter().map(|(c, m)| (c.to_string(), *m)).collect(), values: values.iter().map(|(k, n)| (k.to_string(), *n)).collect() })
    };
    type Row = (i64, &'static str, &'static str, &'static str, &'static str, Option<u64>, f64, Outcome, Option<&'static str>, Option<ApprovalNote>, Option<Seen>);
    let rows: Vec<Row> = vec![
        (2400, "Cursor", "search-es", "nosql_query", "GET _cat/indices", Some(6), 5., Outcome::Ok, None, None, None),
        (1980, "Claude Code", "orders-mongo", "nosql_query", "db.customers.find({ city: \"Recife\" }).limit(20)", Some(20), 8., Outcome::Ok, None, None, None),
        (1614, "Codex", "cache-redis", "nosql_query", "KEYS *", None, 0.4, Outcome::Refused, Some("refused: KEYS never runs for agents"), None, None),
        (1530, "Codex", "cache-redis", "nosql_query", "SCAN 0 MATCH cart:* COUNT 100", Some(100), 3., Outcome::Ok, None, None, None),
        (1140, "Cursor", "analytics-mysql", "run_query", "SELECT * FROM events ORDER BY id DESC LIMIT 100", Some(100), 64., Outcome::Ok, None, None, seen(Some(9), &[], &[])),
        (750, "Claude Code", "prod-pg", "execute", "DELETE FROM sessions WHERE expires_at < now()", None, 2104., Outcome::Refused, Some("refused: denied in Dataman (18,204 rows, over the 1k limit); rolled back"), note(Decision::Denied, &["18,204 rows, over the 1k limit"]), None),
        (360, "Cursor", "search-es", "nosql_query", "GET products/_search {\"query\":{\"match\":{\"name\":\"dock\"}}}", Some(20), 17., Outcome::Ok, None, None, None),
        (240, "Cursor", "analytics-mysql", "run_query", "SELECT date(created_at), count(*) FROM events GROUP BY 1", Some(30), 210., Outcome::Ok, None, None, seen(Some(2), &[], &[])),
        (113, "Claude Code", "orders-mongo", "nosql_query", "db.orders.aggregate([{ $match: { status: \"paid\" } }, { $group: { _id: \"$customer\", total: { $sum: \"$total\" } } }])", Some(500), 91., Outcome::Ok, None, None, seen(None, &[("customer.email", Mask::Partial)], &[])),
        (77, "Claude Code", "prod-pg", "run_query", "SELECT id, name, email, cpf,\n       city, total\n  FROM customers\n WHERE city = 'Recife'\n LIMIT 40;", Some(40), 41., Outcome::Ok, None, None, seen(Some(6), &[("email", Mask::Partial), ("cpf", Mask::Hash)], &[("E-mail", 3)])),
        (42, "Claude Code", "prod-pg", "execute", "INSERT INTO users (email) VALUES ($1)", None, 6., Outcome::Error, Some("23505 duplicate key value violates unique constraint \"users_email_key\" · Key (email)=(‹value›) already exists"), None, None),
        (24, "Claude Code", "prod-pg", "run_query", "SELECT upper(password) FROM users LIMIT 5", None, 0.3, Outcome::Refused, Some("refused: users.password is hidden from agents on prod-pg"), None, None),
        (12, "Claude Code", "prod-pg", "execute", "UPDATE orders SET status = $1 WHERE id = $2", Some(1), 12., Outcome::Ok, None, note(Decision::Approved, &["production"]), None),
    ];
    let audit = Audit::new(core.dirs.mcp_audit_file());
    for (ago, client, conn, tool, sql, rows, ms, outcome, error, approval, seen) in rows {
        audit.write(&Entry {
            ts: now - chrono::Duration::seconds(ago),
            client: client.into(),
            transport: "stdio".into(),
            tool: tool.into(),
            connection: Some(conn.into()),
            database: None,
            sql: Some(sql.into()),
            query: Some(sql.into()),
            rows,
            ms,
            outcome,
            error: error.map(str::to_string),
            approval,
            draft: false,
            favorite: None,
            allowed_by: None,
            rewritten: (sql.starts_with("SELECT id, name, email")).then(|| "SELECT id, name, email, cpf, city, total FROM (SELECT id, name, email, cpf, city, total, created_at FROM public.customers) customers WHERE city = ? LIMIT ?".into()),
            seen,
        });
    }
}

fn agents(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Entity<AgentsPage> {
    cx.update(|_, cx| ws.read(cx).agents.clone()).unwrap()
}

fn id_of(name: &str, core: &Core) -> Option<String> {
    core.config.connections().into_iter().find(|p| p.name == name).map(|p| p.id)
}

/// `agents-home`, `agents-audit`, `agents-permissions[-mongo|-redis]`, `agents-privacy`,
/// `agents-types[-conn]`, `agents-empty`: the page as the mockups draw it.
pub fn is_demo(stage: &str) -> bool {
    stage.starts_with("agents-") && stage != "agents-check"
}

pub fn demo(ws: &Entity<Workspace>, stage: &str, window: &mut gpui::Window, cx: &mut gpui::App) {
    let core = ws.read(cx).store.read(cx).core.clone();
    seed_profiles(&core);
    if stage != "agents-empty" {
        seed_audit(&core);
    }
    ws.update(cx, |w, cx| w.store.update(cx, |s, cx| s.reload(cx)));
    let page = match stage {
        s if s.starts_with("agents-audit") => Page::Audit,
        s if s.starts_with("agents-permissions") => Page::Permissions,
        s if s.starts_with("agents-privacy") => Page::Privacy,
        s if s.starts_with("agents-types") => Page::Types,
        _ => Page::Home,
    };
    let conn = match stage {
        "agents-permissions-mongo" => "orders-mongo",
        "agents-permissions-redis" => "cache-redis",
        _ => "prod-pg",
    };
    let id = id_of(conn, &core);
    ws.update(cx, |w, cx| {
        w.show_agents(page, window, cx);
        w.agents.update(cx, |a, cx| {
            a.conn = id.clone();
            if page == Page::Audit {
                a.selected = Some(3);
            }
            if stage == "agents-types-conn" {
                a.scope = id.clone();
            }
            if page == Page::Types {
                a.editor.name.update(cx, |i, cx| i.set_text("Customer code", cx));
                a.editor.regex.update(cx, |i, cx| i.set_text(r"\bCUST-[0-9]{6}\b", cx));
                a.editor.mask = Mask::Hash;
                a.editor.samples = ["Pedido de CUST-004211 enviado", "merge CUST-000017 into CUST-000930", "ref CUST-42", "xCUST-123456"].map(String::from).to_vec();
            }
            cx.notify();
        });
    });
    if page == Page::Types {
        let regex = ws.read(cx).agents.read(cx).editor.regex.clone();
        window.focus(&gpui::Focusable::focus_handle(regex.read(cx), cx), cx);
    }
}

fn rules(core: &Core, id: &str) -> dataman_core::privacy::AgentRules {
    core.config.get(id).map(|p| p.mcp_rules).unwrap_or_default()
}

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (pg, pw) = crate::check::profile(Engine::Postgres);
    let up = connect(ws, &pg, &pw, cx).await;
    r.check("PostgreSQL connects", up.is_ok(), format!("{up:?}"));
    let core = core(ws, cx);
    seed_profiles(&core);
    seed_audit(&core);
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.store.update(cx, |s, cx| s.reload(cx))));
    let a = agents(ws, cx);
    let page = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).page).unwrap_or(Page::Home);
    let shows = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).view == dataman_core::layout::NavView::Agents).unwrap_or(false);

    r.check("the robot sits at the foot of the rail", probed("rail-agents", cx).is_some(), "");
    click_probe("rail-agents", cx);
    let loaded = until(cx, 5, |cx| cx.update(|_, cx| a.read(cx).entries.len() >= 13).unwrap_or(false)).await;
    r.check("a click on it opens the Agents area on Home, with the agents' calls", shows(cx) && page(cx) == Page::Home && loaded, format!("{:?}", page(cx)));
    let connected = cx.update(|_, cx| a.read(cx).connected_count()).unwrap_or(0);
    r.check("Home: two agents seen in the last minutes (Codex is idle)", connected == 2, connected);
    let cards = cx.update(|_, cx| crate::agents::agents_of(&a.read(cx).entries).into_iter().map(|c| (c.name, crate::agents::ago(c.last))).collect::<Vec<_>>()).unwrap_or_default();
    r.check(
        "the agent cards: Claude Code seconds ago, Cursor minutes ago, Codex idle",
        cards.len() == 3 && cards[0].0 == "Claude Code" && cards[0].1.ends_with("s ago") && cards[1].1.ends_with("min ago") && cards[2].1 == "idle",
        format!("{cards:?}"),
    );
    let words = cx.update(|_, cx| a.read(cx).entries.iter().take(3).map(crate::agents::outcome_word).collect::<Vec<_>>()).unwrap_or_default();
    r.check(
        "outcomes: allowed, refused · hidden column (in the error color), an error",
        words.first().is_some_and(|w| w.0 == "allowed" && !w.1) && words.get(1).is_some_and(|w| w.0 == "refused · hidden column" && w.1) && words.get(2).is_some_and(|w| w.1),
        format!("{words:?}"),
    );
    click_probe("agents-home-row-3", cx);
    pause(cx, 100).await;
    let sel = cx.update(|_, cx| a.read(cx).selected).ok().flatten();
    r.check("a statement's row opens it in Audit", page(cx) == Page::Audit && sel == Some(3), format!("{:?} {sel:?}", page(cx)));
    let note = cx.update(|_, cx| crate::agents::audit_note(&a.read(cx).entries[3])).unwrap_or_default();
    r.check("its note: rows · columns · masked", note.0 == "40 rows · 6 columns · 2 masked" && !note.1, format!("{note:?}"));
    let decision = cx.update(|_, cx| crate::agents::decision_text(&a.read(cx).entries[3])).unwrap_or_default();
    r.check("its decision: Ran · read, no question", decision == "Ran · read, no question", &decision);
    let denied = cx.update(|_, cx| crate::agents::audit_note(&a.read(cx).entries[7])).unwrap_or_default();
    r.check("a denial reads as denied by you, in the error color", denied.0.ends_with("denied by you") && denied.1, format!("{denied:?}"));

    let shown = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).filtered(cx).len()).unwrap_or(0);
    let all = shown(cx);
    click_probe("agents-f-outcome", cx);
    pause(cx, 100).await;
    let labels = cx.update(|_, cx| a.read(cx).demo_menu_labels()).unwrap_or_default();
    r.check("the outcome filter opens its menu", labels.first().map(String::as_str) == Some("Any outcome"), format!("{labels:?}"));
    click_probe("agents-f-outcome:2", cx);
    pause(cx, 100).await;
    let refused = shown(cx);
    r.check("Refused or denied: the two refusals and the denial", refused == 3 && all == 13, format!("{refused} of {all}"));
    click_probe("agents-f-agent", cx);
    pause(cx, 100).await;
    let codex = cx.update(|_, cx| a.read(cx).demo_menu_labels().iter().position(|l| l == "Codex")).ok().flatten().unwrap_or(0);
    click_probe(&format!("agents-f-agent:{codex}"), cx);
    pause(cx, 100).await;
    r.check("… and Codex: KEYS * alone", shown(cx) == 1, shown(cx));
    click_probe("agents-f-outcome", cx);
    click_probe("agents-f-outcome:0", cx);
    click_probe("agents-f-agent", cx);
    click_probe("agents-f-agent:0", cx);
    click_probe("agents-field-search", cx);
    typ(cx, "aggregate");
    pause(cx, 50).await;
    r.check("Search narrows to the statements that contain it", shown(cx) == 1, shown(cx));
    key(cx, "cmd-a");
    key(cx, "backspace");
    pause(cx, 50).await;
    r.check("an empty search shows everything again", shown(cx) == all, shown(cx));

    // Permissions, on the PostgreSQL connection the check saved.
    click_probe("agents-nav-permissions", cx);
    pause(cx, 100).await;
    r.check("the nav opens Permissions", page(cx) == Page::Permissions, format!("{:?}", page(cx)));
    click_probe(&format!("agents-conn-{}", pg.name), cx);
    pause(cx, 50).await;
    click_probe("agents-may-structure", cx);
    pause(cx, 50).await;
    r.check("Agents may › Change structure, off: saved on the connection", !rules(&core, &pg.id).structure, format!("{:?}", rules(&core, &pg.id)));
    click_probe("agents-time-1", cx);
    click_probe("agents-rrows-2", cx);
    pause(cx, 50).await;
    let rr = rules(&core, &pg.id);
    r.check("Limits: 30 s per call, 100 rows per read", rr.time_limit_s == Some(30) && rr.read_rows == Some(100), format!("{:?} {:?}", rr.time_limit_s, rr.read_rows));
    click_probe("agents-tier-2", cx);
    pause(cx, 50).await;
    let tier = core.config.get(&pg.id).map(|p| p.mcp_access());
    r.check("MCP: Write", tier == Some(McpAccess::Write), format!("{tier:?}"));
    let mongo = id_of("orders-mongo", &core).unwrap_or_default();
    click_probe("agents-conn-orders-mongo", cx);
    pause(cx, 50).await;
    r.check("MongoDB has Read from", probed("agents-read-from-0", cx).is_some(), "");
    click_probe("agents-read-from-2", cx);
    pause(cx, 50).await;
    r.check("Read from › Secondary is saved", rules(&core, &mongo).read_from == Some(ReadFrom::Secondary), format!("{:?}", rules(&core, &mongo).read_from));
    let redis = id_of("cache-redis", &core).unwrap_or_default();
    click_probe("agents-conn-cache-redis", cx);
    pause(cx, 50).await;
    click_probe("agents-may-scripting", cx);
    click_probe("agents-may-dangerous", cx);
    pause(cx, 50).await;
    let rd = rules(&core, &redis).redis;
    r.check("Redis: @scripting back on; @dangerous cannot be turned on", rd.scripting && !rd.allows(dataman_core::privacy::RedisCat::Dangerous), format!("{rd:?}"));

    // Privacy.
    click_probe("agents-nav-privacy", cx);
    click_probe(&format!("agents-conn-{}", pg.name), cx);
    pause(cx, 50).await;
    click_probe("agents-view-1", cx);
    pause(cx, 50).await;
    let pr = rules(&core, &pg.id);
    r.check("The agent sees › Masked data: saved, and server activity is forced off", pr.view == AgentView::Masked && pr.activity_forced_off() && !pr.activity_on(), format!("{:?}", pr.view));
    click_probe("agents-field-visible", cx);
    typ(cx, "public.payments");
    click_probe("agents-vis-add", cx);
    click_probe("agents-field-visible", cx);
    typ(cx, "users.password_hash");
    key(cx, "enter");
    pause(cx, 50).await;
    let items = rules(&core, &pg.id).visible.items;
    r.check(
        "Visible to agents: public.payments hidden as a table, users.password_hash as a column",
        items.iter().any(|i| i.name == "public.payments" && i.kind == ItemKind::Table) && items.iter().any(|i| i.name == "users.password_hash" && i.kind == ItemKind::Column),
        format!("{items:?}"),
    );
    click_probe("agents-field-sensitive", cx);
    typ(cx, "customer.email");
    click_probe("agents-sens-add", cx);
    pause(cx, 50).await;
    let sens = rules(&core, &pg.id).sensitive;
    r.check("Sensitive columns: customer.email added, Partial, on", sens.iter().any(|s| s.col == "customer.email" && s.how == Mask::Partial && s.on), format!("{sens:?}"));
    click_probe("agents-sens-mask-0", cx);
    pause(cx, 100).await;
    click_probe("agents-sens-mask-0:2", cx);
    pause(cx, 50).await;
    r.check("its mask menu sets Hash", rules(&core, &pg.id).sensitive.first().is_some_and(|s| s.how == Mask::Hash), "");

    // Sensitive data.
    click_probe("agents-nav-types", cx);
    pause(cx, 100).await;
    r.check("Sensitive data lists the built-in and custom types", cx.update(|_, cx| a.read(cx).type_rows(cx).len()).unwrap_or(0) == dataman_core::privacy::BUILTINS.len() + 2, "");
    click_probe("agents-field-name", cx);
    typ(cx, "Order ref");
    click_probe("agents-field-regex", cx);
    typ(cx, r"\bORD-[0-9]{4}\b");
    for line in ["paid ORD-1234 and ORD-9876", "ORD-12 is short", "nothing here"] {
        click_probe("agents-field-sample", cx);
        typ(cx, line);
        key(cx, "enter");
    }
    pause(cx, 50).await;
    let tested = cx
        .update(|_, cx| {
            let a = a.read(cx);
            let c = a.drafted(cx);
            let samples: Vec<&str> = a.editor.samples.iter().map(String::as_str).collect();
            dataman_core::privacy::test_pattern(&c, &samples, &a.key).ok().map(|t| (t.matches, t.lines.iter().map(|l| (l.1.len(), l.2.clone())).collect::<Vec<_>>()))
        })
        .ok()
        .flatten();
    r.check(
        "the test highlights both matches on the first line and masks them; the others don't match",
        tested.as_ref().is_some_and(|(n, lines)| *n == 2 && lines[0].0 == 2 && lines[0].1.as_deref().is_some_and(|o| o.matches("‹order ref›").count() == 2) && lines[1].1.is_none()),
        format!("{tested:?}"),
    );
    click_probe("agents-type-save", cx);
    pause(cx, 50).await;
    let saved = core.settings.get().agent_types.custom.iter().find(|c| c.name == "Order ref").cloned();
    r.check("Add saves it for every connection", saved.as_ref().is_some_and(|c| c.pattern == r"\bORD-[0-9]{4}\b" && c.on), format!("{saved:?}"));
    click_probe("agents-field-regex", cx);
    typ(cx, "(?=x)");
    pause(cx, 50).await;
    let problem = cx.update(|_, cx| dataman_core::privacy::check_pattern(&a.read(cx).drafted(cx).pattern).err()).ok().flatten();
    r.check("look-around is refused with the editor's message", problem.as_deref() == Some("Look-around isn't supported."), format!("{problem:?}"));
    click_probe("agents-type-cancel", cx);

    click_probe("agents-scope", cx);
    pause(cx, 100).await;
    let at = cx.update(|_, cx| a.read(cx).demo_menu_labels().iter().position(|l| *l == pg.name)).ok().flatten().unwrap_or(0);
    click_probe(&format!("agents-scope:{at}"), cx);
    pause(cx, 50).await;
    click_probe("agents-type-switch-phone", cx);
    pause(cx, 50).await;
    let types = rules(&core, &pg.id).types;
    let row = cx.update(|_, cx| a.read(cx).type_rows(cx).into_iter().find(|t| t.id == "phone")).ok().flatten();
    r.check(
        "in a connection: Phone switched off there only, tagged changed here",
        types.get("phone") == Some(&false) && row.as_ref().is_some_and(|t| !t.on && t.changed_here) && core.settings.get().agent_types.global_on("phone") == Some(true),
        format!("{types:?}"),
    );
    click_probe("agents-reset", cx);
    pause(cx, 50).await;
    r.check("Reset to global clears the connection's switches", rules(&core, &pg.id).types.is_empty(), format!("{:?}", rules(&core, &pg.id).types));

    click_probe("agents-nav-audit", cx);
    pause(cx, 50).await;
    let rail = probed("rail-agents", cx);
    if let Some(b) = rail {
        click(cx, centre(b));
    }
    pause(cx, 100).await;
    r.check("the robot again goes back to the explorer and the editor", !shows(cx), "");
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.toggle_mcp_log(window, cx)));
    pause(cx, 50).await;
    r.check("the MCP log command opens Agents › Audit", shows(cx) && page(cx) == Page::Audit, format!("{:?}", page(cx)));
}
