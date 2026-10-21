//! `agents-check`: the Agents area through its own controls (the rail's robot, the nav, filters,
//! segments, checkboxes, fields and menus), with what each writes read back from the saved
//! connection and settings. The `agents-*` demo stages show its pages for screenshots.


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
    for (col, how, on) in [("users.email", Mask::Partial, true), ("customers.ssn", Mask::Hash, true), ("customers.doc", Mask::Hash, true), ("customers.phone", Mask::Partial, true), ("orders.notes", Mask::Hidden, false)] {
        r.sensitive.push(SensitiveCol { col: col.into(), how, on });
    }
    for (name, kind) in [("public.payments", ItemKind::Table), ("audit.*", ItemKind::Schema), ("users.password_hash", ItemKind::Column), ("users.mfa_secret", ItemKind::Column)] {
        r.visible.items.push(VisibleItem { name: name.into(), kind });
    }
    r.structure = false;
    r.activity = false;
    (r.time_limit_s, r.read_rows, r.read_bytes) = (Some(30), Some(100), Some(64 * 1024));
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
        (1980, "Claude Code", "orders-mongo", "nosql_query", "db.customers.find({ city: \"Austin\" }).limit(20)", Some(20), 8., Outcome::Ok, None, None, None),
        (1614, "Codex", "cache-redis", "nosql_query", "KEYS *", None, 0.4, Outcome::Refused, Some("refused: KEYS never runs for agents"), None, None),
        (1530, "Codex", "cache-redis", "nosql_query", "SCAN 0 MATCH cart:* COUNT 100", Some(100), 3., Outcome::Ok, None, None, None),
        (1140, "Cursor", "analytics-mysql", "run_query", "SELECT * FROM events ORDER BY id DESC LIMIT 100", Some(100), 64., Outcome::Ok, None, None, seen(Some(9), &[], &[])),
        (750, "Claude Code", "prod-pg", "execute", "DELETE FROM sessions WHERE expires_at < now()", None, 2104., Outcome::Refused, Some("refused: denied in Dataman (18,204 rows, over the 1k limit); rolled back"), note(Decision::Denied, &["18,204 rows, over the 1k limit"]), None),
        (360, "Cursor", "search-es", "nosql_query", "GET products/_search {\"query\":{\"match\":{\"name\":\"dock\"}}}", Some(20), 17., Outcome::Ok, None, None, None),
        (240, "Cursor", "analytics-mysql", "run_query", "SELECT date(created_at), count(*) FROM events GROUP BY 1", Some(30), 210., Outcome::Ok, None, None, seen(Some(2), &[], &[])),
        (113, "Claude Code", "orders-mongo", "nosql_query", "db.orders.aggregate([{ $match: { status: \"paid\" } }, { $group: { _id: \"$customer\", total: { $sum: \"$total\" } } }])", Some(500), 91., Outcome::Ok, None, None, seen(None, &[("customer.email", Mask::Partial)], &[])),
        (77, "Claude Code", "prod-pg", "run_query", "SELECT id, name, email, ssn,\n       city, total\n  FROM customers\n WHERE city = 'Austin'\n LIMIT 40;", Some(40), 41., Outcome::Ok, None, None, seen(Some(6), &[("email", Mask::Partial), ("ssn", Mask::Hash)], &[("E-mail", 3)])),
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
        });
    }
}

