//! `agents-check`: the Agents area through its own controls (the rail's robot, the nav, filters,
//! segments, checkboxes, fields and menus), with what each writes read back from the saved
//! connection and settings. The `agents-*` demo stages show its pages for screenshots.


use dataman_core::config::Engine;
use dataman_core::privacy::{AgentView, CustomType, ItemKind, Mask, ReadFrom, SensitiveCol, VisibleItem};
use dataman_core::{ConnectionProfile, Core, Environment, McpAccess};
use dataman_core::audit::{ApprovalNote, Audit, Decision, Entry, Outcome, Seen};
use gpui::{AsyncWindowContext, Entity};

use crate::agents::{AgentsPage, Page};
use crate::app::Workspace;
use crate::check::*;
use std::time::Duration;

fn profile(name: &str, engine: Engine, env: Environment, tier: McpAccess) -> ConnectionProfile {
    let mut p = ConnectionProfile::new(name);
    p.engine = engine;
    p.port = engine.default_port();
    p.env = env;
    p.mcp = Some(tier);
    p
}

fn seed_profiles(core: &Core) {
    let mut pg = profile("prod-pg", Engine::Postgres, Environment::Production, McpAccess::Write);
    let r = &mut pg.mcp_rules;
    r.view = if std::env::var("DATAMAN_SHOT_STAGE").is_ok_and(|s| s.starts_with("agents-privacy-data")) { AgentView::Data } else { AgentView::Masked };
    for (col, how, on) in [("users.email", Mask::PARTIAL, true), ("customers.ssn", Mask::Hidden, true), ("customers.doc", Mask::Hidden, true), ("customers.phone", Mask::Partial(dataman_core::privacy::Keep::new(dataman_core::privacy::Side::First, 2)), true), ("orders.notes", Mask::Hidden, true)] {
        r.sensitive.push(SensitiveCol { col: col.into(), how, on });
    }
    for (name, kind) in [("public.payments", ItemKind::Table), ("audit.*", ItemKind::Schema), ("users.password_hash", ItemKind::Column), ("users.mfa_secret", ItemKind::Column)] {
        r.visible.items.push(VisibleItem { name: name.into(), kind });
    }
    r.structure = false;
    r.activity = false;
    (r.time_limit_s, r.read_rows) = (Some(30), Some(100));
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
    (r.time_limit_s, r.read_rows) = (Some(10), Some(500));
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
        let mut cust = CustomType::new("Customer code", r"\bCUST-[0-9]{6}\b", Mask::Hidden);
        (cust.id, cust.on) = ("t_cust01".into(), false);
        let mut badge = CustomType::new("Employee badge", r"\bEMP[0-9]{5}\b", Mask::Hidden);
        badge.id = "t_badge1".into();
        s.agent_types.custom = vec![cust, badge];
    });
}

fn seed_audit(core: &Core) {
    let now = chrono::Utc::now();
    let note = |d: Decision, reasons: &[&str]| Some(ApprovalNote { decision: d, by: Some("bruno".into()), reasons: reasons.iter().map(|s| s.to_string()).collect(), rows: None });
    let seen = |columns: Option<u64>, masked: &[(&str, Mask)], values: &[(&str, u64)]| {
        Some(Seen { columns, masked: masked.iter().map(|(c, m)| (c.to_string(), *m)).collect(), values: values.iter().map(|(k, n)| (k.to_string(), *n)).collect() })
    };
    type Row = (i64, &'static str, &'static str, &'static str, &'static str, Option<u64>, f64, Outcome, Option<&'static str>, Option<ApprovalNote>, Option<Seen>);
    let rows: Vec<Row> = vec![
        (2400, "Cursor", "search-es", "nosql_query", "GET _cat/indices", Some(6), 5., Outcome::Ok, None, None, None),
        (1980, "Claude Code", "orders-mongo", "nosql_query", "db.customers.find({ city: \"Austin\" }).limit(20)", Some(20), 8., Outcome::Ok, None, None, None),
        (1614, "Codex", "cache-redis", "nosql_query", "KEYS *", None, 0.4, Outcome::Refused, Some("refused: KEYS never runs for agents"), None, None),
        (1530, "Codex", "cache-redis", "nosql_query", "SCAN 0 MATCH cart:* COUNT 100", Some(100), 3., Outcome::Ok, None, None, None),
        (1140, "Cursor", "analytics-mysql", "run_query", "SELECT * FROM events ORDER BY id DESC LIMIT 100", Some(100), 64., Outcome::Ok, None, None, seen(Some(9), &[], &[])),
        (750, "Claude Code", "prod-pg", "execute", "DELETE FROM sessions WHERE expires_at < now()", None, 2104., Outcome::Refused, Some("refused: denied in Dataman (18,204 rows, over the 1k limit); rolled back"), note(Decision::Denied, &["18,204 rows, over the 1k limit"]), None),
        (360, "Cursor", "search-es", "nosql_query", "GET products/_search {\"query\":{\"match\":{\"name\":\"dock\"}}}", Some(20), 17., Outcome::Ok, None, None, None),
        (240, "Cursor", "analytics-mysql", "run_query", "SELECT date(created_at), count(*) FROM events GROUP BY 1", Some(30), 210., Outcome::Ok, None, None, seen(Some(2), &[], &[])),
        (113, "Claude Code", "orders-mongo", "nosql_query", "db.orders.aggregate([{ $match: { status: \"paid\" } }, { $group: { _id: \"$customer\", total: { $sum: \"$total\" } } }])", Some(500), 91., Outcome::Ok, None, None, seen(None, &[("customer.email", Mask::PARTIAL)], &[])),
        (77, "Claude Code", "prod-pg", "run_query", "SELECT id, name, email, ssn,\n       city, total\n  FROM customers\n WHERE city = 'Austin'\n LIMIT 40;", Some(40), 41., Outcome::Ok, None, None, seen(Some(6), &[("email", Mask::PARTIAL), ("ssn", Mask::Hidden)], &[("E-mail", 3)])),
        (42, "Claude Code", "prod-pg", "execute", "INSERT INTO users (email) VALUES ($1)", None, 6., Outcome::Error, Some("23505 duplicate key value violates unique constraint \"users_email_key\" · Key (email)=(‹value›) already exists"), None, None),
        (24, "Claude Code", "prod-pg", "run_query", "SELECT upper(password) FROM users LIMIT 5", None, 0.3, Outcome::Refused, Some("refused: users.password is hidden from agents on prod-pg"), None, None),
        (12, "Claude Code", "prod-pg", "execute", "UPDATE orders SET status = $1 WHERE id = $2", Some(1), 12., Outcome::Ok, None, note(Decision::Approved, &["production"]), None),
    ];
    let audit = Audit::new(core.dirs.mcp_audit_file());
    for (ago, client, conn, tool, sql, rows, ms, outcome, error, approval, seen) in rows {
        let id = format!("demo-{ago}");
        if let Some(r) = demo_reply(ago) {
            audit.write_reply(&id, &r.to_string());
        }
        audit.write(&Entry {
            id: Some(id),
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
            rewritten: (sql.starts_with("SELECT id, name, email")).then(|| "SELECT id, name, email, ssn, city, total FROM (SELECT id, name, email, ssn, city, total, created_at FROM public.customers) customers WHERE city = ? LIMIT ?".into()),
            seen,
            login: None,
        });
    }
}

fn demo_reply(ago: i64) -> Option<serde_json::Value> {
    use serde_json::json;
    Some(match ago {
        77 => json!({
            "columns": [["id", "int4"], ["name", "text"], ["email", "text"], ["ssn", "text"], ["city", "text"], ["total", "numeric"]],
            "rows": [
                [1042, "Ana Souza", "••••••••.com", "‹hidden›", "Austin", "129.90"],
                [1043, "Bruno Lima", "••••••••••.com", "‹hidden›", "Austin", "88.00"],
                [1051, "Carla Dias", "••••••••••.com", "‹hidden›", "Austin", "412.35"],
                [1067, "Davi Rocha", "••••••••p.io", "‹hidden›", "Austin", "57.10"],
                [1070, "Elisa Melo", "••••••••••.com", "‹hidden›", "Austin", "230.00"],
                [1088, "Fábio Reis", "••••••••••.com", "‹hidden›", "Austin", "19.99"]
            ],
            "row_count": 40, "masked_columns": ["email (partial)", "ssn (hash)"], "masked": {"E-mail": 3}, "ms": 41.0
        }),
        113 => json!({"count": 3, "documents": [
            {"_id": "c-301", "total": 1820.5, "customer": {"name": "Ana Souza", "email": "a•••@acme.com"}},
            {"_id": "c-118", "total": 990.0, "customer": {"name": "Bruno Lima", "email": "b•••@mail.com"}},
            {"_id": "c-207", "total": 744.9, "customer": {"name": "Carla Dias", "email": "c•••@acme.com"}}
        ], "more": true, "ms": 91.0}),
        12 => json!({"committed": true, "statements": [["UPDATE", 1]], "approved": {"by": "bruno", "reasons": ["production"]}, "ms": 12.0}),
        240 => json!({"columns": [["date", "date"], ["count", "int8"]], "rows": [["2026-10-18", 4120], ["2026-10-19", 3987], ["2026-10-20", 1311]], "row_count": 30, "ms": 210.0}),
        _ => return None,
    })
}

fn agents(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Entity<AgentsPage> {
    cx.update(|_, cx| ws.read(cx).agents.clone()).unwrap()
}

fn id_of(name: &str, core: &Core) -> Option<String> {
    core.config.connections().into_iter().find(|p| p.name == name).map(|p| p.id)
}

/// `agents-audit[-zoom|-zooming|-wide]`, `agents-privacy-data`, `agents-general[-import]`, `agents-permissions[-mongo|-redis]`, `agents-privacy`,
/// `agents-protection-copy`, `agents-protection-login[-drift|-changes|-checks]`, `agents-types[-conn|-card]`, `agents-empty`: the page as the mockups draw it (`-card`: Card number open in
/// the editor; `agents-privacy`: Protection scrolled to Privacy; `-login`: prod-pg with an agent login, `-drift` with its rules changed since, `-changes` its View changes open, `-checks` View checks open with one failed).
pub fn is_demo(stage: &str) -> bool {
    stage.starts_with("agents-") && stage != "agents-check"
}

pub fn demo(ws: &Entity<Workspace>, stage: &str, window: &mut gpui::Window, cx: &mut gpui::App) {
    let core = ws.read(cx).store.read(cx).core.clone();
    seed_profiles(&core);
    if stage != "agents-empty" {
        seed_audit(&core);
    }
    if stage.starts_with("agents-protection-login") {
        demo_login(&core, stage.ends_with("-drift") || stage.ends_with("-changes"));
    }
    if stage == "agents-protection-copy" {
        // prod-pg copied from a connection with no rules: what loosens shows.
        let _ = core.config.upsert(profile("staging-pg", Engine::Postgres, Environment::Staging, McpAccess::Full));
    }
    ws.update(cx, |w, cx| w.store.update(cx, |s, cx| s.reload(cx)));
    let page = match stage {
        s if s.starts_with("agents-audit") => Page::Audit,
        s if s.starts_with("agents-permissions") || s.starts_with("agents-privacy") || s.starts_with("agents-protection") => Page::Protection,
        s if s.starts_with("agents-types") => Page::Types,
        s if s.starts_with("agents-mcp") || s.starts_with("agents-general") => Page::General,
        s if s.starts_with("agents-login") => Page::Protection,
        _ => Page::Audit,
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
                if stage == "agents-audit-wide" {
                    a.detail_w = 5000.;
                }
            }
            if stage.starts_with("agents-privacy") {
                a.jump = Some(crate::agents_rules::Jump::Privacy);
            }
            if stage.starts_with("agents-protection-login") {
                a.jump = Some(crate::agents_rules::Jump::Login);
            }
            if matches!(stage, "agents-protection-login-drift" | "agents-protection-login-changes")
                && let Some(p) = id.as_ref().and_then(|id| core.config.get(id))
            {
                let lines = [(false, "GRANT SELECT ON public.audit_log TO dataman_agent"), (true, "REVOKE SELECT ON public.audit_log FROM dataman_agent"), (true, "GRANT SELECT ON dataman_agent_views.users TO dataman_agent")];
                a.demo_drift(&p, lines.iter().map(|(add, l)| dataman_core::agent_login::Change { add: *add, line: l.to_string() }).collect());
                if stage.ends_with("-changes") {
                    a.demo_changes();
                }
            }
            if stage == "agents-protection-login-checks"
                && let Some(p) = id.as_ref().and_then(|id| core.config.get(id))
            {
                a.demo_checks(&p);
            }
            if stage == "agents-protection-copy"
                && let Some(from) = id_of("staging-pg", &core)
            {
                a.open_copy(from, window, cx);
            }
            if stage == "agents-types-card" {
                a.select_type("card", cx);
            } else if page == Page::Types {
                a.editor.name.update(cx, |i, cx| i.set_text("Customer code", cx));
                a.editor.regex.update(cx, |i, cx| i.set_text(r"\bCUST-[0-9]{6}\b", cx));
                a.editor.mask = Mask::Hidden;
                a.editor.samples = ["Order CUST-004211 shipped", "merge CUST-000017 into CUST-000930", "ref CUST-42", "xCUST-123456"].map(String::from).to_vec();
            }
            cx.notify();
        });
    });
    if page == Page::Types {
        let regex = ws.read(cx).agents.read(cx).editor.regex.clone();
        window.focus(&gpui::Focusable::focus_handle(regex.read(cx), cx), cx);
    }
    if stage == "agents-general-import" {
        // A file that gives agents more on prod-pg, and knows a connection this Mac doesn't.
        let mut f = dataman_core::agents_config::export(&core.settings.get(), &core.config.connections());
        if let Some(c) = f.connections.iter_mut().find(|c| c.name == "prod-pg") {
            (c.mcp, c.rules.view, c.yolo) = (Some(McpAccess::Full), AgentView::Data, true);
            c.rules.sensitive.retain(|s| s.col != "users.email");
            c.rules.read_rows = Some(500);
            c.allowed.push("DELETE FROM sessions WHERE expires_at < now()".into());
        }
        f.sensitive_data.set_builtin("phone", Some(false), None);
        f.connections.push(dataman_core::agents_config::ConnRules { id: "elsewhere".into(), name: "warehouse".into(), engine: Engine::MySql, database: "dw".into(), mcp: None, row_limit: None, allowed: Vec::new(), yolo: false, rules: Default::default() });
        let text = dataman_core::agents_config::to_text(&f);
        ws.read(cx).agents.clone().update(cx, |a, cx| a.open_import(std::path::Path::new("dataman-agents.toml"), Ok(text), cx));
    }
    if stage == "agents-audit-zoom" || stage == "agents-audit-zooming" {
        // Zooms once the reply is laid out, at once or on its way (DATAMAN_ZOOM_FREEZE holds it
        // part of the way).
        let (a, at_once) = (ws.read(cx).agents.clone(), stage == "agents-audit-zoom");
        window
            .spawn(cx, async move |cx| {
                until(cx, 5, |cx| cx.update(|_, cx| a.read(cx).received_rows.is_some()).unwrap_or(false)).await;
                pause(cx, 300).await;
                let _ = cx.update(|window, cx| {
                    a.update(cx, |a, cx| {
                        a.toggle_received_zoom(window, cx);
                        if at_once {
                            a.received_tween = None;
                        }
                    })
                });
            })
            .detach();
    }
}

