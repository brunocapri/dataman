use std::time::Duration;

use gpui::{AsyncWindowContext, Entity};
use dataman_core::config::{ConnectionProfile, Engine, SslMode};
use dataman_core::nosql::{Output, Page};

use crate::app::Workspace;
use crate::conns::ConnState;
use crate::tab::QueryTab;

pub fn is_stage(s: &str) -> bool {
    s == "nosql-check" || is_demo(s)
}

pub fn is_demo(s: &str) -> bool {
    matches!(s, "nosql" | "nosql-dialog" | "nosql-form" | "nosql-picker" | "nosql-es" | "nosql-redis")
}

async fn demo_statement(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) {
    let Ok(text) = std::env::var("DATAMAN_DEMO_DOC") else { return };
    let _ = cx.update(|window, cx| {
        if let Some(t) = ws.read(cx).active_tab().cloned() {
            let editor = t.read(cx).editor.clone();
            editor.update(cx, |e, cx| {
                e.set_text(&text, cx);
                e.set_cursor(0, cx);
                e.run_script(cx);
            });
            window.focus(&gpui::Focusable::focus_handle(editor.read(cx), cx), cx);
        }
    });
    until(cx, 10, |cx| idle(ws, cx)).await;
    pause(cx, 400).await;
}

pub async fn demo(ws: &Entity<Workspace>, stage: &str, cx: &mut AsyncWindowContext) {
    let (mongo, mpw) = mongo_profile();
    let (pg, ppw) = crate::bench::bench_profile(None);
    let mid = mongo.id.clone();
    let core = cx.update(|_, cx| ws.read(cx).store.read(cx).core.clone()).unwrap();
    {
        let core = core.clone();
        let (mongo, mpw) = (mongo.clone(), mpw.clone());
        let _ = cx
            .background_executor()
            .spawn(async move {
                let c = core.block_on(core.connect(mongo, Some(mpw)))?;
                let people = [("Ada Lovelace", 36, "London"), ("Grace Hopper", 85, "New York"), ("Alan Turing", 41, "Manchester"), ("Barbara Liskov", 84, "Boston"), ("Edsger Dijkstra", 72, "Austin")];
                let docs: Vec<String> = (0..60)
                    .map(|i| {
                        let (n, a, city) = people[i % people.len()];
                        format!("{{name: '{n}', age: NumberLong({a}), email: '{}@example.com', address: {{city: '{city}', zip: '{:05}'}}, tags: ['vip', 'beta'], created: ISODate('2024-0{}-1{}T09:30:00Z'), active: {}}}", n.split(' ').next().unwrap().to_lowercase(), 10000 + i * 37, 1 + i % 9, i % 9, i % 3 != 0)
                    })
                    .collect();
                for stmt in ["db.dropDatabase()".to_string(), format!("db.people.insertMany([{}])", docs.join(",")), "db.orders.insertOne({total: 12.5})".into(), "db.createCollection('events')".into()] {
                    core.block_on(c.run_nosql(DB, &stmt, Page::default()))?;
                }
                anyhow::Ok(())
            })
            .await;
    }
    let _ = cx.update(|_, cx| {
        crate::bench::connect(ws, pg.clone(), ppw.clone(), cx);
        crate::bench::connect(ws, mongo.clone(), mpw.clone(), cx);
    });
    until(cx, 20, |cx| connected(ws, &mid, cx) && connected(ws, &pg.id, cx)).await;
    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    if stage == "nosql-redis" {
        let (rd, rpw) = redis_profile();
        let rid = rd.id.clone();
        let _ = seed_redis(&core, rd.clone(), rpw.clone(), cx).await;
        let _ = cx.update(|_, cx| crate::bench::connect(ws, rd, rpw, cx));
        until(cx, 20, |cx| connected(ws, &rid, cx)).await;
        let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&format!("c:{rid}"), cx)));
        pause(cx, 600).await;
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_collection(&rid, "7", "user:1", window, cx)));
        until(cx, 10, |cx| idle(ws, cx) && shown(ws, cx).is_some()).await;
        demo_statement(ws, cx).await;
        pause(cx, 800).await;
        draw(cx);
        if let Ok(path) = std::env::var("DATAMAN_SHOT") {
            let _ = cx.update(|window, cx| {
                crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
                cx.quit();
            });
        }
        return;
    }
    if stage == "nosql-es" {
        let (es, epw) = es_profile();
        let eid = es.id.clone();
        let _ = seed_es(&core, es.clone(), epw.clone(), cx).await;
        let _ = cx.update(|_, cx| crate::bench::connect(ws, es, epw, cx));
        until(cx, 20, |cx| connected(ws, &eid, cx)).await;
        let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&format!("c:{eid}"), cx)));
        pause(cx, 600).await;
        let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_collection(&eid, "", INDEX, window, cx)));
        until(cx, 10, |cx| idle(ws, cx) && shown(ws, cx).is_some()).await;
        demo_statement(ws, cx).await;
        pause(cx, 800).await;
        draw(cx);
        if let Ok(path) = std::env::var("DATAMAN_SHOT") {
            let _ = cx.update(|window, cx| {
                crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
                cx.quit();
            });
        }
        return;
    }
    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&format!("c:{mid}"), cx)));
    pause(cx, 600).await;
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_collection(&mid, DB, "people", window, cx)));
    until(cx, 10, |cx| idle(ws, cx) && shown(ws, cx).is_some()).await;
    let _ = cx.update(|_, cx| {
        if let Some(t) = ws.read(cx).active_tab().cloned() {
            let editor = t.read(cx).editor.clone();
            editor.update(cx, |e, cx| e.set_text("db.people.find({ age: { $gt: 30 } })\n  .sort({ name: 1 })", cx));
        }
    });
    if !matches!(stage, "nosql-dialog" | "nosql-form" | "nosql-picker") {
        demo_statement(ws, cx).await;
    }
    match stage {
        "nosql-dialog" => {
            let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dialog(None, window, cx)));
        }
        "nosql-form" => {
            let _ = cx.update(|window, cx| {
                ws.update(cx, |w, cx| {
                    w.open_dialog(None, window, cx);
                    if let Some(d) = w.dialog.clone() {
                        d.update(cx, |d, cx| d.demo_engine(Engine::MongoDb, cx));
                    }
                })
            });
        }
        "nosql-picker" => {
            let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.toggle_conn_menu(cx)));
        }
        _ => {}
    }
    pause(cx, 800).await;
    draw(cx);
    if let Ok(path) = std::env::var("DATAMAN_SHOT") {
        let _ = cx.update(|window, cx| {
            crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
            cx.quit();
        });
    }
}