/// What the agent received in some of the mockups' calls, as the server sent it.
fn demo_reply(ago: i64) -> Option<serde_json::Value> {
    use serde_json::json;
    Some(match ago {
        77 => json!({
            "columns": [["id", "int4"], ["name", "text"], ["email", "text"], ["ssn", "text"], ["city", "text"], ["total", "numeric"]],
            "rows": [
                [1042, "Ana Souza", "a•••@acme.com", "‹h:7Q2KX9RM4A›", "Austin", "129.90"],
                [1043, "Bruno Lima", "b•••@mail.com", "‹h:B4DN6WTE2Q›", "Austin", "88.00"],
                [1051, "Carla Dias", "c•••@acme.com", "‹h:K3P9QX2M7A›", "Austin", "412.35"],
                [1067, "Davi Rocha", "d•••@corp.io", "‹h:Q4M8ZP1R6T›", "Austin", "57.10"],
                [1070, "Elisa Melo", "e•••@acme.com", "‹h:H2V7NC5K9W›", "Austin", "230.00"],
                [1088, "Fábio Reis", "f•••@mail.com", "‹h:M6T3XQ8B1D›", "Austin", "19.99"]
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

/// `agents-audit[-zoom|-zooming|-wide]`, `agents-permissions[-mongo|-redis]`, `agents-privacy`,
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
        s if s.starts_with("agents-mcp") => Page::Mcp,
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
            if page == Page::Types {
                a.editor.name.update(cx, |i, cx| i.set_text("Customer code", cx));
                a.editor.regex.update(cx, |i, cx| i.set_text(r"\bCUST-[0-9]{6}\b", cx));
                a.editor.mask = Mask::Hash;
                a.editor.samples = ["Order CUST-004211 shipped", "merge CUST-000017 into CUST-000930", "ref CUST-42", "xCUST-123456"].map(String::from).to_vec();
            }
            cx.notify();
        });
    });
    if page == Page::Types {
        let regex = ws.read(cx).agents.read(cx).editor.regex.clone();
        window.focus(&gpui::Focusable::focus_handle(regex.read(cx), cx), cx);
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
    r.check("a click on it opens the Agents area on Audit, with the agents' calls", shows(cx) && page(cx) == Page::Audit && loaded, format!("{:?}", page(cx)));
    let pages: Vec<&str> = Page::ALL.iter().map(|p| p.label()).collect();
    r.check("the area's pages: MCP first, then Audit, Permissions, Privacy, Sensitive data (no Home)", pages == ["MCP", "Audit", "Permissions", "Privacy", "Sensitive data"], format!("{pages:?}"));
    let when = cx.update(|_, cx| a.read(cx).f_when).ok();
    r.check("Audit opens on all time, so older calls show at once", when == Some(crate::agents::When::All), format!("{when:?}"));
    let connected = cx.update(|_, cx| a.read(cx).connected_count()).unwrap_or(0);
    r.check("its header counts two agents seen in the last minutes (Codex is idle)", connected == 2, connected);
    click_probe("agents-audit-row-3", cx);
    pause(cx, 100).await;
    let sel = cx.update(|_, cx| a.read(cx).selected).ok().flatten();
    r.check("a click selects a call", page(cx) == Page::Audit && sel == Some(3), format!("{:?} {sel:?}", page(cx)));
    let note = cx.update(|_, cx| crate::agents::audit_note(&a.read(cx).entries[3])).unwrap_or_default();
    r.check("its note: rows · columns · masked", note.0 == "40 rows · 6 columns · 2 masked" && !note.1, format!("{note:?}"));
    let decision = cx.update(|_, cx| crate::agents::decision_text(&a.read(cx).entries[3])).unwrap_or_default();
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
    r.check("its detail shows what the agent received, read from the log on disk: the rows, masked as sent", reply.contains("a•••@acme.com") && reply.contains("‹h:") && !reply.contains("ana@"), &reply);
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
    click_probe("agents-recv-zoom", cx);
    pause(cx, 100).await;
    r.check("its zoom button takes the whole Audit page", zoom(cx), "");
    key(cx, "escape");
    pause(cx, 100).await;
    r.check("Esc leaves the zoom, as in a result", !zoom(cx), "");
    key(cx, "cmd-shift-enter");
    pause(cx, 100).await;
    r.check("the result's zoom shortcut zooms it again", zoom(cx), "");
    click_probe("agents-recv-zoom", cx);
    pause(cx, 100).await;
    r.check("… and the button restores it", !zoom(cx), "");
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
        pause(cx, 60).await;
        let (mid, mid_b) = (now(cx), probed("agents-recv", cx));
        let in_time = mid_possible(t0, crate::motion::ZOOM);
        pause(cx, 320).await;
        let after = probed("agents-recv", cx);
        let end = if zoom_in { 1. } else { 0. };
        let between = |m: f32, x: f32, y: f32| (x.min(y) + 1. ..x.max(y) - 1.).contains(&m);
        let grew = before.zip(mid_b).zip(after).is_some_and(|((b, m), a)| between(m.top().as_f32(), b.top().as_f32(), a.top().as_f32()) && between(m.size.width.as_f32(), b.size.width.as_f32(), a.size.width.as_f32()));
        r.check(
            &format!("{name}, the reply moves part of the way, then lands"),
            (!motion || !in_time || (mid > 0.05 && mid < 0.999 && grew)) && now(cx) == end && !moving(cx),
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
    pause(cx, 400).await;
    let zoomed = cx.update(|_, cx| a.read(cx).received_zoom && a.read(cx).received_tween.is_none()).unwrap_or(false);
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
    click_probe("agents-clear-filters", cx);
    pause(cx, 50).await;
    r.check("Nothing matches offers Clear filters, which brings every call back", shown(cx) == all, shown(cx));
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

    // Permissions, on the PostgreSQL connection the check saved.
    click_probe("agents-nav-permissions", cx);
    pause(cx, 100).await;
    r.check("the nav opens Permissions", page(cx) == Page::Permissions, format!("{:?}", page(cx)));
    click_probe(&format!("agents-conn-{}", pg.name), cx);
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
        "Permissions: ↓ and ↑ move through the connections, and the page follows",
        down.as_ref() == order.get((at + 1).min(order.len() - 1)) && up.as_deref() == Some(pg.id.as_str()),
        format!("{down:?} {up:?}"),
    );
    click_probe("agents-tier-1", cx);
    pause(cx, 50).await;
    click_probe("agents-may-structure", cx);
    pause(cx, 50).await;
    use crate::agents_rules::may_state;
    r.check(
        "on Read, Change structure is greyed (it needs Full) and a click changes nothing",
        rules(&core, &pg.id).structure && may_state(McpAccess::Read, McpAccess::Full, true, false, false) == ("needs Full", false),
        format!("{:?}", rules(&core, &pg.id).structure),
    );
    click_probe("agents-tier-3", cx);
    pause(cx, 50).await;
    r.check("on Full it asks you first", may_state(McpAccess::Full, McpAccess::Full, true, false, false) == ("asks you first", true), "");
    click_probe("agents-may-structure", cx);
    pause(cx, 50).await;
    r.check("Agents may › Change structure, off: saved on the connection, and reads as never", !rules(&core, &pg.id).structure && may_state(McpAccess::Full, McpAccess::Full, false, false, false).0 == "never", format!("{:?}", rules(&core, &pg.id)));
    let chips = |p: &str, cx: &mut AsyncWindowContext| (0..8).filter(|k| probed(&format!("{p}-{k}"), cx).is_some()).count();
    let counts = (chips("agents-wrows", cx), chips("agents-rrows", cx), chips("agents-rbytes", cx));
    let unset = rules(&core, &pg.id);
    r.check(
        "Limits show real values only (no Default chip), each list holding its default",
        counts == (4, 4, 3)
            && dataman_core::privacy::WRITE_ROWS.contains(&dataman_core::settings::MCP_WRITE_ROW_LIMIT)
            && dataman_core::privacy::READ_ROWS.contains(&dataman_core::settings::MCP_MAX_ROWS)
            && dataman_core::privacy::READ_BYTES.contains(&dataman_core::privacy::DEFAULT_READ_BYTES)
            && unset.read_rows.is_none(),
        format!("{counts:?}"),
    );
    click_probe("agents-time-1", cx);
    click_probe("agents-rrows-1", cx);
    pause(cx, 50).await;
    let rr = rules(&core, &pg.id);
    r.check("Limits: 30 s per call, 100 rows per read", rr.time_limit_s == Some(30) && rr.read_rows == Some(100), format!("{:?} {:?}", rr.time_limit_s, rr.read_rows));
    click_probe("agents-rrows-3", cx);
    pause(cx, 50).await;
    r.check("picking the default (1k) stores nothing: the connection follows the default again", rules(&core, &pg.id).read_rows.is_none(), format!("{:?}", rules(&core, &pg.id).read_rows));
    click_probe("agents-rrows-1", cx);
    pause(cx, 50).await;
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
    key(cx, "home");
    pause(cx, 50).await;
    let first = cx.update(|_, cx| a.read(cx).conn.clone()).ok().flatten();
    r.check("Privacy: Home goes to the first connection", first.as_ref() == order.first(), format!("{first:?}"));
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
    // Suggestions under the fields, from the connection's schema.
    click_probe("agents-field-visible", cx);
    typ(cx, "pub");
    let listed = until(cx, 5, |cx| cx.update(|_, cx| !a.read(cx).suggest.items.is_empty()).unwrap_or(false)).await;
    let first = cx.update(|_, cx| a.read(cx).suggest.items.first().map(|c| c.text.clone())).ok().flatten();
    r.check("Visible to agents suggests as you type (schemas and tables)", listed && first.as_deref().is_some_and(|f| f.starts_with("public")), format!("{first:?}"));
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
    click_probe("agents-field-sensitive", cx);
    typ(cx, &format!("{table}."));
    let cols = until(cx, 5, |cx| cx.update(|_, cx| a.read(cx).suggest.items.first().is_some_and(|c| c.kind == ItemKind::Column)).unwrap_or(false)).await;
    r.check("Sensitive columns: a table and a dot list its columns", cols, &table);
    key(cx, "escape");
    pause(cx, 50).await;
    r.check("Esc closes the suggestions", cx.update(|_, cx| a.read(cx).suggest.which.is_none()).unwrap_or(false), "");
    key(cx, "cmd-a");
    key(cx, "backspace");
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
    r.check("Sensitive data lists the built-in and custom types", cx.update(|_, cx| a.read(cx).type_rows().len()).unwrap_or(0) == dataman_core::privacy::BUILTINS.len() + 2, "");
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
    r.check("Sensitive data: ↓ moves to E-mail, Enter switches it off and on", tsel == Some(1) && flipped == Some(false) && back == Some(true), format!("{tsel:?} {flipped:?} {back:?}"));
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

    // One list for every connection: no picker of connections, no reset, and a switch is global.
    let ids: Vec<String> = cx.update(|_, cx| a.read(cx).type_rows().into_iter().map(|t| t.id).collect()).unwrap_or_default();
    r.check("Sensitive data has no CPF or CNPJ, and starts with cards", ids.first().map(String::as_str) == Some("card") && !ids.iter().any(|i| i == "cpf" || i == "cnpj"), format!("{ids:?}"));
    r.check("Sensitive data is one list: no connection picker, no Reset to global", probed("agents-scope", cx).is_none() && probed("agents-reset", cx).is_none(), "");
    click_probe("agents-type-switch-phone", cx);
    pause(cx, 50).await;
    let off = core.settings.get().agent_types.is_on("phone") == Some(false);
    click_probe("agents-type-switch-phone", cx);
    pause(cx, 50).await;
    r.check("Phone's switch turns it off and on for every connection", off && core.settings.get().agent_types.is_on("phone") == Some(true), "");
    let tip = dataman_core::privacy::builtin("phone").map(|b| (b.rule, b.example)).unwrap_or_default();
    r.check("Phone reads US numbers", tip.0.contains("(555) 123-4567") && tip.1.starts_with("(555) 123-4567"), format!("{tip:?}"));

    // MCP: the server's own page.
    click_probe("agents-nav-mcp", cx);
    pause(cx, 100).await;
    r.check("Agents › MCP: agent access and the command that connects Claude Code", page(cx) == Page::Mcp && probed("agents-mcp-copy", cx).is_some() && probed("agents-mcp-access-1", cx).is_some(), "");
    click_probe("agents-mcp-access-0", cx);
    pause(cx, 50).await;
    let off = !core.settings.get().mcp_enabled;
    click_probe("agents-mcp-access-1", cx);
    pause(cx, 50).await;
    r.check("its Agent access switch is the global one", off && core.settings.get().mcp_enabled, "");
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

    // Showing a tab uncovers it.
    let active = cx.update(|_, cx| ws.read(cx).active).unwrap_or(0);
    let tab = cx.update(|_, cx| ws.read(cx).tab_bounds.borrow().first().copied()).ok().flatten();
    if let Some(b) = tab {
        click(cx, centre(b));
    }
    pause(cx, 100).await;
    let now = cx.update(|_, cx| ws.read(cx).active).unwrap_or(9);
    r.check("in Agents, a click on the tab that was already active shows it", tab.is_some() && !shows(cx) && now == active, format!("{tab:?} {now}"));
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
}