/// A click on a Protection control, scrolled into view first: the page runs longer than a window.
async fn tap(a: &Entity<AgentsPage>, id: &str, cx: &mut AsyncWindowContext) -> bool {
    if let Some(b) = probed(id, cx).filter(|_| !id.contains(':')) {
        let moved = cx
            .update(|_, cx| {
                let s = a.read(cx).protection_scroll.clone();
                let view = s.bounds();
                if b.left() < view.left() || (b.top() >= view.top() && b.bottom() <= view.bottom()) {
                    return false;
                }
                let o = s.offset();
                s.set_offset(gpui::point(o.x, (o.y - (b.top() - view.top()) + gpui::px(120.)).max(-s.max_offset().y).min(gpui::px(0.))));
                true
            })
            .unwrap_or(false);
        if moved {
            pause(cx, 50).await;
        }
    }
    click_probe(id, cx)
}

/// How many items a popup's menu holds (none while it is disabled): opened by a click, shut after.
async fn menu_len(a: &Entity<AgentsPage>, id: &str, cx: &mut AsyncWindowContext) -> usize {
    tap(a, id, cx).await;
    pause(cx, 100).await;
    let n = (0..10).filter(|k| probed(&format!("{id}:{k}"), cx).is_some()).count();
    let _ = cx.update(|_, cx| {
        a.update(cx, |a, cx| {
            a.menu = None;
            cx.notify();
        })
    });
    pause(cx, 50).await;
    n
}

/// Picks the item at `k` of a popup's menu: a click on it, then on the item.
async fn pick(a: &Entity<AgentsPage>, cx: &mut AsyncWindowContext, id: &str, k: usize) {
    tap(a, id, cx).await;
    pause(cx, 100).await;
    click_probe(&format!("{id}:{k}"), cx);
    pause(cx, 50).await;
}

/// prod-pg with an agent login, its rules unchanged since or (`drift`) not.
fn demo_login(core: &Core, drift: bool) {
    use dataman_core::agent_login;
    let Some(mut p) = core.config.connections().into_iter().find(|p| p.name == "prod-pg") else { return };
    let rules = if drift { "changed".to_string() } else { agent_login::rules_hash(&p, &p.database) };
    // The drift stages show the changes waiting for Apply.
    p.agent_login = Some(login_of(&p, rules, !drift));
    let _ = core.config.upsert(p);
}