pub fn mongo_profile() -> (ConnectionProfile, String) {
    let spec = std::env::var("DATAMAN_BENCH_MONGO").unwrap_or_else(|_| "127.0.0.1:27027:root:dataman".into());
    let p: Vec<&str> = spec.split(':').collect();
    let mut prof = ConnectionProfile::new("mongo-local");
    prof.engine = Engine::MongoDb;
    prof.host = p[0].into();
    prof.port = p[1].parse().unwrap();
    prof.user = p[2].into();
    prof.database = DB.into();
    prof.sslmode = SslMode::Disable;
    prof.save_password = true;
    (prof, p.get(3).unwrap_or(&"").to_string())
}

const DB: &str = "dataman_ui";

pub fn es_profile() -> (ConnectionProfile, String) {
    let spec = std::env::var("DATAMAN_BENCH_ES").unwrap_or_else(|_| "127.0.0.1:9201:elastic:dataman".into());
    let p: Vec<&str> = spec.split(':').collect();
    let mut prof = ConnectionProfile::new("es-local");
    prof.engine = Engine::Elasticsearch;
    prof.host = p[0].into();
    prof.port = p[1].parse().unwrap();
    prof.user = p[2].into();
    prof.database = String::new();
    prof.sslmode = SslMode::Disable;
    prof.save_password = true;
    (prof, p.get(3).unwrap_or(&"").to_string())
}

const INDEX: &str = "dataman_ui_orders";

pub fn redis_profile() -> (ConnectionProfile, String) {
    let spec = std::env::var("DATAMAN_BENCH_REDIS").unwrap_or_else(|_| "127.0.0.1:63790:dataman".into());
    let p: Vec<&str> = spec.split(':').collect();
    let mut prof = ConnectionProfile::new("redis-local");
    prof.engine = Engine::Redis;
    prof.host = p[0].into();
    prof.port = p[1].parse().unwrap();
    prof.user = String::new();
    prof.database = "7".into();
    prof.sslmode = SslMode::Disable;
    prof.save_password = true;
    (prof, p.get(2).unwrap_or(&"").to_string())
}

async fn seed_redis(core: &std::sync::Arc<dataman_core::Core>, p: ConnectionProfile, pw: String, cx: &mut AsyncWindowContext) -> anyhow::Result<()> {
    let core = core.clone();
    cx.background_executor()
        .spawn(async move {
            let c = core.block_on(core.connect(p, Some(pw)))?;
            for cmd in [
                "FLUSHDB",
                "HSET user:1 name \"Ada Lovelace\" email ada@example.com city London plan pro",
                "HSET user:2 name \"Grace Hopper\" email grace@example.com city \"New York\"",
                "RPUSH jobs:queue send-mail resize-image rebuild-index",
                "ZADD leaderboard 4200 ada 3900 grace 3100 alan 2800 barbara",
                "SET feature:dark-mode on",
            ] {
                core.block_on(c.run_nosql("7", cmd, Page::default()))?;
            }
            anyhow::Ok(())
        })
        .await
}