fn login_of(p: &ConnectionProfile, rules: String, auto_sync: bool) -> dataman_core::agent_login::AgentLogin {
    use dataman_core::agent_login::{self, AgentLogin};
    let now = chrono::Utc::now();
    AgentLogin {
        user: agent_login::login_name(&p.id),
        server: String::new(),
        created: now - chrono::Duration::days(7),
        synced: now,
        verified: Some(now),
        rules,
        plan: String::new(),
        rotate_days: Some(30),
        rotated: None,
        conn_limit: Some(4),
        auto_sync,
        database: p.database.clone(),
        made: Vec::new(),
        views: Vec::new(),
        granted: 12,
        not_granted: 0,
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
    let page = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).page).unwrap_or(Page::Audit);
    let shows = |cx: &mut AsyncWindowContext| cx.update(|_, cx| ws.read(cx).view == dataman_core::layout::NavView::Agents).unwrap_or(false);

    r.check("the robot sits at the foot of the rail", probed("rail-agents", cx).is_some(), "");
    click_probe("rail-agents", cx);
    let loaded = until(cx, 5, |cx| cx.update(|_, cx| a.read(cx).entries.len() >= 13).unwrap_or(false)).await;
    r.check("a click on it opens the Agents area on Protection, with the agents' calls loaded", shows(cx) && page(cx) == Page::Protection && loaded, format!("{:?}", page(cx)));
    let pages: Vec<&str> = Page::NAV.iter().map(|p| p.label()).collect();
    r.check(
        "the area's pages: General first, then Protection, Audit, Masks (no Home; the agent login is inside Protection)",
        pages == ["General", "Protection", "Audit", "Masks"] && probed("agents-nav-login", cx).is_none(),
        format!("{pages:?}"),
    );
    // Protection is one page: Access, then Privacy, under one bar; no tabs.
    click_probe("agents-nav-protection", cx);
    pause(cx, 100).await;
    let (bar, access, privacy) = (probed("agents-protection-conn", cx), probed("agents-access", cx), probed("agents-privacy", cx));
    r.check(
        "Protection is one page: the connection over it, Access then Privacy, no tabs and no Agents may",
        page(cx) == Page::Protection
            && bar.zip(access).is_some_and(|(b, a)| a.top() > b.bottom())
            && access.zip(privacy).is_some_and(|(a, p)| p.top() > a.bottom())
            && ["agents-protection-tab-0", "agents-protection-tab-1", "agents-nav-privacy", "agents-may-structure", "agents-may-code", "agents-may-activity"].iter().all(|id| probed(id, cx).is_none()),
        format!("{bar:?} {access:?} {privacy:?}"),
    );
    click_probe("agents-nav-audit", cx);
    pause(cx, 50).await;
    let when = cx.update(|_, cx| a.read(cx).f_when).ok();
    r.check("Audit opens on all time, so older calls show at once", when == Some(crate::agents::When::All), format!("{when:?}"));
    click_probe("agents-audit-row-3", cx);
    pause(cx, 100).await;
    let sel = cx.update(|_, cx| a.read(cx).selected).ok().flatten();
    r.check("a click selects a call", page(cx) == Page::Audit && sel == Some(3), format!("{:?} {sel:?}", page(cx)));
    let note = cx.update(|_, cx| a.read(cx).entries.get(3).map(crate::agents::audit_note)).ok().flatten().unwrap_or_default();
    r.check("its note: rows · columns · masked", note.0 == "40 rows · 6 columns · 2 masked" && !note.1, format!("{note:?}"));
    let decision = cx.update(|_, cx| a.read(cx).entries.get(3).map(crate::agents::decision_text)).ok().flatten().unwrap_or_default();
    r.check("its decision: Ran · read, no question", decision == "Ran · read, no question", &decision);
    let kept = |cx: &mut AsyncWindowContext, i: usize| {
        cx.update(|_, cx| {
            let a = a.read(cx);
            a.entries.get(i).and_then(|e| e.id.clone()).and_then(|id| a.replies.get(&id).cloned())
        })
        .ok()
        .flatten()
    };
    let read = |k: &Option<crate::agents::Kept>| matches!(k, Some(crate::agents::Kept::Reply { .. }));
    let text = |k: &Option<crate::agents::Kept>| match k {
        Some(crate::agents::Kept::Reply { text, .. }) => text.to_string(),
        _ => String::new(),
    };
    until(cx, 5, |cx| read(&kept(cx, 3))).await;
    let reply = text(&kept(cx, 3));
    r.check("its detail shows what the agent received, read from the log on disk: the rows, masked as sent", reply.contains("••••••••.com") && reply.contains("‹hidden›") && !reply.contains("ana@"), &reply);
    // A restart: nothing in memory, the entries read again.
    let _ = cx.update(|_, cx| {
        a.update(cx, |a, cx| {
            a.forget_replies(cx);
            a.reload(cx);
        })
    });
    let again = until(cx, 5, |cx| read(&kept(cx, 3))).await;
    r.check("after a restart the reply is still there", again && text(&kept(cx, 3)) == reply, "");
    let gone = cx.update(|_, cx| a.read(cx).entries.iter().position(|e| e.id.as_deref() == Some("demo-2400"))).ok().flatten().unwrap_or(0);
    let _ = cx.update(|_, cx| {
        a.update(cx, |a, cx| {
            a.selected = Some(gone);
            cx.notify();
        })
    });
    let rotated = until(cx, 5, |cx| matches!(kept(cx, gone), Some(crate::agents::Kept::RotatedOut))).await;
    r.check("a reply no longer in the log says it rotated out", rotated, format!("{:?}", kept(cx, gone)));
    let _ = cx.update(|_, cx| {
        a.update(cx, |a, cx| {
            a.selected = Some(3);
            cx.notify();
        })
    });
    until(cx, 5, |cx| cx.update(|_, cx| a.read(cx).received_rows.is_some()).unwrap_or(false)).await;
    let rows = cx.update(|_, cx| a.read(cx).received_rows).ok().flatten();
    r.check("the reply is in the results grid: 6 rows shown of the 40 sent", rows == Some((6, 40)), format!("{rows:?}"));
    let zoom = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).received_zoom).unwrap_or(false);
    // Each step once the panel has landed: a click mid-move lands where the button was.
    let still = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).received_tween.is_none()).unwrap_or(false);
    click_probe("agents-recv-zoom", cx);
    r.check("its zoom button takes the whole Audit page", until(cx, 2, |cx| zoom(cx)).await, "");
    until(cx, 2, still).await;
    key(cx, "escape");
    r.check("Esc leaves the zoom, as in a result", until(cx, 2, |cx| !zoom(cx)).await, "");
    until(cx, 2, still).await;
    key(cx, "cmd-shift-enter");
    r.check("the result's zoom shortcut zooms it again", until(cx, 2, |cx| zoom(cx)).await, "");
    until(cx, 2, still).await;
    click_probe("agents-recv-zoom", cx);
    r.check("… and the button restores it", until(cx, 2, |cx| !zoom(cx)).await, "");
    until(cx, 2, still).await;
    pause(cx, 350).await;
    r.check("it has no close: always there while a call with rows is selected", probed("agents-recv-close", cx).is_none() && probed("agents-recv", cx).is_some(), "");
    // The zoom moves like a result's: the panel grows from its place in the detail over the page
    // and shrinks back, each way part of the way after 60 ms, landed after the zoom's time.
    let now = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).received_now()).unwrap_or(-1.);
    let moving = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).received_tween.is_some()).unwrap_or(true);
    let motion = !cx.update(|_, cx| cx.reduce_motion()).unwrap_or(false);
    for (zoom_in, name) in [(true, "zooming in"), (false, "zooming out")] {
        let before = probed("agents-recv", cx);
        let t0 = std::time::Instant::now();
        click_probe("agents-recv-zoom", cx);
        // The first frame on its way, not a fixed pause: a slow renderer's frames come late.
        let start = if zoom_in { 0. } else { 1. };
        until(cx, 1, |cx| (now(cx) - start).abs() > 0.05 || !moving(cx)).await;
        let (mid, mid_b) = (now(cx), probed("agents-recv", cx));
        let in_time = zoom_mid_possible(t0);
        pause(cx, 320).await;
        let after = probed("agents-recv", cx);
        let end = if zoom_in { 1. } else { 0. };
        let between = |m: f32, x: f32, y: f32| (x.min(y) + 1. ..x.max(y) - 1.).contains(&m);
        let grew = before.zip(mid_b).zip(after).is_some_and(|((b, m), a)| between(m.top().as_f32(), b.top().as_f32(), a.top().as_f32()) && between(m.size.width.as_f32(), b.size.width.as_f32(), a.size.width.as_f32()));
        r.check(
            &format!("{name}, the reply moves part of the way, then lands"),
            (!motion || !in_time || (mid > 0.001 && mid < 0.999 && grew)) && now(cx) == end && !moving(cx),
            format!("mid {mid} end {} {before:?} → {mid_b:?} → {after:?}{}", now(cx), if in_time { "" } else { " (frames too slow for a mid-way sample)" }),
        );
    }

    // The reply's row numbers as narrow as their digits: no editor beside it to line up with.
    let (gutter, digit) = cx.update(|_, cx| (a.read(cx).received.read(cx).demo_gutter_w(), a.read(cx).received.read(cx).demo_char_w() * 11. / 12.)).unwrap_or((0., 0.));
    r.check("the reply's row numbers take one digit and a little padding", (gutter - (digit + 18.)).abs() < 0.5 && gutter < 30., format!("{gutter} for a {digit} digit"));

    // The seam between the calls and the detail: dragged, held between the detail's narrowest
    // and half the page; a double click restores it; the session keeps it.
    let shown = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).detail_shown()).unwrap_or(0.);
    let page_w = cx.update(|_, cx| a.read(cx).demo_audit_w()).unwrap_or(0.);
    let seam = probed("agents-detail-seam", cx);
    r.check("Audit has a seam between the calls and the detail, at its default width", seam.is_some() && shown(cx) == crate::agents::DETAIL_W, format!("{seam:?} {}", shown(cx)));
    let drag = |cx: &mut AsyncWindowContext, to: f32, count: usize| {
        let Some(b) = probed("agents-detail-seam", cx) else { return };
        let (from, y) = (centre(b).x, centre(b).y);
        move_to(cx, centre(b));
        input(cx, gpui::PlatformInput::MouseDown(gpui::MouseDownEvent { button: gpui::MouseButton::Left, position: gpui::point(from, y), modifiers: Default::default(), click_count: count, first_mouse: false }));
        for k in 1..=4 {
            let x = from + (gpui::px(to) - from) * (k as f32 / 4.);
            input(cx, gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent { position: gpui::point(x, y), pressed_button: Some(gpui::MouseButton::Left), modifiers: Default::default() }));
        }
        input(cx, gpui::PlatformInput::MouseUp(gpui::MouseUpEvent { button: gpui::MouseButton::Left, position: gpui::point(gpui::px(to), y), modifiers: Default::default(), click_count: count }));
    };
    let x0 = seam.map_or(0., |b| centre(b).x.as_f32());
    drag(cx, x0 - 80., 1);
    r.check("dragging it left widens the detail by as much", (shown(cx) - (crate::agents::DETAIL_W + 80.)).abs() < 1., format!("{}", shown(cx)));
    drag(cx, 0., 1);
    r.check("… up to half the page", (shown(cx) - page_w / 2.).abs() < 1., format!("{} of {page_w}", shown(cx)));
    drag(cx, x0 + 2000., 1);
    r.check("dragging right stops at the detail's narrowest", shown(cx) == crate::agents::DETAIL_MIN, format!("{}", shown(cx)));
    let _ = cx.update(|_, cx| a.update(cx, |a, _| a.detail_w = 5000.));
    r.check("a width saved on a wider window still shows at most half the page", (shown(cx) - page_w / 2.).abs() < 1., format!("{} of {page_w}", shown(cx)));
    let seam = probed("agents-detail-seam", cx);
    if let Some(b) = seam {
        click_with(cx, centre(b), gpui::MouseButton::Left, Default::default(), 2);
    }
    let dragging = cx.update(|_, cx| a.read(cx).detail_drag).unwrap_or(true);
    r.check("a double click on the seam restores the default", shown(cx) == crate::agents::DETAIL_W && !dragging, format!("{}", shown(cx)));
    drag(cx, x0 - 60., 1);
    let saved = cx.update(|_, cx| ws.update(cx, |w, cx| w.session_snapshot(cx).0.audit_detail_w)).unwrap_or(0.);
    r.check("the session keeps where the seam was left", (saved - (crate::agents::DETAIL_W + 60.)).abs() < 1., format!("{saved}"));
    let back = probed("agents-recv-zoom", cx);
    if let Some(b) = back {
        click(cx, centre(b));
    }
    // The zoom tweens over frames: waited for, so a slow renderer sees its end too.
    let mut zoomed = false;
    for _ in 0..40 {
        pause(cx, 100).await;
        draw(cx);
        zoomed = cx.update(|_, cx| a.read(cx).received_zoom && a.read(cx).received_tween.is_none()).unwrap_or(false);
        if zoomed {
            break;
        }
    }
    let area = cx.update(|_, cx| a.read(cx).demo_received_area_w()).unwrap_or(0.);
    r.check("the zoom still takes the whole page over the wider detail", zoomed && area > page_w - 1., format!("{area} of {page_w}"));
    if let Some(b) = probed("agents-recv-zoom", cx) {
        click(cx, centre(b));
    }
    pause(cx, 400).await;
    let _ = cx.update(|_, cx| a.update(cx, |a, _| a.detail_w = crate::agents::DETAIL_W));
    let older = chrono::Utc::now() - chrono::Duration::days(3);
    let days = (crate::agents::day_label(chrono::Utc::now()), crate::agents::day_label(older));
    r.check("Audit groups calls by day: Today, then the weekday and date", days.0 == "Today" && days.1.contains(','), format!("{days:?}"));
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
    click_probe("agents-field-search", cx);
    typ(cx, "nothing-like-this");
    pause(cx, 50).await;
    click_probe("agents-reset-filters", cx);
    pause(cx, 50).await;
    r.check("Nothing matches offers Reset filters, which brings every call back", shown(cx) == all, shown(cx));
    // The lists answer the keys as the explorer and history do.
    click_probe("agents-audit-row-0", cx);
    pause(cx, 50).await;
    let sel = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).selected).ok().flatten();
    key(cx, "down");
    pause(cx, 50).await;
    let down = sel(cx);
    key(cx, "end");
    pause(cx, 50).await;
    let end = sel(cx);
    key(cx, "home");
    pause(cx, 50).await;
    let home = sel(cx);
    key(cx, "pagedown");
    pause(cx, 50).await;
    let paged = sel(cx);
    r.check(
        "Audit: ↓ selects the next call, End the last, Home the first, PageDown ten down",
        down == Some(1) && end == Some(all - 1) && home == Some(0) && paged == Some(10.min(all - 1)),
        format!("{down:?} {end:?} {home:?} {paged:?}"),
    );
    key(cx, "up");
    pause(cx, 50).await;
    r.check("… and ↑ the one before", sel(cx) == Some(9.min(all - 2)), format!("{:?}", sel(cx)));

    tap(&a, "agents-nav-protection", cx).await;
    pause(cx, 100).await;
    r.check("the nav opens Protection", page(cx) == Page::Protection, format!("{:?}", page(cx)));
    tap(&a, &format!("agents-conn-{}", pg.name), cx).await;
    pause(cx, 50).await;
    let order: Vec<String> = cx.update(|_, cx| a.read(cx).profiles(cx).into_iter().map(|p| p.id).collect()).unwrap_or_default();
    let at = order.iter().position(|i| *i == pg.id).unwrap_or(0);
    let conn = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).conn.clone()).ok().flatten();
    key(cx, "down");
    pause(cx, 50).await;
    let down = conn(cx);
    key(cx, "up");
    pause(cx, 50).await;
    let up = conn(cx);
    r.check(
        "Protection: ↓ and ↑ move through the connections, and the page follows",
        down.as_ref() == order.get((at + 1).min(order.len() - 1)) && up.as_deref() == Some(pg.id.as_str()),
        format!("{down:?} {up:?}"),
    );
    tap(&a, "agents-tier-1", cx).await;
    pause(cx, 50).await;
    let shut = menu_len(&a, "agents-wrows", cx).await;
    r.check("on Read, Rows per write is disabled: a click opens nothing", shut == 0, shut);
    tap(&a, "agents-tier-3", cx).await;
    pause(cx, 50).await;
    let counts = (menu_len(&a, "agents-time", cx).await, menu_len(&a, "agents-wrows", cx).await, menu_len(&a, "agents-rrows", cx).await, probed("agents-rbytes", cx).is_some());
    let unset = rules(&core, &pg.id);
    r.check(
        "Limits are three popups showing real values only (no Default item), each list holding its default and ending on ∞; no Size per read",
        counts == (5, 5, 5, false)
            && dataman_core::privacy::WRITE_ROWS.contains(&dataman_core::settings::MCP_WRITE_ROW_LIMIT)
            && dataman_core::privacy::READ_ROWS.contains(&dataman_core::settings::MCP_MAX_ROWS)
            && [dataman_core::privacy::TIME_LIMITS[4], dataman_core::privacy::WRITE_ROWS[4], dataman_core::privacy::READ_ROWS[4]] == [dataman_core::privacy::UNLIMITED; 3]
            && unset.read_rows.is_none(),
        format!("{counts:?}"),
    );
    pick(&a, cx, "agents-rrows", 4).await;
    pick(&a, cx, "agents-wrows", 4).await;
    let unlimited = (rules(&core, &pg.id).read_rows, core.config.get(&pg.id).and_then(|p| p.mcp_row_limit));
    r.check("∞ is saved as no limit (0), for reads and for writes", unlimited == (Some(0), Some(0)), format!("{unlimited:?}"));
    pick(&a, cx, "agents-wrows", 1).await;
    pause(cx, 50).await;
    pick(&a, cx, "agents-time", 1).await;
    pick(&a, cx, "agents-rrows", 1).await;
    pause(cx, 50).await;
    let rr = rules(&core, &pg.id);
    r.check("Limits: 30 s per call, 100 rows per read", rr.time_limit_s == Some(30) && rr.read_rows == Some(100), format!("{:?} {:?}", rr.time_limit_s, rr.read_rows));
    pick(&a, cx, "agents-rrows", 3).await;
    pause(cx, 50).await;
    r.check("picking the default (1k) stores nothing: the connection follows the default again", rules(&core, &pg.id).read_rows.is_none(), format!("{:?}", rules(&core, &pg.id).read_rows));
    pick(&a, cx, "agents-rrows", 1).await;
    pause(cx, 50).await;
    tap(&a, "agents-tier-2", cx).await;
    pause(cx, 50).await;
    let tier = core.config.get(&pg.id).map(|p| p.mcp_access());
    r.check("MCP: Write", tier == Some(McpAccess::Write), format!("{tier:?}"));
    let mongo = id_of("orders-mongo", &core).unwrap_or_default();
    tap(&a, "agents-conn-orders-mongo", cx).await;
    pause(cx, 50).await;
    r.check("MongoDB has Read from", probed("agents-read-from", cx).is_some(), "");
    pick(&a, cx, "agents-read-from", 2).await;
    r.check("Read from › Secondary is saved", rules(&core, &mongo).read_from == Some(ReadFrom::Secondary), format!("{:?}", rules(&core, &mongo).read_from));
    let redis = id_of("cache-redis", &core).unwrap_or_default();
    tap(&a, "agents-conn-cache-redis", cx).await;
    pause(cx, 50).await;
    let rd = rules(&core, &redis).redis;
    r.check("Redis: no command categories on the page; the rules are kept as they were", probed("agents-may-scripting", cx).is_none() && probed("agents-may-dangerous", cx).is_none() && !rd.scripting && !rd.allows(dataman_core::privacy::RedisCat::Dangerous), format!("{rd:?}"));

    // Without an agent login: a card above Access, its ? saying what one does.
    tap(&a, &format!("agents-conn-{}", pg.name), cx).await;
    pause(cx, 50).await;
    let (card, access) = (probed("agents-login-section", cx), probed("agents-access", cx));
    let tip = crate::agents_login::login_tip().text();
    r.check(
        "Protection: without an agent login, How agents reach sits above Access, with a ? that says what a login does",
        card.zip(access).is_some_and(|(c, a)| c.bottom() < a.top()) && probed("agents-login-tip", cx).is_some() && tip.contains(dataman_core::secret::STORE_NAME) && tip.contains("refuses what the rules refuse"),
        format!("{card:?} {access:?}"),
    );
    r.check("…the chain agent → rules → the database as your login, and Create agent login; no key in the list", probed("agents-login-chain", cx).is_some() && probed("agents-login-create", cx).is_some() && probed(&format!("agents-conn-key-{}", pg.name), cx).is_none() && probed("agents-nav-login", cx).is_none(), "");
    // A server that only speaks PostgreSQL's protocol (MotherDuck, by its host): no logins to
    // make, said plainly, without connecting to it.
    let mut md = profile("motherduck-pg", Engine::Postgres, Environment::None, McpAccess::Read);
    md.host = "pg.us-east-1-aws.motherduck.com".into();
    let md_id = md.id.clone();
    let _ = core.config.upsert(md);
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.store.update(cx, |s, cx| s.reload(cx))));
    pause(cx, 50).await;
    tap(&a, "agents-conn-motherduck-pg", cx).await;
    pause(cx, 100).await;
    let opened = cx.update(|_, cx| ws.read(cx).store.read(cx).connection(&md_id).is_some()).unwrap_or(true);
    r.check(
        "MotherDuck: the section says the server has no logins of its own and how to narrow it (a read-only token), with no Create and no connection opened",
        probed("agents-login-pgwire", cx).is_some() && probed("agents-login-motherduck", cx).is_some() && probed("agents-login-create", cx).is_none() && probed("agents-login-note", cx).is_none() && !opened,
        format!("opened {opened}"),
    );
    shoot("22-pgwire", cx);
    // Another such server, found by its review (or by asking once where Dataman is connected):
    // the same sentence, without the MotherDuck hint.
    let _ = cx.update(|_, cx| a.update(cx, |a, cx| {
        a.no_logins.insert(pg.id.clone());
        cx.notify();
    }));
    tap(&a, &format!("agents-conn-{}", pg.name), cx).await;
    pause(cx, 100).await;
    r.check("…another PostgreSQL-protocol server without logins: the sentence, no MotherDuck hint, no Create", probed("agents-login-pgwire", cx).is_some() && probed("agents-login-motherduck", cx).is_none() && probed("agents-login-create", cx).is_none(), "");
    let _ = cx.update(|_, cx| a.update(cx, |a, cx| {
        a.no_logins.clear();
        cx.notify();
    }));
    let _ = core.config.remove(&md_id);
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.store.update(cx, |s, cx| s.reload(cx))));
    pause(cx, 50).await;
    tap(&a, "agents-tier-0", cx).await;
    pause(cx, 50).await;
    tap(&a, "agents-tier-2", cx).await;
    pause(cx, 50).await;

    // Copy from…: this engine's other connections; a sheet first; what is one server's stays.
    let mut source = profile("copy-source", Engine::Postgres, Environment::Dev, McpAccess::Full);
    source.mcp_row_limit = Some(1000);
    source.mcp_yolo = true;
    source.agent_login = Some(login_of(&source, "elsewhere".into(), false));
    let s = &mut source.mcp_rules;
    (s.view, s.structure) = (AgentView::Masked, false);
    s.visible.items.push(VisibleItem { name: "no_such_table".into(), kind: ItemKind::Table });
    s.sensitive.push(SensitiveCol { col: "no_such.col".into(), how: Mask::Hidden, on: true });
    let target = profile("copy-target", Engine::Postgres, Environment::Staging, McpAccess::Read);
    let alone = profile("copy-alone", Engine::DuckDb, Environment::None, McpAccess::Read);
    for p in [&source, &target, &alone] {
        let _ = core.config.upsert(p.clone());
    }
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.store.update(cx, |s, cx| s.reload(cx))));
    until(cx, 5, |cx| probed("agents-conn-copy-alone", cx).is_some()).await;
    tap(&a, "agents-conn-copy-alone", cx).await;
    r.check("Copy from…: not offered where no other connection has this engine", until(cx, 5, |cx| probed("agents-protection-conn", cx).is_some() && probed("agents-copy-from", cx).is_none()).await, String::new());
    tap(&a, "agents-conn-copy-target", cx).await;
    until(cx, 5, |cx| probed("agents-copy-from", cx).is_some()).await;
    tap(&a, "agents-copy-from", cx).await;
    until(cx, 5, |cx| probed("agents-copy-from:0", cx).is_some()).await;
    let listed = cx.update(|_, cx| a.read(cx).demo_menu_labels()).unwrap_or_default();
    let is = |n: &str| listed.iter().any(|l| l == n);
    r.check("… it lists the engine's other connections, not this one, nor another engine's", is("copy-source") && is(&pg.name) && !is("copy-target") && !is("copy-alone"), format!("{listed:?}"));
    let at = listed.iter().position(|l| l == "copy-source").unwrap_or(0);
    tap(&a, &format!("agents-copy-from:{at}"), cx).await;
    let up = until(cx, 5, |cx| probed("agents-copy-cancel", cx).is_some()).await;
    r.check(
        "… a sheet first, what loosens flagged, nothing written yet",
        up && probed("agents-copy-loosens", cx).is_some() && probed("agents-copy-change-0", cx).is_some() && core.config.get(&target.id).as_ref() == Some(&target),
        String::new(),
    );
    key(cx, "escape");
    let shut = until(cx, 5, |cx| probed("agents-copy-cancel", cx).is_none()).await;
    r.check("… Esc keeps the connection as it was", shut && core.config.get(&target.id).as_ref() == Some(&target), String::new());
    r.check("… the page has no rule yet", probed("agents-sens-edit-0", cx).is_none(), String::new());
    tap(&a, "agents-copy-from", cx).await;
    until(cx, 5, |cx| probed("agents-copy-from:0", cx).is_some()).await;
    tap(&a, &format!("agents-copy-from:{at}"), cx).await;
    until(cx, 5, |cx| probed("agents-copy-apply", cx).is_some()).await;
    tap(&a, "agents-copy-apply", cx).await;
    let copied = until(cx, 5, |cx| probed("agents-copy-cancel", cx).is_none() && probed("agents-sens-edit-0", cx).is_some()).await;
    let got = core.config.get(&target.id);
    r.check(
        "… Copy writes the source's tier, limits and rules, and the page shows them",
        copied && got.as_ref().is_some_and(|p| p.mcp == source.mcp && p.mcp_row_limit == source.mcp_row_limit && p.mcp_rules == source.mcp_rules),
        format!("{got:?}"),
    );
    r.check("… not its agent login, nor YOLO", got.as_ref().is_some_and(|p| p.agent_login.is_none() && !p.mcp_yolo), format!("{got:?}"));
    r.check("… and the source stays as it was", core.config.get(&source.id).as_ref() == Some(&source), String::new());
    tap(&a, "agents-copy-from", cx).await;
    until(cx, 5, |cx| probed("agents-copy-from:0", cx).is_some()).await;
    tap(&a, &format!("agents-copy-from:{at}"), cx).await;
    let same = until(cx, 5, |cx| probed("agents-copy-same", cx).is_some()).await;
    r.check("… copied again, nothing is left to change", same && probed("agents-copy-apply", cx).is_none(), String::new());
    key(cx, "enter");
    until(cx, 5, |cx| probed("agents-copy-cancel", cx).is_none()).await;
    for p in [&source, &target, &alone] {
        let _ = core.config.remove(&p.id);
    }
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.store.update(cx, |s, cx| s.reload(cx))));
    pause(cx, 50).await;

    tap(&a, &format!("agents-conn-{}", pg.name), cx).await;
    pause(cx, 50).await;
    key(cx, "home");
    pause(cx, 50).await;
    let first = cx.update(|_, cx| a.read(cx).conn.clone()).ok().flatten();
    r.check("Privacy: Home goes to the first connection", first.as_ref() == order.first(), format!("{first:?}"));
    tap(&a, &format!("agents-conn-{}", pg.name), cx).await;
    pause(cx, 50).await;
    // Sensitive columns only mask in Masked data: kept and editable in the other modes, dimmed,
    // with a line that says so and switches to Masked data.
    if let Some(mut p) = core.config.get(&pg.id).filter(|p| p.mcp_rules.sensitive.is_empty()) {
        p.mcp_rules.sensitive.push(SensitiveCol { col: "users.email".into(), how: Mask::PARTIAL, on: true });
        let _ = core.config.upsert(p);
        let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.store.update(cx, |s, cx| s.reload(cx))));
        pause(cx, 50).await;
    }
    let kept = rules(&core, &pg.id).sensitive.clone();
    let mut unused = Vec::new();
    let mut inert = Vec::new();
    for k in [0, 2, 1] {
        tap(&a, &format!("agents-view-{k}"), cx).await;
        pause(cx, 50).await;
        unused.push(probed("agents-sens-unused", cx).is_some());
        if k != 1 {
            // Disabled for real: clicking the field and typing, Add, the × and a mask menu do nothing.
            tap(&a, "agents-field-sensitive", cx).await;
            typ(cx, "users.ssn");
            key(cx, "enter");
            tap(&a, "agents-sens-add", cx).await;
            tap(&a, "agents-sens-edit-0", cx).await;
            pause(cx, 50).await;
            let focused = cx.update(|window, cx| gpui::Focusable::focus_handle(a.read(cx).sens_input.read(cx), cx).is_focused(window)).unwrap_or(true);
            let editing = cx.update(|_, cx| a.read(cx).sens_edit.is_some()).unwrap_or(true);
            inert.push(rules(&core, &pg.id).sensitive == kept && !focused && probed("agents-sens-rm-0", cx).is_none() && !editing);
        }
    }
    r.check("in Data and Structure only, Sensitive columns are disabled: no focus, typing, Add, removal or mask change", inert == [true, true] && !kept.is_empty(), format!("{inert:?} {:?}", rules(&core, &pg.id).sensitive));
    let kept = kept.len();
    tap(&a, "agents-view-0", cx).await;
    pause(cx, 50).await;
    tap(&a, "agents-sens-use-masked", cx).await;
    pause(cx, 50).await;
    let pr = rules(&core, &pg.id);
    r.check(
        "Sensitive columns: a note in Data and Structure only, none in Masked data; its link switches to Masked; the list is kept",
        unused == [true, true, false] && pr.view == AgentView::Masked && probed("agents-sens-unused", cx).is_none() && pr.sensitive.len() == kept,
        format!("{unused:?} {:?} {} of {kept}", pr.view, pr.sensitive.len()),
    );
    tap(&a, "agents-view-1", cx).await;
    pause(cx, 50).await;
    let pr = rules(&core, &pg.id);
    r.check("Agents see › Masked data: saved, and server activity is forced off", pr.view == AgentView::Masked && pr.activity_forced_off() && !pr.activity_on(), format!("{:?}", pr.view));
    tap(&a, "agents-field-visible", cx).await;
    typ(cx, "public.payments");
    tap(&a, "agents-vis-add", cx).await;
    tap(&a, "agents-field-visible", cx).await;
    typ(cx, "users.password_hash");
    key(cx, "enter");
    pause(cx, 50).await;
    let items = rules(&core, &pg.id).visible.items;
    r.check(
        "Table visibility: public.payments hidden as a table, users.password_hash as a column",
        items.iter().any(|i| i.name == "public.payments" && i.kind == ItemKind::Table) && items.iter().any(|i| i.name == "users.password_hash" && i.kind == ItemKind::Column),
        format!("{items:?}"),
    );
    tap(&a, "agents-field-visible", cx).await;
    typ(cx, "pub");
    let listed = until(cx, 5, |cx| cx.update(|_, cx| !a.read(cx).suggest.items.is_empty()).unwrap_or(false)).await;
    let first = cx.update(|_, cx| a.read(cx).suggest.items.first().map(|c| c.text.clone())).ok().flatten();
    r.check("Table visibility suggests as you type (schemas and tables)", listed && first.as_deref().is_some_and(|f| f.starts_with("public")), format!("{first:?}"));
    key(cx, "tab");
    pause(cx, 50).await;
    let taken = cx.update(|_, cx| a.read(cx).vis_input.read(cx).text().to_string()).unwrap_or_default();
    r.check("Tab takes the highlighted suggestion into the field", Some(taken.as_str()) == first.as_deref(), &taken);
    key(cx, "cmd-a");
    key(cx, "backspace");
    let table = cx
        .update(|_, cx| {
            let c = a.read(cx).store.read(cx).connection(&pg.id)?;
            let db = c.database().to_string();
            c.schema.cached_relations(&db, "public")?.into_iter().find(|r| !r.kind.is_view()).map(|r| r.name)
        })
        .ok()
        .flatten()
        .unwrap_or_default();
    // The empty field lists again, above it now (the page's bottom): closed before the click.
    key(cx, "escape");
    pause(cx, 50).await;
    tap(&a, "agents-field-sensitive", cx).await;
    let shown = until(cx, 5, |cx| cx.update(|_, cx| a.read(cx).suggest.which == Some(crate::agents_suggest::Which::Sensitive)).unwrap_or(false)).await;
    let n = cx.update(|_, cx| a.read(cx).suggest.items.len()).unwrap_or(0);
    r.check("Sensitive columns: a click in the empty field lists suggestions at once, 8 at most", shown && (1..=8).contains(&n), n);
    typ(cx, &format!("{table}."));
    let cols = until(cx, 5, |cx| cx.update(|_, cx| a.read(cx).suggest.items.first().is_some_and(|c| c.kind == ItemKind::Column)).unwrap_or(false)).await;
    r.check("Sensitive columns: a table and a dot list its columns", cols, &table);
    key(cx, "escape");
    pause(cx, 50).await;
    r.check("Esc closes the suggestions", cx.update(|_, cx| a.read(cx).suggest.which.is_none()).unwrap_or(false), "");
    key(cx, "cmd-a");
    key(cx, "backspace");
    tap(&a, "agents-field-sensitive", cx).await;
    typ(cx, "customer.email");
    tap(&a, "agents-sens-add", cx).await;
    pause(cx, 50).await;
    let sens = rules(&core, &pg.id).sensitive;
    r.check("Sensitive columns: customer.email added, Redacted (the E-mail Mask covers e-mails: the name suggests nothing), on", sens.iter().any(|s| s.col == "customer.email" && s.how == Mask::Hidden && s.on), format!("{sens:?}"));
    let saved_how = || rules(&core, &pg.id).sensitive.first().map(|s| s.how);
    let editing = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).sens_edit.as_ref().map(|e| (e.col.clone(), e.how))).ok().flatten();
    r.check("the row says its mask in words, with Edit beside it, not menus", probed("agents-sens-edit-0", cx).is_some() && probed("agents-sens-mask-0", cx).is_none(), "");
    tap(&a, "agents-sens-edit-0", cx).await;
    pause(cx, 100).await;
    r.check("Edit opens the column's sheet: Redacted or Partial, then Show the first or last 1–4 characters", editing(cx).is_some_and(|(c, _)| Some(c) == rules(&core, &pg.id).sensitive.first().map(|s| s.col.clone())) && probed("agents-sens-edit-mask-1", cx).is_some() && probed("agents-sens-edit-mask-2", cx).is_none() && probed("agents-sens-edit-side-1", cx).is_some() && probed("agents-sens-edit-side-2", cx).is_none() && probed("agents-sens-edit-count-3", cx).is_some() && probed("agents-sens-edit-count-4", cx).is_none() && probed("agents-sens-edit-short-tip", cx).is_some(), format!("{:?}", editing(cx)));
    tap(&a, "agents-sens-edit-mask-0", cx).await;
    pause(cx, 50).await;
    r.check("Redacted in the sheet: Show the… goes, and nothing is saved yet", editing(cx).is_some_and(|(_, h)| h == Mask::Hidden) && probed("agents-sens-edit-side-0", cx).is_none() && saved_how() == Some(Mask::PARTIAL), "");
    tap(&a, "agents-sens-edit-mask-1", cx).await;
    pause(cx, 50).await;
    tap(&a, "agents-sens-edit-side-0", cx).await;
    pause(cx, 50).await;
    tap(&a, "agents-sens-edit-count-1", cx).await;
    pause(cx, 300).await;
    let picked = Mask::Partial(dataman_core::privacy::Keep::new(dataman_core::privacy::Side::First, 2));
    r.check("the first 2 picked in the sheet", editing(cx).is_some_and(|(_, h)| h == picked), format!("{:?}", editing(cx)));
    let short = crate::agents_sens::summary(picked);
    r.check("the row's words: Partial · first 2", short == "Partial · first 2" && crate::agents_sens::summary(Mask::PARTIAL) == "Partial · last 4", short);
    shoot("sensitive-edit", cx);
    key(cx, "escape");
    pause(cx, 50).await;
    r.check("Esc closes it and keeps the column as it was", editing(cx).is_none() && saved_how() == Some(Mask::PARTIAL), format!("{:?}", saved_how()));
    tap(&a, "agents-sens-edit-0", cx).await;
    pause(cx, 100).await;
    tap(&a, "agents-sens-edit-side-0", cx).await;
    tap(&a, "agents-sens-edit-count-1", cx).await;
    pause(cx, 50).await;
    tap(&a, "agents-sens-edit-save", cx).await;
    pause(cx, 300).await;
    r.check("Save keeps the first 2, and closes the sheet", saved_how() == Some(picked) && editing(cx).is_none(), format!("{:?}", saved_how()));
    let stored = std::fs::read_to_string(core.dirs.connections_file()).unwrap_or_default();
    r.check("…saved as partial:first:2", stored.contains("partial:first:2"), "");
    shoot("sensitive-row", cx);
    // customer.email is in the bench's public schema: its sheet masks a value read from it.
    let connected = cx.update(|_, cx| a.read(cx).store.read(cx).connection(&pg.id).is_some()).unwrap_or(false);
    let k = rules(&core, &pg.id).sensitive.iter().position(|s| s.col == "customer.email").unwrap_or(0);
    tap(&a, &format!("agents-sens-edit-{k}"), cx).await;
    let read = until(cx, 5, |cx| cx.update(|_, cx| a.read(cx).sens_edit.as_ref().is_some_and(|e| e.sample.is_some())).unwrap_or(false)).await;
    pause(cx, 100).await;
    r.check("connected, the sheet shows the mask on a value read from the column", !connected || (read && probed("agents-sens-edit-from", cx).is_some()), format!("connected {connected}, read {read}"));
    shoot("sensitive-real", cx);
    key(cx, "escape");
    pause(cx, 50).await;

    click_probe("agents-nav-masks", cx);
    pause(cx, 100).await;
    r.check("Masks lists the built-in and custom ones, then + to add one", cx.update(|_, cx| a.read(cx).type_rows().len()).unwrap_or(0) == dataman_core::privacy::BUILTINS.len() + 2 && probed("agents-type-add", cx).is_some(), "");
    click_probe("agents-type-row-card", cx);
    pause(cx, 50).await;
    key(cx, "down");
    pause(cx, 50).await;
    let tsel = cx.update(|_, cx| a.read(cx).type_sel).ok().flatten();
    key(cx, "enter");
    pause(cx, 50).await;
    let flipped = core.settings.get().agent_types.is_on("email");
    key(cx, "enter");
    pause(cx, 50).await;
    let back = core.settings.get().agent_types.is_on("email");
    r.check("Masks: ↓ moves to E-mail, Enter switches it off and on", tsel == Some(1) && flipped == Some(false) && back == Some(true), format!("{tsel:?} {flipped:?} {back:?}"));
    let opened = cx.update(|_, cx| (a.read(cx).editor.editing.clone(), a.read(cx).editor.name.read(cx).text().to_string(), a.read(cx).editor.regex.read(cx).text().to_string())).ok();
    let email = dataman_core::privacy::builtin("email").map(|b| b.pattern().to_string());
    r.check(
        "the editor follows the selection: E-mail, a built-in mask, with its name and pattern",
        opened.as_ref().is_some_and(|(id, name, re)| id.as_deref() == Some("email") && name == "E-mail" && Some(re) == email.as_ref()) && probed("agents-editor-title", cx).is_some(),
        format!("{opened:?}"),
    );
    click_probe("agents-type-row-jwt", cx);
    pause(cx, 50).await;
    let _ = cx.update(|_, cx| a.update(cx, |a, cx| a.editor.name.update(cx, |i, cx| i.set_text("JWT token", cx))));
    click_probe("agents-type-save", cx);
    pause(cx, 50).await;
    let types = core.settings.get().agent_types;
    let jwt = dataman_core::privacy::builtin("jwt").map(|b| types.builtin_def(b).name);
    let edited = types.builtin_edited("jwt");
    click_probe("agents-type-delete", cx);
    pause(cx, 50).await;
    click_probe("agents-type-delete-yes", cx);
    pause(cx, 50).await;
    let reset = !core.settings.get().agent_types.builtin_edited("jwt");
    r.check("a click on JWT opens it; Save renames it for every connection, Reset brings the default back", jwt.as_deref() == Some("JWT token") && edited && reset, format!("{jwt:?} {edited} {reset}"));
    click_probe("agents-type-add", cx);
    pause(cx, 50).await;
    let fresh = cx.update(|_, cx| a.read(cx).editor.editing.is_none() && a.read(cx).editor.name.read(cx).text().is_empty()).unwrap_or(false);
    r.check("+ opens an empty New mask", fresh, "");
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
            dataman_core::privacy::test_pattern(&c, &samples).ok().map(|t| (t.matches, t.lines.iter().map(|l| (l.1.len(), l.2.clone())).collect::<Vec<_>>()))
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

    let sees = crate::agents_rules::sees_tip().text();
    r.check(
        "Agents see's tip: nothing masked in Data; in Masked data the types in everything, and the columns below whole on top",
        sees.contains("Data: Rows as stored. Nothing is masked.")
            && sees.contains("Masked data: Values a Mask recognizes (cards, e-mails, keys…) are masked in everything the agent receives. The Sensitive columns below are masked whole, on top of that."),
        sees.clone(),
    );
    let sens = crate::agents_rules::sens_tip().text();
    let types = crate::agents_types::types_tip().text();
    r.check(
        "Sensitive columns' and Masks' tips say where each applies: columns on top of the Masks, Masks on everything in Masked data",
        sens.starts_with("Sensitive columns\nColumns masked in Masked data, in addition to the values Masks already catch.")
            && sens.contains("Partial: Only the first or last 1–4 characters")
            && types == "Masks\nApplied to everything agents get from connections on Masked data."
            && probed("agents-types-tip", cx).is_some()
            && probed("agents-types-applies", cx).is_none(),
        format!("{sens}\n{types}"),
    );
    // One list for every connection: no picker of connections, no reset, and a switch is global.
    let ids: Vec<String> = cx.update(|_, cx| a.read(cx).type_rows().into_iter().map(|t| t.id).collect()).unwrap_or_default();
    r.check("Masks has no CPF or CNPJ, and starts with cards", ids.first().map(String::as_str) == Some("card") && !ids.iter().any(|i| i == "cpf" || i == "cnpj"), format!("{ids:?}"));
    r.check("Masks is one list: no connection picker, no Reset to global", probed("agents-scope", cx).is_none() && probed("agents-reset", cx).is_none(), "");
    click_probe("agents-type-switch-phone", cx);
    pause(cx, 50).await;
    let off = core.settings.get().agent_types.is_on("phone") == Some(false);
    click_probe("agents-type-switch-phone", cx);
    pause(cx, 50).await;
    r.check("Phone's switch turns it off and on for every connection", off && core.settings.get().agent_types.is_on("phone") == Some(true), "");
    let tip = dataman_core::privacy::builtin("phone").map(|b| (b.rule, b.example)).unwrap_or_default();
    r.check("Phone reads US numbers", tip.0.contains("(555) 123-4567") && tip.1.starts_with("(555) 123-4567"), format!("{tip:?}"));

    click_probe("agents-nav-general", cx);
    pause(cx, 100).await;
    r.check("Agents › General: agent access and the command that connects Claude Code", page(cx) == Page::General && probed("agents-mcp-copy", cx).is_some() && probed("agents-mcp-access", cx).is_some(), "");
    click_probe("agents-mcp-access-off", cx);
    pause(cx, 50).await;
    let off = !core.settings.get().mcp_enabled;
    click_probe("agents-mcp-access-on", cx);
    pause(cx, 50).await;
    r.check("its Agent access switch is the global one", off && core.settings.get().mcp_enabled, "");

    // Settings file: Export writes it; Import shows what would change, Cancel first when
    // something loosens; Apply writes it all and logs it in Audit; a bad file changes nothing.
    let dir = std::env::temp_dir().join(format!("dataman-agents-check-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let (out, loose, bad) = (dir.join("agents.toml"), dir.join("loose.toml"), dir.join("bad.toml"));
    crate::sql_picker::set_test_save_path(out.clone());
    click_probe("agents-export", cx);
    // The note comes once the write is done: the file exists before its last byte.
    let written = until(cx, 5, |cx| probed("agents-file-note", cx).is_some()).await && out.exists();
    let text = std::fs::read_to_string(&out).unwrap_or_default();
    r.check(
        "Export writes the agents settings file: General, Sensitive data, each connection; no password or address",
        written && text.contains("version = 1") && text.contains(&format!("id = \"{}\"", pg.id)) && text.contains("sensitive_data") && !text.lines().any(|l| l.trim_start().starts_with("password")) && !text.contains("host =") && !text.contains(&format!("\"{}\"", pg.host)),
        text.lines().take(12).collect::<Vec<_>>().join(" | "),
    );
    let mut f = dataman_core::agents_config::parse(&text).unwrap_or_else(|_| dataman_core::agents_config::export(&core.settings.get(), &core.config.connections()));
    let before = rules(&core, &pg.id).view;
    if let Some(c) = f.connections.iter_mut().find(|c| c.id == pg.id) {
        c.rules.view = AgentView::Data;
    }
    f.connections.push(dataman_core::agents_config::ConnRules { id: "elsewhere".into(), name: "warehouse".into(), engine: Engine::MySql, database: "dw".into(), mcp: None, row_limit: None, allowed: Vec::new(), yolo: false, rules: Default::default() });
    let _ = std::fs::write(&loose, dataman_core::agents_config::to_text(&f));
    let import = |path: &std::path::Path, cx: &mut AsyncWindowContext| {
        crate::sql_picker::set_test_paths(vec![path.to_path_buf()]);
        click_probe("agents-import", cx);
    };
    let open = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).import.is_some()).unwrap_or(false);
    import(&loose, cx);
    until(cx, 5, |cx| open(cx)).await;
    let flags = cx.update(|_, cx| a.read(cx).import.as_ref().map(|i| (i.plan.loosens(), i.plan.not_found.clone()))).ok().flatten();
    r.check(
        "Import shows the summary: the loosening flagged, the connection not on this Mac listed",
        probed("agents-import-loosens", cx).is_some() && probed("agents-import-missing", cx).is_some() && probed("agents-import-apply", cx).is_some() && flags.as_ref().is_some_and(|(l, nf)| *l && nf.len() == 1),
        format!("{flags:?}"),
    );
    key(cx, "enter");
    pause(cx, 50).await;
    r.check("with a loosening, Enter is Cancel: nothing changes", !open(cx) && rules(&core, &pg.id).view == before, format!("{:?}", rules(&core, &pg.id).view));
    import(&loose, cx);
    until(cx, 5, |cx| open(cx)).await;
    click_probe("agents-import-apply", cx);
    pause(cx, 100).await;
    let logged = std::fs::read_to_string(core.dirs.mcp_audit_file()).unwrap_or_default().lines().last().is_some_and(|l| l.contains("import_settings") && l.contains("\"ok\""));
    r.check("Apply writes it and logs the import in Audit", !open(cx) && rules(&core, &pg.id).view == AgentView::Data && logged, format!("{:?} logged {logged}", rules(&core, &pg.id).view));
    import(&out, cx);
    until(cx, 5, |cx| open(cx)).await;
    click_probe("agents-import-apply", cx);
    pause(cx, 100).await;
    r.check("importing the export brings the settings back", rules(&core, &pg.id).view == before, format!("{:?}", rules(&core, &pg.id).view));
    // The summary is what gets written: a change made while it is open (an agent, a project
    // sync, another window) makes Apply write nothing.
    import(&loose, cx);
    until(cx, 5, |cx| open(cx)).await;
    let rows0 = rules(&core, &pg.id).read_rows;
    if let Some(mut p) = core.config.get(&pg.id) {
        p.mcp_rules.read_rows = Some(20);
        let _ = core.config.upsert(p);
    }
    click_probe("agents-import-apply", cx);
    pause(cx, 100).await;
    let note = cx.update(|_, cx| a.read(cx).settings_note.clone()).ok().flatten().unwrap_or_default();
    r.check(
        "a change made while the summary is open: Apply writes nothing and says so",
        !open(cx) && rules(&core, &pg.id).view == before && rules(&core, &pg.id).read_rows == Some(20) && note.contains("changed since"),
        format!("{:?} {note}", rules(&core, &pg.id).view),
    );
    if let Some(mut p) = core.config.get(&pg.id) {
        p.mcp_rules.read_rows = rows0;
        let _ = core.config.upsert(p);
    }
    let mut b = f.clone();
    let mut wrong = CustomType::new("Bad", "(?=x)a", Mask::Hidden);
    wrong.id = "t_bad".into();
    b.sensitive_data.custom.push(wrong);
    let _ = std::fs::write(&bad, dataman_core::agents_config::to_text(&b));
    import(&bad, cx);
    until(cx, 5, |cx| open(cx)).await;
    r.check(
        "a file with an invalid pattern is refused whole: its errors, no Apply",
        probed("agents-import-errors", cx).is_some() && probed("agents-import-apply", cx).is_none() && !core.settings.get().agent_types.custom.iter().any(|c| c.id == "t_bad"),
        "",
    );
    key(cx, "escape");
    pause(cx, 50).await;
    let _ = std::fs::remove_dir_all(&dir);
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

    // No tabs over Agents; the header's × closes it.
    let active = cx.update(|_, cx| ws.read(cx).active).unwrap_or(0);
    let closed = click_probe("agents-close", cx);
    pause(cx, 100).await;
    let now = cx.update(|_, cx| ws.read(cx).active).unwrap_or(9);
    r.check("in Agents, the header's × closes it on the tab that was active", closed && !shows(cx) && now == active, format!("{closed} {now}"));
    let back_to = cx.update(|_, cx| ws.read(cx).view).ok();
    r.check("… and the sidebar is back where it was", back_to == Some(dataman_core::layout::NavView::Explorer), format!("{back_to:?}"));
    click_probe("rail-agents", cx);
    pause(cx, 50).await;
    key(cx, "cmd-t");
    pause(cx, 100).await;
    let tabs = cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap_or(0);
    r.check("⌘T from Agents opens the new tab in front", !shows(cx) && tabs >= 2, tabs);
    click_probe("rail-agents", cx);
    pause(cx, 50).await;
    key(cx, "cmd-1");
    pause(cx, 100).await;
    let first = cx.update(|_, cx| ws.read(cx).active).unwrap_or(9);
    r.check("⌘1 from Agents shows the first tab", !shows(cx) && first == 0, first);
    click_probe("rail-agents", cx);
    pause(cx, 50).await;
    key(cx, "ctrl-tab");
    pause(cx, 100).await;
    let next = cx.update(|_, cx| ws.read(cx).active).unwrap_or(9);
    r.check("⌃Tab from Agents shows the next tab", !shows(cx) && next == 1, next);
    click_probe("rail-agents", cx);
    pause(cx, 50).await;
    click_probe("agents-nav-audit", cx);
    pause(cx, 50).await;
    let before = cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap_or(0);
    key(cx, "home");
    pause(cx, 50).await;
    let state = cx.update(|window, cx| {
        let a = a.read(cx);
        (a.page, a.selected, a.selected.and_then(|i| a.entries.get(i)).map(|e| e.statement().is_some()), gpui::Focusable::focus_handle(a, cx).contains_focused(window, cx))
    });
    key(cx, "enter");
    pause(cx, 150).await;
    let after = cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap_or(0);
    r.check("Audit: Enter opens the selected call in a tab", !shows(cx) && after == before + 1, format!("{before} → {after} {state:?}"));
    check_clear(ws, &a, r, cx).await;
    check_login(ws, &a, &pg, &pw, r, cx).await;
}

fn shoot(name: &str, cx: &mut AsyncWindowContext) {
    let Ok(dir) = std::env::var("DATAMAN_SHOT_DIR") else { return };
    let path = format!("{dir}/{name}.png");
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        crate::bench::capture(window, &path, cx);
    });
}

fn login_ui<T>(a: &Entity<AgentsPage>, cx: &mut AsyncWindowContext, f: impl FnOnce(&crate::agents_login::LoginUi) -> T) -> Option<T> {
    cx.update(|_, cx| f(&a.read(cx).login)).ok()
}

/// Database login, driven through Protection on a real PostgreSQL: Create from the
/// review (the password never in the script, always verified before it is kept), the server's
/// identity as the in-app hub connects, Verify,
/// Rotate (the old password refused), Remove with its SQL (agents refused after), then a
/// connection deleted with its login dropped on the server.
async fn check_login(ws: &Entity<Workspace>, a: &Entity<AgentsPage>, pg: &ConnectionProfile, pw: &str, r: &mut Report, cx: &mut AsyncWindowContext) {
    use dataman_core::agent_login::{self, live};
    use crate::agents_login::Sheet;
    use crate::theme::ActiveTheme;
    let core = core(ws, cx);
    let page = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).page).unwrap_or(Page::Audit);
    let user = agent_login::login_name(&pg.id);
    // A leftover of an earlier run on this server goes first.
    let _ = sql(&core, pg, pw, &format!("DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{user}') THEN EXECUTE 'DROP OWNED BY {user}'; EXECUTE 'DROP ROLE {user}'; END IF; END $$"), cx).await;
    let _ = sql(&core, pg, pw, &format!("DROP SCHEMA IF EXISTS {}_views CASCADE", user), cx).await;
    let _ = sql(&core, pg, pw, "CREATE TABLE IF NOT EXISTS public.au_ui_users (id int PRIMARY KEY, name text, email text, password_hash text)", cx).await;
    let _ = sql(&core, pg, pw, "INSERT INTO public.au_ui_users VALUES (1, 'Ana', 'ana@acme.co', 'h1') ON CONFLICT DO NOTHING", cx).await;
    if let Some(mut p) = core.config.get(&pg.id) {
        p.mcp = Some(McpAccess::Write);
        p.mcp_rules = Default::default();
        p.mcp_rules.view = AgentView::Masked;
        p.mcp_rules.sensitive.push(SensitiveCol { col: "au_ui_users.email".into(), how: Mask::PARTIAL, on: true });
        p.mcp_rules.visible.items.push(VisibleItem { name: "au_ui_users.password_hash".into(), kind: ItemKind::Column });
        (p.agent_login, p.agents_refused) = (None, false);
        let _ = core.config.upsert(p);
    }
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.store.update(cx, |s, cx| s.reload(cx))));
    // A link to the login (from elsewhere in the app) lands on Protection, at its card.
    let id = pg.id.clone();
    let _ = cx.update(|window, cx| {
        ws.update(cx, |w, cx| {
            w.show_agents(Page::Audit, window, cx);
            w.agents.update(cx, |a, cx| a.show_login(&id, cx));
        })
    });
    pause(cx, 100).await;
    let on = cx.update(|_, cx| (a.read(cx).page, a.read(cx).conn.clone())).ok();
    r.check("a link to the database login lands on Protection on the connection, its card in view", on == Some((Page::Protection, Some(pg.id.clone()))) && probed("agents-login-section", cx).is_some(), format!("{on:?}"));
    shoot("01-off", cx);
    let width = cx.update(|window, _| window.viewport_size().width).ok();
    let create = probed("agents-login-create", cx);
    r.check("the section's Create agent login is inside the window", create.zip(width).is_some_and(|(b, w)| b.right() <= w), format!("{create:?} in {width:?}"));
    tap(a, "agents-login-create", cx).await;
    let reviewed = until(cx, 30, |cx| login_ui(a, cx, |l| matches!(l.sheet, Some(Sheet::Create { review: Some(_), .. }))).unwrap_or(false)).await;
    let review = login_ui(a, cx, |l| match &l.sheet {
        Some(Sheet::Create { review: Some(Ok(rv)), .. }) => Ok((rv.plan.shown(), rv.plan.copy(), rv.password.clone(), rv.plan.lacks.clone(), rv.plan.views.len())),
        Some(Sheet::Create { review: Some(Err(e)), .. }) => Err(e.clone()),
        _ => Err("no review".into()),
    })
    .unwrap_or_else(|| Err("gone".into()));
    r.check("Create agent login opens the create modal at once: the plan over what exists now", reviewed && review.is_ok() && page(cx) == Page::Protection, format!("{:?}", review.as_ref().err()));
    if let Ok((shown, copy, password, lacks, views)) = &review {
        r.check("the script never holds the password, shown or as it runs", !shown.contains(password.as_str()) && !copy.contains(password.as_str()), "");
        r.check("…and shows the verifier masked (what runs has the verifier, still not the password)", shown.contains("SCRAM-SHA-256$4096:…") && copy.contains("SCRAM-SHA-256$4096:") && !copy.contains("SCRAM-SHA-256$4096:…"), "");
        r.check("nothing the user's login lacks; the masked table gets its view", lacks.is_empty() && *views == 1, format!("{lacks:?} {views}"));
    }
    // The modal: one column, the SQL under a plain WHAT RUNS and the options under it.
    pause(cx, 100).await;
    let (code, options) = (probed("agents-login-sql", cx), probed("agents-login-options", cx));
    r.check(
        "the modal is one column: the options sit below the SQL, as wide as it",
        code.zip(options).is_some_and(|(c, o)| o.top() >= c.bottom() - gpui::px(1.) && (o.left() - c.left()).abs() < gpui::px(1.) && (o.right() - c.right()).abs() < gpui::px(1.)),
        format!("{code:?} {options:?}"),
    );
    shoot("20-modal", cx);
    // A long line runs on to the right: the code scrolls sideways (a trackpad's swipe).
    let scroll = cx.update(|_, cx| a.read(cx).login.sql_scroll.clone()).ok();
    let wide = scroll.as_ref().map(|s| s.max_offset().x).unwrap_or_default();
    if let Some(c) = code {
        move_to(cx, centre(c));
        input(cx, gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent { position: centre(c), delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(-240.), gpui::px(0.))), modifiers: gpui::Modifiers::default(), touch_phase: gpui::TouchPhase::Moved }));
    }
    pause(cx, 100).await;
    let moved = scroll.as_ref().map(|s| s.offset().x).unwrap_or_default();
    r.check("WHAT RUNS fits the sheet, or a sideways swipe moves it", wide <= gpui::px(0.) || moved < gpui::px(-1.), format!("max {wide:?} at {moved:?}"));
    // Laid out to read: the view a column a line, the CASE a branch a line.
    let display = cx.update(|_, cx| match &a.read(cx).login.sheet {
        Some(Sheet::Create { review: Some(Ok(rv)), .. }) => rv.plan.display(),
        _ => String::new(),
    });
    let display = display.unwrap_or_default();
    r.check("WHAT RUNS breaks long statements into their clauses", display.contains(" AS\nSELECT\n  ") && display.contains("\n  CASE\n    WHEN ") && display.lines().all(|l| l.chars().count() <= 120), &display);
    shoot("21-modal-scrolled", cx);
    input(cx, gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent { position: code.map(centre).unwrap_or_default(), delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(-200.))), modifiers: gpui::Modifiers::default(), touch_phase: gpui::TouchPhase::Moved }));
    pause(cx, 100).await;
    let down = scroll.as_ref().map(|s| s.offset().y).unwrap_or_default();
    r.check("…and still scrolls down", down < gpui::px(-1.), format!("{down:?}"));
    if let Some(s) = &scroll {
        s.set_offset(gpui::Point::default());
    }
    r.check(
        "the modal: no who-enforces column, no password or verify rows, no Copy SQL, no I'll run it myself",
        probed("agents-login-what", cx).is_some() && probed("agents-login-opt-rotate", cx).is_some() && ["agents-login-rules", "agents-login-opt-pw", "agents-login-opt-verify", "agents-login-copy", "agents-login-manual"].iter().all(|id| probed(id, cx).is_none()),
        "",
    );
    let colours = review.as_ref().ok().and_then(|x| cx.update(|_, cx| {
        let t = cx.theme().clone();
        crate::editor::highlighted_lines_in(&t, &x.0, Engine::Postgres).iter().flat_map(|(_, runs)| runs.iter().map(|r| r.color)).filter(|c| *c == t.kw || *c == t.str || *c == t.cm).map(|c| format!("{c:?}")).collect::<std::collections::BTreeSet<_>>().len()
    }).ok());
    r.check("WHAT RUNS is highlighted as the editor colours SQL (keywords, strings, comments)", colours == Some(3), format!("{colours:?}"));
    let sessions = login_ui(a, cx, |l| match &l.sheet {
        Some(Sheet::Create { review: Some(Ok(rv)), .. }) => rv.plan.rules.iter().any(|x| x.rule == format!("At most {} sessions at once", agent_login::CONN_LIMIT) && x.who == agent_login::Who::Database),
        _ => false,
    }).unwrap_or(false);
    r.check("PostgreSQL: at most 4 sessions open at once, held by the database (an option, and a rule the plan holds)", sessions && probed("agents-login-opt-conns-on", cx).is_some(), "");
    pause(cx, 200).await;
    shoot("02-review", cx);
    shoot("11-modal", cx);
    // Esc closes it as Cancel does (nothing made, the page stays); opened again to go on.
    key(cx, "escape");
    pause(cx, 100).await;
    let shut = login_ui(a, cx, |l| l.sheet.is_none()).unwrap_or(false);
    r.check("Esc closes the create modal as Cancel does: nothing made, still on Protection", shut && core.config.get(&pg.id).is_some_and(|p| p.agent_login.is_none()) && page(cx) == Page::Protection, "");
    tap(a, "agents-login-create", cx).await;
    until(cx, 30, |cx| login_ui(a, cx, |l| matches!(l.sheet, Some(Sheet::Create { review: Some(Ok(_)), .. }))).unwrap_or(false)).await;
    pause(cx, 100).await;
    tap(a, "agents-login-go", cx).await;
    let made = until(cx, 30, |_| core.config.get(&pg.id).is_some_and(|p| p.agent_login.is_some())).await;
    let rec = core.config.get(&pg.id).and_then(|p| p.agent_login);
    r.check("Create login: made on the server, verified, kept on the connection", made && rec.as_ref().is_some_and(|l| l.verified.is_some() && l.user == user), format!("{rec:?} {:?}", login_ui(a, cx, |l| l.note.clone())));
    r.check("…always by logging in as it first: its checks ran and held", login_ui(a, cx, |l| !l.checks.is_empty() && l.checks.iter().all(|c| c.ok)).unwrap_or(false), format!("{:?}", login_ui(a, cx, |l| l.checks.len())));
    r.check("…its password in the Keychain (account agent:<id>), never in connections.toml", core.secrets.get(&agent_login::secret_account(&pg.id)).ok().flatten().is_some() && !std::fs::read_to_string(core.dirs.connections_file()).unwrap_or_default().contains(review.as_ref().map(|x| x.2.as_str()).unwrap_or("-")), "");
    r.check("the Agent login section says who agents connect as", until(cx, 5, |cx| probed("agents-login-as", cx).is_some()).await, "");
    let (section, privacy) = (probed("agents-login-section", cx), probed("agents-privacy", cx));
    r.check(
        "…last on the page, under Privacy, with Verify, Rotate password and Remove login…; no card on top, a key in the list",
        page(cx) == Page::Protection
            && section.zip(privacy).is_some_and(|(s, p)| s.top() > p.bottom())
            && ["agents-login-verify", "agents-login-rotate", "agents-login-remove", &format!("agents-conn-key-{}", pg.name)].iter().all(|id| probed(id, cx).is_some())
            && probed("agents-login-chain", cx).is_none(),
        format!("{section:?} {privacy:?}"),
    );
    pause(cx, 200).await;
    shoot("03-created", cx);
    shoot("10-perm-section", cx);
    // Rule changes reach the login on their own (the default), or wait for Apply (off).
    let in_sync = |core: &std::sync::Arc<Core>| core.config.get(&pg.id).is_some_and(|p| p.agent_login.as_ref().is_some_and(|l| l.rules == agent_login::rules_hash(&p, &l.database)));
    let writes_q = format!("SELECT count(*) FROM information_schema.role_table_grants WHERE grantee = '{user}' AND privilege_type IN ('INSERT', 'UPDATE', 'DELETE')");
    let rec = core.config.get(&pg.id).and_then(|p| p.agent_login);
    r.check("Auto sync: on by default, the switch on the login, the dot live", rec.as_ref().is_some_and(|l| l.auto_sync) && probed("agents-login-auto-off", cx).is_some() && probed("agents-login-state-insync", cx).is_some(), format!("{rec:?}"));
    let before = sql1(&core, pg, pw, &writes_q, cx).await;
    let id = pg.id.clone();
    let _ = cx.update(|_, cx| a.update(cx, |a, cx| a.edit_conn(&id, |p| p.mcp = Some(McpAccess::Read), cx)));
    let synced = until(cx, 30, |_| in_sync(&core)).await;
    let after = sql1(&core, pg, pw, &writes_q, cx).await;
    r.check(
        "…so agents turned read-only: the login's writes revoked on the server, no Apply",
        synced && before.as_deref().is_some_and(|n| n != "0") && after.as_deref() == Some("0") && probed("agents-login-apply", cx).is_none(),
        format!("{synced} {before:?} → {after:?} {:?}", login_ui(a, cx, |l| l.note.clone())),
    );
    tap(a, "agents-login-auto", cx).await;
    let off = until(cx, 5, |_| core.config.get(&pg.id).is_some_and(|p| p.agent_login.is_some_and(|l| !l.auto_sync))).await;
    let _ = cx.update(|_, cx| a.update(cx, |a, cx| a.edit_conn(&id, |p| p.mcp = Some(McpAccess::Write), cx)));
    let waits = until(cx, 30, |cx| probed("agents-login-apply", cx).is_some()).await;
    pause(cx, 500).await;
    let still = sql1(&core, pg, pw, &writes_q, cx).await;
    r.check("turned off, a rule change waits for Apply: the login keeps its grants", off && waits && !in_sync(&core) && still.as_deref() == Some("0") && probed("agents-login-state-changes", cx).is_some(), format!("{off} {waits} {still:?}"));
    pause(cx, 200).await;
    shoot("03b-auto-off", cx);
    tap(a, "agents-login-apply", cx).await;
    let applied = until(cx, 30, |_| in_sync(&core)).await;
    let back = sql1(&core, pg, pw, &writes_q, cx).await;
    r.check("…and Apply runs it", applied && back == before, format!("{back:?}"));
    tap(a, "agents-login-auto", cx).await;
    r.check("…on again", until(cx, 5, |_| core.config.get(&pg.id).is_some_and(|p| p.agent_login.is_some_and(|l| l.auto_sync))).await, "");
    // The in-app hub (the agents' tabs and the MCP) connects as the login.
    let p = core.config.get(&pg.id).unwrap_or_else(|| pg.clone());
    let shared = cx.update(|_, cx| crate::mcp::shared(&core, cx)).ok();
    let who = match shared.map(|s| s.hub()) {
        Some(Ok((hub, _))) => {
            let h = hub.handle();
            let p2 = p.clone();
            cx.background_executor()
                .spawn(async move {
                    let c = h.block_on(hub.connect(&p2))?;
                    let rs = c.session(None).execute("SELECT current_user || ' ' || session_user");
                    rs.fetch_all();
                    for _ in 0..400 {
                        if rs.phase().is_finished() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    Ok::<String, String>(rs.snapshot().text(0, 0).map(|t| t.into_owned()).unwrap_or_default())
                })
                .await
        }
        _ => Err("no hub".into()),
    };
    r.check("the in-app hub connects as the agent login (current_user and session_user)", who.as_deref() == Ok(&format!("{user} {user}")), format!("{who:?}"));
    tap(a, "agents-login-verify", cx).await;
    let verified = until(cx, 30, |cx| login_ui(a, cx, |l| l.note.as_ref().is_some_and(|(ok, n)| *ok && n.contains("Verified"))).unwrap_or(false)).await;
    r.check("Verify logs in as it and every check holds", verified && login_ui(a, cx, |l| !l.checks.is_empty() && l.checks.iter().all(|c| c.ok)).unwrap_or(false), format!("{:?}", login_ui(a, cx, |l| (l.note.clone(), l.checks.iter().filter(|c| !c.ok).map(|c| c.what.clone()).collect::<Vec<_>>()))));
    let groups = ["login", "reading", "writing", "privacy", "structure"].iter().filter(|g| probed(&format!("agents-login-checks-{g}"), cx).is_some()).count();
    r.check("Verify opens its checks by category, each with its tip, a lock on what it can't do", login_ui(a, cx, |l| matches!(l.sheet, Some(Sheet::Checks))).unwrap_or(false) && groups >= 3 && probed("agents-login-check-tip-1", cx).is_some() && probed("agents-login-check-lock", cx).is_some(), format!("{groups} categories"));
    pause(cx, 200).await;
    shoot("04b-checks", cx);
    key(cx, "escape");
    pause(cx, 100).await;
    r.check("…closed, the section keeps them in one line, View checks opening them again", probed("agents-login-checks-line", cx).is_some() && probed("agents-login-check-tip-1", cx).is_none(), "");
    tap(a, "agents-login-checks", cx).await;
    r.check("…as it does", probed("agents-login-check-tip-1", cx).is_some(), "");
    key(cx, "escape");
    pause(cx, 100).await;
    pause(cx, 200).await;
    shoot("04-verified", cx);
    let old = core.secrets.get(&agent_login::secret_account(&pg.id)).ok().flatten().unwrap_or_default();
    tap(a, "agents-login-rotate", cx).await;
    let rotated = until(cx, 30, |cx| login_ui(a, cx, |l| l.note.as_ref().is_some_and(|(_, n)| n.contains("New password"))).unwrap_or(false)).await;
    let new = core.secrets.get(&agent_login::secret_account(&pg.id)).ok().flatten().unwrap_or_default();
    r.check("Rotate password: a new one in the Keychain, the old one's sessions ended", rotated && !new.is_empty() && new != old && login_ui(a, cx, |l| l.note.as_ref().is_some_and(|(ok, _)| *ok)).unwrap_or(false), format!("{:?}", login_ui(a, cx, |l| l.note.clone())));
    let mut as_old = live::agent_profile(&p, &user);
    as_old.id = format!("{}-old", p.id);
    let old_in = cx.background_executor().spawn({
        let core = core.clone();
        async move { core.block_on(core.connect(as_old, Some(old))).map(|_| ()).map_err(|e| format!("{e:#}")) }
    }).await;
    r.check("…and the old password no longer logs in", old_in.is_err(), format!("{old_in:?}"));
    pause(cx, 200).await;
    shoot("05-rotated", cx);
    tap(a, "agents-login-remove", cx).await;
    let planned = until(cx, 20, |cx| login_ui(a, cx, |l| matches!(l.sheet, Some(Sheet::Remove { plan: Some(_), .. }))).unwrap_or(false)).await;
    let rm = login_ui(a, cx, |l| match &l.sheet {
        Some(Sheet::Remove { plan: Some(Ok(p)), .. }) => p.shown(),
        _ => String::new(),
    })
    .unwrap_or_default();
    r.check("Remove login… shows the exact SQL first (no password in it)", planned && rm.contains(&format!("DROP ROLE \"{user}\"")) && !rm.contains(&new), &rm);
    r.check("…under a plain WHAT RUNS, with no Copy SQL nor I'll run it myself", probed("agents-login-what", cx).is_some() && probed("agents-login-remove-copy", cx).is_none() && probed("agents-login-remove-manual", cx).is_none(), "");
    let rm_code = probed("agents-login-remove-sql", cx);
    let rm_scroll = cx.update(|_, cx| a.read(cx).login.sql_scroll.clone()).ok();
    r.check("…in one column, the code scrolling as the create sheet's does", rm_code.is_some() && rm_scroll.is_some_and(|s| s.bounds().size.width > gpui::px(0.) && rm_code.is_some_and(|b| (s.bounds().left() - b.left()).abs() < gpui::px(1.))), format!("{rm_code:?}"));
    pause(cx, 200).await;
    shoot("06-remove", cx);
    key(cx, "escape");
    pause(cx, 100).await;
    let shut = login_ui(a, cx, |l| l.sheet.is_none()).unwrap_or(false);
    r.check("Esc closes Remove login as Cancel does: the login stays", shut && core.config.get(&pg.id).is_some_and(|p| p.agent_login.is_some()), "");
    tap(a, "agents-login-remove", cx).await;
    until(cx, 20, |cx| login_ui(a, cx, |l| matches!(l.sheet, Some(Sheet::Remove { plan: Some(Ok(_)), .. }))).unwrap_or(false)).await;
    pause(cx, 100).await;
    tap(a, "agents-login-remove-go", cx).await;
    let gone = until(cx, 30, |_| core.config.get(&pg.id).is_some_and(|p| p.agent_login.is_none() && p.agents_refused)).await;
    r.check("removed: the record and its Keychain item go, agents are refused here", gone && core.secrets.get(&agent_login::secret_account(&pg.id)).ok().flatten().is_none(), "");
    let roles = sql1(&core, pg, pw, &format!("SELECT count(*) FROM pg_roles WHERE rolname = '{user}'"), cx).await;
    r.check("…and the role is gone from the server", roles.as_deref() == Some("0"), format!("{roles:?}"));
    r.check("the refused section offers to let agents use the user's login (explicitly)", until(cx, 5, |cx| probed("agents-login-mine", cx).is_some()).await, "");
    r.check("…and the tier says agents are refused here", probed("agents-login-refused", cx).is_some(), "");
    pause(cx, 200).await;
    shoot("07-refused", cx);

    // A connection deleted with its agent login: the delete sheet offers to drop it (checked).
    let mut other = pg.clone();
    other.id = format!("{}x2", pg.id);
    other.name = "au-ui-delete".into();
    other.agent_login = None;
    let _ = core.save_connection(other.clone(), Some(pw), None);
    let other_user = agent_login::login_name(&other.id);
    let made2 = cx.background_executor().spawn({
        let (core, other) = (core.clone(), other.clone());
        async move {
            let admin = core.block_on(core.connect(other.clone(), None)).map_err(|e| format!("{e:#}"))?;
            let rv = core.block_on(live::review(&admin, &other, &agent_login::Options::default())).map_err(|e| format!("{e:#}"))?;
            if !rv.plan.lacks.is_empty() {
                return Err(format!("{:?}", rv.plan.lacks));
            }
            core.block_on(live::apply(&admin, &rv.plan, &other.database)).map_err(|e| format!("{e:#}"))?;
            core.secrets.set(&agent_login::secret_account(&other.id), &rv.password).map_err(|e| format!("{e:#}"))?;
            let rec = live::record(&other, &rv.plan, &agent_login::Options::default());
            core.config.upsert(ConnectionProfile { agent_login: Some(rec), ..other.clone() }).map_err(|e| format!("{e:#}"))
        }
    }).await;
    r.check("a second connection with its own agent login", made2.is_ok(), format!("{made2:?}"));
    let _ = cx.update(|window, cx| {
        ws.update(cx, |w, cx| {
            w.store.update(cx, |s, cx| s.reload(cx));
            w.ask_delete_connections(vec![other.id.clone()], window, cx);
        })
    });
    let asked = until(cx, 5, |cx| probed("dc-drop-agent", cx).is_some()).await;
    r.check("deleting it asks to also drop its agent login on the server (checked)", asked && cx.update(|_, cx| ws.read(cx).drop_agent_login).unwrap_or(false), "");
    let (row, sheet) = (probed("dc-drop-agent", cx), probed("dc-delete", cx));
    r.check("…its line wraps inside the sheet", row.zip(sheet).is_some_and(|(r, d)| r.right() <= d.right() + gpui::px(1.)), format!("{row:?} {sheet:?}"));
    pause(cx, 200).await;
    shoot("08-delete", cx);
    click_probe("dc-delete", cx);
    let dropped = until(cx, 30, |_| core.config.get(&other.id).is_none()).await;
    let mut left = String::new();
    for _ in 0..60 {
        left = sql1(&core, pg, pw, &format!("SELECT count(*) FROM pg_roles WHERE rolname = '{other_user}'"), cx).await.unwrap_or_default();
        if left == "0" {
            break;
        }
        pause(cx, 250).await;
    }
    r.check("…the connection goes, its login dropped on the server and its Keychain item gone", dropped && left == "0" && core.secrets.get(&agent_login::secret_account(&other.id)).ok().flatten().is_none(), format!("roles left: {left}"));
    pause(cx, 300).await;
    shoot("09-deleted", cx);
}