async fn seed_es(core: &std::sync::Arc<dataman_core::Core>, p: ConnectionProfile, pw: String, cx: &mut AsyncWindowContext) -> anyhow::Result<()> {
    let core = core.clone();
    cx.background_executor()
        .spawn(async move {
            let c = core.block_on(core.connect(p, Some(pw)))?;
            let mut bulk = format!("POST /{INDEX}/_bulk?refresh=true\n");
            for i in 1..=75 {
                let status = ["paid", "shipped", "new"][i % 3];
                bulk.push_str(&format!("{{\"index\": {{\"_id\": \"{i}\"}}}}\n{{\"n\": {i}, \"status\": \"{status}\", \"total\": {}.5, \"customer\": {{\"name\": \"customer {i}\", \"city\": \"Lisbon\"}}}}\n", i * 3));
            }
            let _ = core.block_on(c.run_nosql("", &format!("DELETE /{INDEX}"), Page::default()));
            core.block_on(c.run_nosql("", &format!("PUT /{INDEX}\n{{\"mappings\": {{\"properties\": {{\"n\": {{\"type\": \"integer\"}}, \"status\": {{\"type\": \"keyword\"}}}}}}}}"), Page::default()))?;
            core.block_on(c.run_nosql("", &bulk, Page::default()))?;
            anyhow::Ok(())
        })
        .await
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn key(cx: &mut AsyncWindowContext, k: &str) {
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(crate::keymap::keystroke(k), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

async fn run_text(ws: &Entity<Workspace>, text: &str, cx: &mut AsyncWindowContext) -> (Option<Result<Output, String>>, String) {
    let _ = cx.update(|_, cx| {
        if let Some(t) = ws.read(cx).active_tab().cloned() {
            let editor = t.read(cx).editor.clone();
            editor.update(cx, |e, cx| {
                e.set_text(text, cx);
                e.set_cursor(0, cx);
                e.run_script(cx);
            });
        }
    });
    pause(cx, 50).await;
    until(cx, 10, |cx| idle(ws, cx)).await;
    draw(cx);
    cx.update(|_, cx| {
        let doc = ws.read(cx).active_tab()?.read(cx).doc.clone()?;
        let d = doc.read(cx);
        let out = d.shown().map(|(_, r)| r.map(|r| r.output.clone()).map_err(|f| f.message.clone()));
        let detail = d.demo_detail().map(|e| e.read(cx).text().to_string()).unwrap_or_default();
        Some((out, detail))
    })
    .ok()
    .flatten()
    .unwrap_or((None, String::new()))
}

async fn until(cx: &mut AsyncWindowContext, secs: u64, mut ok: impl FnMut(&mut AsyncWindowContext) -> bool) -> bool {
    for _ in 0..secs * 20 {
        if ok(cx) {
            return true;
        }
        pause(cx, 50).await;
        draw(cx);
    }
    ok(cx)
}

async fn count(core: &std::sync::Arc<dataman_core::Core>, p: &ConnectionProfile, pw: &str, stmt: &str, cx: &mut AsyncWindowContext) -> Option<String> {
    let (core, p, pw, stmt) = (core.clone(), p.clone(), pw.to_string(), stmt.to_string());
    cx.background_executor()
        .spawn(async move {
            let c = core.block_on(core.connect(p, Some(pw))).ok()?;
            match core.block_on(c.run_nosql(DB, &stmt, Page::default())).ok()?.output {
                Output::Value(b) => Some(b.inline().trim_start_matches("NumberLong(").trim_end_matches(')').to_string()),
                _ => None,
            }
        })
        .await
}

fn active(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Entity<QueryTab>> {
    cx.update(|_, cx| ws.read(cx).active_tab().cloned()).ok().flatten()
}

fn connected(ws: &Entity<Workspace>, id: &str, cx: &mut AsyncWindowContext) -> bool {
    matches!(cx.update(|_, cx| ws.read(cx).connection_state(id, cx)).ok().flatten(), Some(ConnState::Connected(_)))
}

fn shown(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<(String, usize, Option<u64>, bool)> {
    cx.update(|_, cx| {
        let tab = ws.read(cx).active_tab()?.read(cx);
        let doc = tab.doc.as_ref()?.read(cx);
        let (sql, r) = doc.shown()?;
        let (n, total, more) = match r.ok()?.output {
            Output::Docs(ref d) => (d.docs.len(), d.total, d.more),
            _ => return None,
        };
        Some((sql.to_string(), n, total, more))
    })
    .ok()
    .flatten()
}

fn idle(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|_, cx| ws.read(cx).active_tab().is_some_and(|t| !t.read(cx).is_running())).unwrap_or(false)
}

pub async fn run(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut check = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass || detail.is_empty() { String::new() } else { format!(": {detail}") });
        ok &= pass;
    };
    let (mongo, mpw) = mongo_profile();
    let (pg, ppw) = crate::bench::bench_profile(None);
    let (mid, pid) = (mongo.id.clone(), pg.id.clone());
    let core = cx.update(|_, cx| ws.read(cx).store.read(cx).core.clone()).unwrap();
    let seed = {
        let core = core.clone();
        let (mongo, mpw) = (mongo.clone(), mpw.clone());
        cx.background_executor()
            .spawn(async move {
                let c = core.block_on(core.connect(mongo, Some(mpw)))?;
                let docs: Vec<String> = (1..=60).map(|i| format!("{{_id: {i}, name: 'person {i}', age: NumberLong({}), tags: ['t{}']}}", 20 + i % 30, i % 3)).collect();
                for stmt in ["db.dropDatabase()".to_string(), format!("db.people.insertMany([{}])", docs.join(",")), "db.createCollection('empty')".into()] {
                    core.block_on(c.run_nosql(DB, &stmt, Page::default()))?;
                }
                anyhow::Ok(())
            })
            .await
    };
    if let Err(e) = seed {
        check("the MongoDB test server answers", false, format!("{e:#}"));
        return false;
    }
    let _ = cx.update(|_, cx| {
        crate::bench::connect(ws, pg.clone(), ppw.clone(), cx);
        crate::bench::connect(ws, mongo.clone(), mpw.clone(), cx);
    });
    let up = until(cx, 20, |cx| connected(ws, &mid, cx) && connected(ws, &pid, cx)).await;
    check("PostgreSQL and MongoDB connect side by side", up, String::new());
    if !up {
        return false;
    }

    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&format!("c:{mid}"), cx)));
    let db_id = format!("c:{mid}/d:{DB}");
    let listed = until(cx, 10, |cx| cx.update(|_, cx| tree.read(cx).demo_children(&db_id)).unwrap_or_default().len() >= 2).await;
    let (logos, children) = cx.update(|_, cx| {
        let t = tree.read(cx);
        ((t.demo_logo(&mid), t.demo_logo(&pid)), t.demo_children(&db_id))
    })
    .unwrap();
    check("each connection's row has its engine's logo", logos == (Some(Engine::MongoDb), Some(Engine::Postgres)), format!("{logos:?}"));
    check("MongoDB's own database opens on its collections", listed && children == ["empty", "people"], format!("{children:?}"));

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_collection(&mid, DB, "people", window, cx)));
    let got = until(cx, 10, |cx| idle(ws, cx) && shown(ws, cx).is_some()).await;
    let s = shown(ws, cx);
    let lang = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.read(cx).editor.read(cx).doc_lang())).ok().flatten().flatten();
    check(
        "a collection opens in a tab as documents, the editor in MongoDB's language",
        got && lang == Some(Engine::MongoDb) && s.as_ref().is_some_and(|(sql, n, total, more)| sql == "db.people.find({})" && *n == 50 && *total == Some(60) && *more),
        format!("{s:?} {lang:?}"),
    );
    let Some(doc) = cx.update(|_, cx| ws.read(cx).active_tab().and_then(|t| t.read(cx).doc.clone())).ok().flatten() else {
        check("the tab has a document view", false, String::new());
        return false;
    };
    let _ = cx.update(|_, cx| doc.update(cx, |d, cx| d.demo_page(50, cx)));
    until(cx, 10, |cx| idle(ws, cx) && shown(ws, cx).is_some_and(|s| s.1 == 10)).await;
    let s = shown(ws, cx);
    check("› shows the next page: the last 10", s.as_ref().is_some_and(|s| s.1 == 10 && !s.3), format!("{s:?}"));
    let _ = cx.update(|_, cx| doc.update(cx, |d, cx| d.demo_page(0, cx)));
    until(cx, 10, |cx| idle(ws, cx) && shown(ws, cx).is_some_and(|s| s.1 == 50)).await;

    draw(cx);
    let detail = cx.update(|_, cx| doc.read(cx).demo_detail()).ok().flatten();
    let text = cx.update(|_, cx| detail.as_ref().map(|d| d.read(cx).text().to_string())).ok().flatten().unwrap_or_default();
    check("the first document shows in the shell's form", text.contains("\"_id\": 1") && text.contains("NumberLong(21)"), text.clone());

    let outline = |cx: &mut AsyncWindowContext| cx.update(|_, cx| doc.read(cx).demo_outline(cx)).ok().flatten();
    let lines = |cx: &mut AsyncWindowContext| cx.update(|_, cx| doc.read(cx).demo_outline(cx).map(|o| o.read(cx).demo_lines())).ok().flatten().unwrap_or_default();
    let full = lines(cx);
    check(
        "the document shows as a tree, every key in sight, the shell's types",
        full == ["_id: 1", "name: \"person 1\"", "age: NumberLong(21)", "tags: 1 item", "  0: \"t1\""],
        format!("{full:?}"),
    );
    if let Some(o) = outline(cx) {
        let _ = cx.update(|window, cx| {
            window.focus(&gpui::Focusable::focus_handle(o.read(cx), cx), cx);
            o.update(cx, |o, cx| o.demo_put(3, cx));
        });
        draw(cx);
        key(cx, "left");
        let folded = lines(cx);
        key(cx, "right");
        let open = lines(cx);
        check("← folds the focused array to its preview, → opens it", folded.get(3).map(String::as_str) == Some("tags: […] 1 item") && folded.len() == 4 && open == full, format!("{folded:?} {open:?}"));
        let _ = cx.update(|_, cx| o.update(cx, |o, cx| o.demo_click(3, true, cx)));
        let deep = lines(cx);
        let _ = cx.update(|_, cx| o.update(cx, |o, cx| o.demo_click(3, true, cx)));
        check("⌥-click folds the subtree and opens it again", deep.len() == 4 && lines(cx) == full, format!("{deep:?}"));
        let _ = cx.update(|_, cx| o.update(cx, |o, cx| o.level(1, cx)));
        let _ = cx.update(|_, cx| doc.update(cx, |d, cx| d.demo_select(1, cx)));
        draw(cx);
        let other = lines(cx);
        let _ = cx.update(|_, cx| doc.update(cx, |d, cx| d.demo_select(0, cx)));
        draw(cx);
        let back = lines(cx);
        check(
            "Collapse to level 1 stays with that document: another opens whole, the first comes back folded",
            other.len() == 5 && other[0] == "_id: 2" && back.len() == 4 && back[3] == "tags: […] 1 item",
            format!("{other:?} {back:?}"),
        );
        let _ = cx.update(|window, cx| {
            window.focus(&gpui::Focusable::focus_handle(o.read(cx), cx), cx);
            o.update(cx, |o, cx| o.demo_put(0, cx));
        });
        draw(cx);
        key(cx, "left");
        let in_list = cx.update(|window, cx| doc.read(cx).list_focus().is_focused(window)).unwrap_or(false);
        key(cx, "right");
        let in_tree = cx.update(|window, cx| gpui::Focusable::focus_handle(o.read(cx), cx).is_focused(window)).unwrap_or(false);
        check("← on a top-level key goes back to the list, → from the list into the document", in_list && in_tree, format!("{in_list} {in_tree}"));
        let _ = cx.update(|_, cx| o.update(cx, |o, cx| o.all(true, cx)));
        let _ = cx.update(|window, cx| doc.update(cx, |d, cx| d.demo_set_tree(false, window, cx)));
        let text_mode = outline(cx).is_none();
        let _ = cx.update(|window, cx| doc.update(cx, |d, cx| d.demo_set_tree(true, window, cx)));
        check("Text shows the editor, Tree the outline again", text_mode && outline(cx).is_some(), String::new());
    }
    if let Some(d) = &detail {
        let edited = text.replace("\"person 1\"", "\"Ada\"").replace("NumberLong(21)", "NumberLong(36)");
        let _ = cx.update(|_, cx| {
            d.update(cx, |e, cx| e.set_text(&edited, cx));
            doc.update(cx, |p, cx| p.demo_save(cx));
        });
        let saved = until(cx, 10, |cx| cx.update(|_, cx| doc.read(cx).demo_note()).ok().flatten().is_some_and(|n| n.0 == "Saved")).await;
        until(cx, 5, |cx| idle(ws, cx)).await;
        let check_c = core.clone();
        let (mongo2, mpw2) = (mongo.clone(), mpw.clone());
        let back = cx
            .background_executor()
            .spawn(async move {
                let c = check_c.block_on(check_c.connect(mongo2, Some(mpw2))).ok()?;
                let r = check_c.block_on(c.run_nosql(DB, "db.people.findOne({_id: 1})", Page::default())).ok()?;
                match r.output {
                    Output::Docs(d) => d.docs.first().map(|d| d.body.text()),
                    _ => None,
                }
            })
            .await
            .unwrap_or_default();
        check("Save writes the edited document, its types kept", saved && back.contains("\"name\": \"Ada\"") && back.contains("NumberLong(36)"), back);
    }
    let _ = cx.update(|window, cx| doc.update(cx, |p, cx| p.demo_new(window, cx)));
    if let Some(d) = &detail {
        let _ = cx.update(|_, cx| {
            d.update(cx, |e, cx| e.set_text("{_id: 100, name: 'new one'}", cx));
            doc.update(cx, |p, cx| p.demo_save(cx));
        });
        let inserted = until(cx, 10, |cx| cx.update(|_, cx| doc.read(cx).demo_note()).ok().flatten().is_some_and(|n| n.0 == "Inserted")).await;
        until(cx, 5, |cx| idle(ws, cx)).await;
        let s = shown(ws, cx);
        check("New inserts a document; the page shows the new total", inserted && s.as_ref().is_some_and(|s| s.2 == Some(61)), format!("{s:?}"));
    }

    let _ = cx.update(|window, cx| {
        let f = doc.read(cx).list_focus();
        window.focus(&f, cx);
        doc.update(cx, |d, cx| d.demo_select(30, cx));
    });
    for _ in 0..10 {
        key(cx, "down");
    }
    draw(cx);
    draw(cx);
    let (sel, b) = cx.update(|_, cx| (doc.read(cx).demo_selected(), doc.read(cx).demo_list_bounds(40))).unwrap_or_default();
    let in_view = b.is_some_and(|(list, row)| row.top() >= list.top() - gpui::px(1.) && row.bottom() <= list.bottom() + gpui::px(1.));
    check("↓ moves the selection and the list scrolls with it", sel == Some(40) && in_view, format!("{sel:?} {b:?}"));
    if let Some(d) = &detail {
        let text = cx.update(|_, cx| d.read(cx).text().to_string()).unwrap_or_default();
        let id = text.lines().find(|l| l.contains("\"_id\"")).map(str::to_string).unwrap_or_default();
        let _ = cx.update(|_, cx| {
            d.update(cx, |e, cx| e.set_text(&text.replace("\"name\": \"", "\"name\": \"zz "), cx));
            doc.update(cx, |p, cx| p.demo_save(cx));
        });
        until(cx, 10, |cx| cx.update(|_, cx| doc.read(cx).demo_note()).ok().flatten().is_some_and(|n| n.0 == "Saved")).await;
        until(cx, 5, |cx| idle(ws, cx)).await;
        draw(cx);
        let (sel, now) = cx.update(|_, cx| (doc.read(cx).demo_selected(), d.read(cx).text().to_string())).unwrap_or_default();
        check("after a save the same document stays selected", sel == Some(40) && now.contains(&id) && now.contains("zz "), format!("{sel:?} {id} {now}"));
    }

    let _ = cx.update(|window, cx| {
        let f = doc.read(cx).list_focus();
        window.focus(&f, cx);
    });
    key(cx, "cmd-shift-enter");
    draw(cx);
    let zoomed = cx.update(|_, cx| (ws.read(cx).active_tab().map(|t| t.read(cx).zoomed), doc.read(cx).zoomed)).ok();
    key(cx, "escape");
    draw(cx);
    let back = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.read(cx).zoomed)).ok().flatten();
    check("⌘⇧↩ on the documents zooms the result like the grid's; esc puts it back", zoomed == Some((Some(true), true)) && back == Some(false), format!("{zoomed:?} {back:?}"));
    let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::app::ToggleResultZoom), cx));
    let by_button = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.read(cx).zoomed)).ok().flatten();
    let _ = cx.update(|window, cx| window.dispatch_action(Box::new(crate::app::ToggleResultZoom), cx));
    draw(cx);
    check("the bar's zoom button does the same", by_button == Some(true), format!("{by_button:?}"));

    let tab = active(ws, cx).unwrap();
    let _ = cx.update(|_, cx| {
        let editor = tab.read(cx).editor.clone();
        editor.update(cx, |e, cx| {
            e.set_text("db.people.deleteMany({})", cx);
            e.run_script(cx);
        });
    });
    draw(cx);
    let asked = cx.update(|_, cx| doc.read(cx).has_confirm()).unwrap_or(false);
    let _ = cx.update(|_, cx| doc.update(cx, |d, cx| d.resolve_confirm(false, cx)));
    let s = shown(ws, cx);
    check("deleteMany({}) waits for a yes; Cancel runs nothing", asked && s.as_ref().is_some_and(|s| s.0 == "db.people.find({})"), format!("asked {asked}, {s:?}"));

    let _ = cx.update(|_, cx| {
        let editor = tab.read(cx).editor.clone();
        editor.update(cx, |e, cx| {
            e.set_text("db.peo", cx);
            e.demo_show_completions(cx);
        });
    });
    let first = cx.update(|_, cx| tab.read(cx).editor.read(cx).demo_completions()).unwrap_or_default();
    let mut fields = Vec::new();
    for _ in 0..40 {
        let _ = cx.update(|_, cx| {
            let editor = tab.read(cx).editor.clone();
            editor.update(cx, |e, cx| {
                e.set_text("db.people.find({ag", cx);
                e.demo_show_completions(cx);
            });
        });
        fields = cx.update(|_, cx| tab.read(cx).editor.read(cx).demo_completions()).unwrap_or_default();
        if fields.contains(&"age".to_string()) {
            break;
        }
        pause(cx, 100).await;
    }
    check("completions: collections after db., fields inside a filter", first == ["people"] && fields.contains(&"age".to_string()), format!("{first:?} {fields:?}"));

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| {
        w.new_tab(window, cx);
    }));
    let t2 = active(ws, cx).unwrap();
    let on_mongo = cx.update(|_, cx| (t2.read(cx).connection().map(|c| c.profile.id.clone()), t2.read(cx).doc.is_some())).unwrap();
    check("⌘T on the MongoDB tab: a documents tab on MongoDB", on_mongo == (Some(mid.clone()), true), format!("{on_mongo:?}"));
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.pick_target(&pid, &pg.database, window, cx)));
    let sql_now = cx.update(|_, cx| (t2.read(cx).connection().map(|c| c.engine()), t2.read(cx).doc.is_some(), t2.read(cx).editor.read(cx).doc_lang())).unwrap();
    check("the picker moves a tab to PostgreSQL: SQL again, the grid's view", sql_now == (Some(Engine::Postgres), false, None), format!("{sql_now:?}"));
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| {
        w.new_tab(window, cx);
    }));
    let t3 = active(ws, cx).unwrap();
    let on_pg = cx.update(|_, cx| (t3.read(cx).connection().map(|c| c.engine()), t3.read(cx).doc.is_some())).unwrap();
    check("⌘T now: a SQL tab on PostgreSQL", on_pg == (Some(Engine::Postgres), false), format!("{on_pg:?}"));
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.pick_target(&mid, DB, window, cx)));
    let doc_now = cx.update(|_, cx| (t3.read(cx).connection().map(|c| c.engine()), t3.read(cx).doc.is_some(), t3.read(cx).editor.read(cx).doc_lang())).unwrap();
    check("and back to MongoDB from the picker: documents and the shell's language", doc_now == (Some(Engine::MongoDb), true, Some(Engine::MongoDb)), format!("{doc_now:?}"));
    let _ = cx.update(|_, cx| {
        let editor = t3.read(cx).editor.clone();
        editor.update(cx, |e, cx| {
            e.set_text("use admin", cx);
            e.run_script(cx);
        });
    });
    until(cx, 5, |cx| idle(ws, cx)).await;
    let db_now = cx.update(|_, cx| t3.read(cx).database().to_string()).unwrap_or_default();
    check("use admin moves the tab to that database", db_now == "admin", db_now);

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dialog(None, window, cx)));
    draw(cx);
    let d = cx.update(|_, cx| ws.read(cx).dialog.clone()).ok().flatten();
    if let Some(d) = d {
        let step = cx.update(|_, cx| d.read(cx).step()).ok();
        let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_search("redi", cx)));
        key(cx, "enter");
        draw(cx);
        let (step2, p) = cx.update(|_, cx| (d.read(cx).step(), d.read(cx).current(cx).0)).unwrap();
        check(
            "the connection sheet starts with the engines; a search and ↩ open Redis' form",
            step == Some(crate::conn_dialog::Step::Engine) && step2 == crate::conn_dialog::Step::Form && p.engine == Engine::Redis && p.port == 6379,
            format!("{step:?} {step2:?} {:?} {}", p.engine, p.port),
        );
        key(cx, "escape");
        pause(cx, 100).await;
    }
    let (es, epw) = es_profile();
    let eid = es.id.clone();
    if let Err(e) = seed_es(&core, es.clone(), epw.clone(), cx).await {
        check("the Elasticsearch test server answers", false, format!("{e:#}"));
        return false;
    }
    let _ = cx.update(|_, cx| crate::bench::connect(ws, es.clone(), epw.clone(), cx));
    until(cx, 20, |cx| connected(ws, &eid, cx)).await;
    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&format!("c:{eid}"), cx)));
    let listed = until(cx, 10, |cx| cx.update(|_, cx| tree.read(cx).demo_children(&format!("c:{eid}"))).unwrap_or_default().iter().any(|n| n == INDEX)).await;
    let logo = cx.update(|_, cx| tree.read(cx).demo_logo(&eid)).ok().flatten();
    check("Elasticsearch's indices list under it, its logo on its row", listed && logo == Some(Engine::Elasticsearch), format!("{logo:?}"));
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_collection(&eid, "", INDEX, window, cx)));
    until(cx, 10, |cx| idle(ws, cx) && shown(ws, cx).is_some()).await;
    let s = shown(ws, cx);
    let lang = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.read(cx).editor.read(cx).doc_lang())).ok().flatten().flatten();
    check(
        "an index opens as documents in the console's language: the first 50 of 75",
        lang == Some(Engine::Elasticsearch) && s.as_ref().is_some_and(|(sql, n, total, more)| sql.starts_with(&format!("GET /{INDEX}/_search")) && *n == 50 && *total == Some(75) && *more),
        format!("{s:?} {lang:?}"),
    );
    if let Some(doc) = cx.update(|_, cx| ws.read(cx).active_tab().and_then(|t| t.read(cx).doc.clone())).ok().flatten() {
        draw(cx);
        if let Some(d) = cx.update(|_, cx| doc.read(cx).demo_detail()).ok().flatten() {
            let text = cx.update(|_, cx| d.read(cx).text().to_string()).unwrap_or_default();
            let _ = cx.update(|_, cx| {
                d.update(cx, |e, cx| e.set_text(&text.replace("\"shipped\"", "\"returned\"").replace("\"paid\"", "\"returned\"").replace("\"new\"", "\"returned\""), cx));
                doc.update(cx, |p, cx| p.demo_save(cx));
            });
            let saved = until(cx, 10, |cx| cx.update(|_, cx| doc.read(cx).demo_note()).ok().flatten().is_some_and(|n| n.0 == "Saved")).await;
            until(cx, 5, |cx| idle(ws, cx)).await;
            draw(cx);
            let page = |cx: &mut AsyncWindowContext| {
                cx.update(|_, cx| {
                    let d = doc.read(cx);
                    let (_, r) = d.shown()?;
                    let docs = match &r.ok()?.output {
                        Output::Docs(docs) => docs.clone(),
                        _ => return None,
                    };
                    let detail = d.demo_detail().map(|e| e.read(cx).text().to_string()).unwrap_or_default();
                    Some((docs, d.demo_selected(), detail))
                })
                .ok()
                .flatten()
            };
            let after = page(cx);
            let kept = after.as_ref().is_some_and(|(d, sel, detail)| {
                d.docs.len() == 50 && *sel == Some(0) && d.docs[0].body.text().contains("returned") && detail.contains("returned")
            });
            check("an edited hit stays on the page, selected, as saved (Elasticsearch reorders what it reindexes)", saved && kept, format!("{:?}", after.as_ref().map(|(d, s, _)| (d.docs.len(), *s, d.docs.first().map(|x| x.body.text())))));
            let _ = cx.update(|window, cx| doc.update(cx, |p, cx| p.demo_new(window, cx)));
            draw(cx);
            let _ = cx.update(|_, cx| {
                d.update(cx, |e, cx| e.set_text("{\"n\": 1000, \"status\": \"inserted\"}", cx));
                doc.update(cx, |p, cx| p.demo_save(cx));
            });
            let inserted = until(cx, 10, |cx| cx.update(|_, cx| doc.read(cx).demo_note()).ok().flatten().is_some_and(|n| n.0 == "Inserted")).await;
            draw(cx);
            let after = page(cx);
            let first = after.as_ref().is_some_and(|(d, sel, _)| *sel == Some(0) && d.docs[0].body.text().contains("inserted") && d.total == Some(76));
            check("a new hit shows first on the page, selected, counted", inserted && first, format!("{:?}", after.as_ref().map(|(d, s, _)| (d.total, *s, d.docs.first().map(|x| x.body.text())))));
            let tab = active(ws, cx).unwrap();
            let _ = cx.update(|_, cx| {
                let editor = tab.read(cx).editor.clone();
                editor.update(cx, |e, cx| {
                    e.set_text(&format!("GET /{INDEX}/_count\n{{\"query\": {{\"term\": {{\"status\": \"returned\"}}}}}}"), cx);
                    e.run_script(cx);
                });
            });
            until(cx, 10, |cx| idle(ws, cx)).await;
            let count = cx
                .update(|_, cx| {
                    let d = ws.read(cx).active_tab()?.read(cx).doc.clone()?;
                    let (_, r) = d.read(cx).shown()?;
                    match &r.ok()?.output {
                        Output::Value(dataman_core::nosql::Body::Json(v)) => v["count"].as_u64(),
                        _ => None,
                    }
                })
                .ok()
                .flatten();
            check("a hit edited and saved goes to its index: the count finds it", saved && count == Some(1), format!("saved {saved}, count {count:?} {text}"));
        }
    }

    let (rd, rpw) = redis_profile();
    let rid = rd.id.clone();
    if let Err(e) = seed_redis(&core, rd.clone(), rpw.clone(), cx).await {
        check("the Redis test server answers", false, format!("{e:#}"));
        return false;
    }
    let _ = cx.update(|_, cx| crate::bench::connect(ws, rd.clone(), rpw.clone(), cx));
    until(cx, 20, |cx| connected(ws, &rid, cx)).await;
    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&format!("c:{rid}"), cx)));
    let db7 = format!("c:{rid}/d:7");
    let keys_listed = until(cx, 10, |cx| cx.update(|_, cx| tree.read(cx).demo_children(&db7)).unwrap_or_default().len() >= 5).await;
    let keys = cx.update(|_, cx| tree.read(cx).demo_children(&db7)).unwrap_or_default();
    check("Redis' database opens on its keys", keys_listed && keys == ["feature:dark-mode", "jobs:queue", "leaderboard", "user:1", "user:2"], format!("{keys:?}"));
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_collection(&rid, "7", "user:1", window, cx)));
    until(cx, 10, |cx| idle(ws, cx) && shown(ws, cx).is_some()).await;
    let s = shown(ws, cx);
    check("a hash opens as its fields, one entry each", s.as_ref().is_some_and(|(sql, n, total, _)| sql == "HGETALL user:1" && *n == 4 && *total == Some(4)), format!("{s:?}"));
    if let Some(doc) = cx.update(|_, cx| ws.read(cx).active_tab().and_then(|t| t.read(cx).doc.clone())).ok().flatten() {
        draw(cx);
        if let Some(d) = cx.update(|_, cx| doc.read(cx).demo_detail()).ok().flatten() {
            let text = cx.update(|_, cx| d.read(cx).text().to_string()).unwrap_or_default();
            let edited = serde_json::from_str::<serde_json::Value>(&text).ok().map(|mut v| {
                v["value"] = serde_json::json!("changed");
                serde_json::to_string_pretty(&v).unwrap_or_default()
            });
            let field = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v["field"].as_str().map(str::to_string)).unwrap_or_default();
            if let Some(edited) = edited {
                let _ = cx.update(|_, cx| {
                    d.update(cx, |e, cx| e.set_text(&edited, cx));
                    doc.update(cx, |p, cx| p.demo_save(cx));
                });
            }
            let saved = until(cx, 10, |cx| cx.update(|_, cx| doc.read(cx).demo_note()).ok().flatten().is_some_and(|n| n.0 == "Saved")).await;
            until(cx, 5, |cx| idle(ws, cx)).await;
            let (rd2, rpw2, core2) = (rd.clone(), rpw.clone(), core.clone());
            let back = cx
                .background_executor()
                .spawn(async move {
                    let c = core2.block_on(core2.connect(rd2, Some(rpw2))).ok()?;
                    match core2.block_on(c.run_nosql("7", &format!("HGET user:1 {field}"), Page::default())).ok()?.output {
                        Output::Value(dataman_core::nosql::Body::Json(v)) => v.as_str().map(str::to_string),
                        _ => None,
                    }
                })
                .await;
            check("an entry edited and saved is the hash's field now", saved && back.as_deref() == Some("changed"), format!("saved {saved}, {back:?} from {text}"));
        }
    }
    // A favorite on Redis opens as it was saved: no `;` (it would be part of the key).
    let mut fav = dataman_core::Favorite::new("nosql-check key", "HGETALL user:1");
    fav.connection_id = Some(rid.clone());
    fav.database = Some("7".into());
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_favorite(fav, false, window, cx)));
    let text = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.read(cx).editor.read(cx).text().to_string())).ok().flatten().unwrap_or_default();
    check("a NoSQL favorite opens without a ;", text == "HGETALL user:1", text);

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_collection(&mid, DB, "people", window, cx)));
    until(cx, 10, |cx| idle(ws, cx) && shown(ws, cx).is_some()).await;
    let (out, _) = run_text(ws, "db.people.find({name: 'PERSON 5'}).collation({locale: 'en', strength: 2})", cx).await;
    let n = match &out {
        Some(Ok(Output::Docs(d))) => d.docs.len(),
        _ => 0,
    };
    check("a find's collation applies (case-insensitive match)", n == 1, format!("{out:?}"));
    let (out, detail) = run_text(ws, "db.people.findOneAndUpdate({_id: 5}, {$set: {age: NumberLong(99)}}, {returnDocument: 'after'})", cx).await;
    let ok_ = matches!(&out, Some(Ok(Output::Written { affected: 1, .. }))) && detail.contains("NumberLong(99)");
    check("findOneAndUpdate returns the updated document", ok_, format!("{out:?} {detail}"));
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_collection(&eid, "", INDEX, window, cx)));
    until(cx, 10, |cx| idle(ws, cx) && shown(ws, cx).is_some()).await;
    let (_, detail) = run_text(ws, &format!("GET /{INDEX}/_search\n{{\"size\": 0, \"aggs\": {{\"s\": {{\"terms\": {{\"field\": \"status\"}}}}}}}}"), cx).await;
    check("a search's aggregations come first", detail.trim_start().starts_with("{\n  \"aggregations\""), detail.chars().take(80).collect());
    let (out, detail) = run_text(ws, &format!("GET /{INDEX}/_search\n{{\"query\": {{\"match\": {{\"customer.name\": \"customer\"}}}}, \"highlight\": {{\"fields\": {{\"customer.name\": {{}}}}}}}}"), cx).await;
    let read_only = matches!(&out, Some(Ok(Output::Docs(d))) if d.source.is_none());
    check("highlights show beside the source, read-only", read_only && detail.contains("_highlight") && detail.contains("<em>"), detail.chars().take(80).collect());
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_collection(&rid, "7", "user:2", window, cx)));
    until(cx, 10, |cx| idle(ws, cx) && shown(ws, cx).is_some()).await;
    let (_, detail) = run_text(ws, "INFO keyspace", cx).await;
    check("text replies show as text (INFO)", detail.starts_with("# Keyspace\n"), detail.chars().take(60).collect());

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_collection(&mid, DB, "people", window, cx)));
    until(cx, 10, |cx| idle(ws, cx) && shown(ws, cx).is_some()).await;
    let script = "let total = 0\ndb.people.find({}).forEach(p => { total += Number(p.age) })\nprint('ages', total)\ndb.marks.insertOne({at: new Date()})\ndb.people.find({}).sort({_id: 1})";
    let one = dataman_core::nosql::statements(Engine::MongoDb, script).len() == 1;
    let (out, _) = run_text(ws, script, cx).await;
    let ran = cx.update(|_, cx| ws.read(cx).active_tab()?.read(cx).doc.as_ref()?.read(cx).shown()?.1.ok().cloned()).ok().flatten();
    let printed = ran.as_ref().map(|r| r.printed.clone()).unwrap_or_default();
    let page_one = matches!(&out, Some(Ok(Output::Docs(d))) if d.docs.len() == 50 && d.more && d.source.is_some());
    check("a script runs as one: its print over its final cursor's page (editable)", one && page_one && printed.len() == 1 && printed[0].starts_with("ages "), format!("{one} {printed:?} {out:?}"));
    if let Some(doc) = cx.update(|_, cx| ws.read(cx).active_tab()?.read(cx).doc.clone()).ok().flatten() {
        let _ = cx.update(|_, cx| doc.update(cx, |d, cx| d.demo_page(50, cx)));
        pause(cx, 50).await;
        until(cx, 10, |cx| idle(ws, cx)).await;
    }
    let next = shown(ws, cx);
    let marks = count(&core, &mongo, &mpw, "db.marks.countDocuments({})", cx).await;
    check("its next page reads the cursor again: the script's write ran once", next.as_ref().is_some_and(|s| s.1 > 0 && !s.3) && marks.as_deref() == Some("1"), format!("{next:?} marks {marks:?}"));
    let (out, _) = run_text(ws, "let check = 'ping'\nuse admin\ndb.runCommand({ping: 1}).ok", cx).await;
    let db_now = active(ws, cx).map(|t| cx.update(|_, cx| t.read(cx).database().to_string()).unwrap_or_default()).unwrap_or_default();
    check("use in a script moves the tab", db_now == "admin" && matches!(&out, Some(Ok(Output::Value(_)))), format!("{db_now} {out:?}"));
    let (out, _) = run_text(ws, "let x = 1\nlet = = 2", cx).await;
    check("a script's syntax error says so", matches!(&out, Some(Err(m)) if m.contains("SyntaxError")), format!("{out:?}"));

    let _ = cx.update(|_, cx| {
        if let Some(t) = ws.read(cx).active_tab().cloned() {
            t.update(cx, |t, cx| t.set_database(DB.to_string(), cx));
        }
    });
    if let Some(ipc) = crate::mcp_check::serve(ws, &mid, cx).await {
        let name = mongo.name.clone();
        let agent_tab = |cx: &mut AsyncWindowContext| {
            cx.update(|_, cx| ws.read(cx).tabs.iter().find(|t| t.read(cx).agent.as_ref().is_some_and(|a| a.connection_id == mid)).cloned()).ok().flatten()
        };
        let tab_shows = |cx: &mut AsyncWindowContext| -> (String, Option<Result<Output, String>>) {
            let Some(t) = agent_tab(cx) else { return (String::new(), None) };
            cx.update(|_, cx| {
                let t = t.read(cx);
                let out = t.doc.as_ref().and_then(|d| d.read(cx).shown().map(|(_, r)| r.map(|r| r.output.clone()).map_err(|f| f.message.clone())));
                (t.editor.read(cx).text().to_string(), out)
            })
            .unwrap_or_default()
        };
        let read = "const old = 40\ndb.people.find({age: {$gte: old}}).sort({_id: 1})";
        let (err, out) = crate::mcp_check::call(&ipc, "open_query", serde_json::json!({"connection": name, "database": DB, "sql": read, "max_rows": 5})).await;
        let v: serde_json::Value = serde_json::from_str(&out).unwrap_or_default();
        draw(cx);
        let (text, on_tab) = tab_shows(cx);
        let tab_docs = matches!(&on_tab, Some(Ok(Output::Docs(d))) if d.docs.len() == 5 && d.more);
        check("open_query on MongoDB: the agent's documents, and its tab's document view shows them", !err && v["count"] == 5 && v["in_tab"] == true && text == read && tab_docs, format!("{err} {out} | {text:?} {on_tab:?}"));
        let (err, out) = crate::mcp_check::call(&ipc, "open_query", serde_json::json!({"connection": name, "database": DB, "sql": "db.marks.insertOne({by: 'agent'})"})).await;
        draw(cx);
        let (_, on_tab) = tab_shows(cx);
        check("a write (tier full): runs unasked, the tab shows what it wrote", !err && out.contains("\"affected\":1") && matches!(&on_tab, Some(Ok(Output::Written { affected: 1, .. }))), format!("{err} {out} {on_tab:?}"));
        let rx = crate::mcp_check::start(&ipc, "open_query", serde_json::json!({"connection": name, "database": DB, "sql": "for (const i of [1, 2]) db.marks.insertOne({i})\ndb.marks.countDocuments({})"}));
        let asked = crate::mcp_check::sheet(cx).await;
        let why = asked.as_ref().map(|a| a.reasons.join(", ")).unwrap_or_default();
        check("a script's writes wait for the user (they cannot be counted first)", why.contains("a script"), why);
        crate::mcp_check::answer(ws, true, cx);
        let (err, out) = rx.await.unwrap_or((true, "gone".into()));
        draw(cx);
        let (_, on_tab) = tab_shows(cx);
        check("Allow: the script runs, its writes listed, its value in the tab", !err && out.contains("\"value\":4") && out.contains("\"writes\"") && matches!(&on_tab, Some(Ok(Output::Value(_)))), format!("{err} {out} {on_tab:?}"));
        let (err, out) = crate::mcp_check::call(&ipc, "open_query", serde_json::json!({"connection": name, "database": DB, "sql": "db.marks.deleteMany({})", "run": false})).await;
        draw(cx);
        let (text, on_tab) = tab_shows(cx);
        let marks = count(&core, &mongo, &mpw, "db.marks.countDocuments({})", cx).await;
        check("a draft (run: false): the text in the tab, nothing run, nothing shown", !err && out.contains("drafted") && text == "db.marks.deleteMany({})" && on_tab.is_none() && marks.as_deref() == Some("4"), format!("{err} {out} {text:?} {on_tab:?} {marks:?}"));
    }

    crate::assets::set_logos(false);
    let hidden = crate::assets::small_logo(Engine::MongoDb, 13.).is_none();
    crate::assets::set_logos(true);
    check("Settings › Database logos hides them", hidden && crate::assets::small_logo(Engine::MongoDb, 13.).is_some(), String::new());

    core.block_on(async {
        if let Ok(c) = core.connect(mongo.clone(), Some(mpw.clone())).await {
            let _ = c.run_nosql(DB, "db.dropDatabase()", Page::default()).await;
        }
        if let Ok(c) = core.connect(es.clone(), Some(epw.clone())).await {
            let _ = c.run_nosql("", &format!("DELETE /{INDEX}"), Page::default()).await;
        }
        if let Ok(c) = core.connect(rd.clone(), Some(rpw.clone())).await {
            let _ = c.run_nosql("7", "FLUSHDB", Page::default()).await;
        }
    });
    ok
}