/// Audit › Clear…: each way asks first with the count of what goes, takes the replies with the
/// calls, and the list, its filters and its selection follow. Last: it leaves the log empty.
async fn check_clear(ws: &Entity<Workspace>, a: &Entity<AgentsPage>, r: &mut Report, cx: &mut AsyncWindowContext) {
    use dataman_core::audit::{Clear, Recent};
    let core = core(ws, cx);
    let log = core.dirs.mcp_audit_file();
    let now = chrono::Utc::now();
    let audit = Audit::new(log.clone());
    for days in [200, 120, 60, 45, 10] {
        let id = format!("old-{days}");
        audit.write_reply(&id, r#"{"rows":[]}"#);
        audit.write(&Entry {
            id: Some(id),
            ts: now - chrono::Duration::days(days),
            client: "Old Agent".into(),
            transport: "stdio".into(),
            tool: "run_query".into(),
            connection: Some("old-pg".into()),
            database: None,
            sql: Some(format!("SELECT {days}")),
            query: Some(format!("SELECT {days}")),
            rows: Some(1),
            ms: 1.,
            outcome: Outcome::Ok,
            error: None,
            approval: None,
            draft: false,
            favorite: None,
            allowed_by: None,
            rewritten: None,
            seen: None,
            login: None,
        });
    }
    // A reply whose call rotated out long ago.
    let orphan = dataman_core::audit::Reply { id: "orphan".into(), ts: now - chrono::Duration::days(300), cut: false, reply: "{}".into() };
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(dataman_core::audit::replies_path(&log)) {
        use std::io::Write;
        let _ = writeln!(f, "{}", serde_json::to_string(&orphan).unwrap_or_default());
    }
    if !cx.update(|_, cx| ws.read(cx).view == dataman_core::layout::NavView::Agents).unwrap_or(false) {
        click_probe("rail-agents", cx);
    }
    click_probe("agents-nav-audit", cx);
    // The import sheet the steps before leave open (its Esc goes to the editor) covers the page.
    let _ = cx.update(|_, cx| a.update(cx, |a, _| a.import = None));
    let ids = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).entries.iter().filter_map(|e| e.id.clone()).collect::<Vec<_>>()).unwrap_or_default();
    let has = |cx: &mut AsyncWindowContext, id: &str| ids(cx).iter().any(|i| i == id);
    let _ = cx.update(|_, cx| a.update(cx, |a, cx| a.reload(cx)));
    until(cx, 5, |cx| has(cx, "old-10")).await;
    let at = |cx: &mut AsyncWindowContext, id: &str| cx.update(|_, cx| a.read(cx).entries.iter().position(|e| e.id.as_deref() == Some(id))).ok().flatten();
    let old45 = at(cx, "old-45");
    let _ = cx.update(|_, cx| {
        a.update(cx, |a, cx| {
            a.f_agent = Some("Old Agent".into());
            a.selected = old45;
            cx.notify();
        })
    });
    draw(cx);
    click_probe("agents-clear", cx);
    pause(cx, 50).await;
    let labels = cx.update(|_, cx| a.read(cx).demo_menu_labels()).unwrap_or_default();
    r.check(
        "Audit › Clear… offers all, before a date, and keeping the last 30 days, 3 or 6 months",
        labels == ["Clear All…", "Clear Before a Date…", "Keep Only the Last 30 Days…", "Keep Only the Last 3 Months…", "Keep Only the Last 6 Months…"],
        format!("{labels:?}"),
    );
    let sheet = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).clear.as_ref().map(|s| (s.what, s.count))).ok().flatten();
    let on_disk = |id: &str| dataman_core::audit::tail(&log, 100_000).iter().any(|e| e.id.as_deref() == Some(id));
    click_probe("agents-clear:3", cx);
    let counted = until(cx, 5, |cx| sheet(cx).is_some_and(|(_, n)| n.is_some())).await;
    r.check("Keep Only the Last 3 Months… asks first, counting the two older calls", counted && sheet(cx) == Some((Clear::Keep(Recent::Months3), Some(2))), format!("{:?}", sheet(cx)));
    key(cx, "escape");
    pause(cx, 100).await;
    r.check("Esc keeps them: the sheet closes and nothing goes", sheet(cx).is_none() && on_disk("old-200") && has(cx, "old-200"), format!("{:?}", sheet(cx)));
    click_probe("agents-clear", cx);
    pause(cx, 50).await;
    click_probe("agents-clear:3", cx);
    until(cx, 5, |cx| sheet(cx).is_some_and(|(_, n)| n == Some(2))).await;
    click_probe("agents-clear-delete", cx);
    let gone = until(cx, 5, |cx| sheet(cx).is_none() && !has(cx, "old-200") && !has(cx, "old-120")).await;
    let sel = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).selected.and_then(|i| a.read(cx).entries.get(i)).and_then(|e| e.id.clone())).ok().flatten();
    let filter = |cx: &mut AsyncWindowContext| cx.update(|_, cx| a.read(cx).f_agent.clone()).ok().flatten();
    r.check(
        "Delete takes the calls older than 3 months off the disk and the list at once",
        gone && !on_disk("old-200") && !on_disk("old-120") && on_disk("old-60") && has(cx, "old-60"),
        format!("{:?}", ids(cx)),
    );
    r.check(
        "… with their replies and the orphan ones; the kept calls' replies stay",
        dataman_core::audit::reply(&log, "old-200").is_none() && dataman_core::audit::reply(&log, "orphan").is_none() && dataman_core::audit::reply(&log, "old-60").is_some(),
        "",
    );
    r.check("… and the filter and the selected call stay as they were", sel(cx).as_deref() == Some("old-45") && filter(cx).as_deref() == Some("Old Agent"), format!("{:?} {:?}", sel(cx), filter(cx)));
    // Before a date: a calendar, a month back at first; the day picked stays.
    click_probe("agents-clear", cx);
    pause(cx, 50).await;
    click_probe("agents-clear:1", cx);
    until(cx, 5, |cx| sheet(cx).is_some_and(|(_, n)| n.is_some())).await;
    let today = chrono::Local::now().date_naive();
    let first = sheet(cx).map(|(w, _)| w);
    r.check("Clear Before a Date… shows a calendar on the day a month back", first == Some(Clear::Before(today - chrono::Days::new(30))) && probed("agents-clear-calendar", cx).is_some(), format!("{first:?}"));
    let tomorrow = today + chrono::Days::new(1);
    let day = today - chrono::Days::new(20);
    for d in [tomorrow, day] {
        // Tomorrow may sit a month on; the day 20 back, then, a month behind (early in a month).
        if probed(&format!("agents-clear-day-{d}"), cx).is_none() {
            click_probe(if d == tomorrow { "agents-clear-next" } else { "agents-clear-prev" }, cx);
            pause(cx, 50).await;
        }
        click_probe(&format!("agents-clear-day-{d}"), cx);
        pause(cx, 50).await;
        if d == tomorrow {
            let still = sheet(cx).map(|(w, _)| w);
            r.check("a day still to come cannot be picked", still == first, format!("{still:?}"));
        }
    }
    let picked = until(cx, 5, |cx| sheet(cx) == Some((Clear::Before(day), Some(2)))).await;
    r.check("a click on a day picks it and counts again: the two calls before it", picked, format!("{:?}", sheet(cx)));
    key(cx, "cmd-backspace");
    let gone = until(cx, 5, |cx| sheet(cx).is_none() && !has(cx, "old-45")).await;
    r.check("⌘⌫ deletes them; the day's later calls stay", gone && !on_disk("old-60") && !on_disk("old-45") && on_disk("old-10"), format!("{:?}", ids(cx)));
    r.check("… and the selection, its call gone, moves to the first left under the filter", sel(cx).as_deref() == Some("old-10") && filter(cx).as_deref() == Some("Old Agent"), format!("{:?} {:?}", sel(cx), filter(cx)));
    let all = dataman_core::audit::count_before(&log, chrono::Utc::now());
    click_probe("agents-clear", cx);
    pause(cx, 50).await;
    click_probe("agents-clear:0", cx);
    until(cx, 5, |cx| sheet(cx).is_some_and(|(_, n)| n.is_some())).await;
    r.check("Clear All… counts every call in the log", sheet(cx) == Some((Clear::All, Some(all))) && all > 1, format!("{:?} of {all}", sheet(cx)));
    click_probe("agents-clear-delete", cx);
    let empty = until(cx, 5, |cx| sheet(cx).is_none() && ids(cx).is_empty()).await;
    let replies = std::fs::read_to_string(dataman_core::audit::replies_path(&log)).unwrap_or_default();
    r.check("Delete empties the audit and its replies", empty && dataman_core::audit::tail(&log, 10).is_empty() && replies.trim().is_empty(), format!("{:?} {}", ids(cx), replies.len()));
    r.check("… and a filter on an agent no longer there goes, with the selection", filter(cx).is_none() && sel(cx).is_none(), format!("{:?} {:?}", filter(cx), sel(cx)));
    let tools: Vec<String> = dataman_mcp::registry(&core.dirs).all().iter().map(|t| t.name().to_string()).collect();
    r.check("no MCP tool clears or touches the audit", !tools.is_empty() && !tools.iter().any(|t| t.contains("audit") || t.contains("clear") || t.contains("prune")), format!("{tools:?}"));
}
