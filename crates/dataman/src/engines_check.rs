//! `nosql-engines-check`: what nosql-check leaves out. Redis' other types (string, list, set,
//! sorted set, stream) opened from the explorer and edited with ⌘↩ in the detail; OpenSearch in
//! the document view, an edited hit saved, an aggregation typed in the console; Elasticsearch's
//! _cat text; MongoDB shell errors and ⌘. on a running script.

use std::time::{Duration, Instant};

use gpui::{AsyncWindowContext, Entity, Focusable};
use dataman_core::config::Engine;
use dataman_core::nosql::{Body, Output};

use crate::app::Workspace;
use crate::check::*;
use crate::doc_view::DocPane;

fn doc(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Entity<DocPane>> {
    cx.update(|_, cx| ws.read(cx).active_tab().and_then(|t| t.read(cx).doc.clone())).ok().flatten()
}

/// The statement shown and its output (or the server's error).
fn shown(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<(String, Result<Output, String>)> {
    let d = doc(ws, cx)?;
    cx.update(|_, cx| d.read(cx).shown().map(|(s, r)| (s.to_string(), r.map(|r| r.output.clone()).map_err(|f| f.message.clone())))).ok().flatten()
}

fn docs(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> usize {
    match shown(ws, cx) {
        Some((_, Ok(Output::Docs(d)))) => d.docs.len(),
        _ => 0,
    }
}

fn detail(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<(Entity<crate::editor::CodeEditor>, String)> {
    let d = doc(ws, cx)?;
    draw(cx);
    let e = cx.update(|_, cx| d.read(cx).demo_detail()).ok().flatten()?;
    let text = cx.update(|_, cx| e.read(cx).text().to_string()).unwrap_or_default();
    Some((e, text))
}

async fn open(ws: &Entity<Workspace>, conn: &str, db: &str, item: &str, cx: &mut AsyncWindowContext) -> Option<String> {
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_collection(conn, db, item, window, cx)));
    let tab = active(ws, cx)?;
    pause(cx, 50).await;
    until(cx, 10, |cx| idle(&tab, cx) && shown(ws, cx).is_some()).await;
    shown(ws, cx).map(|s| s.0)
}

/// The detail's JSON with `field` set to `value`, typed over with ⌘A, saved with ⌘↩.
async fn edit_save(ws: &Entity<Workspace>, field: &str, value: serde_json::Value, cx: &mut AsyncWindowContext) -> bool {
    // The detail settles on the first entry once the page is in.
    pause(cx, 400).await;
    let Some((e, text)) = detail(ws, cx) else { return false };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else {
        eprintln!("check: the detail is not JSON: {text}");
        return false;
    };
    v[field] = value;
    let edited = serde_json::to_string_pretty(&v).unwrap();
    let _ = cx.update(|window, cx| {
        e.update(cx, |e, cx| e.set_text(&edited, cx));
        window.focus(&e.focus_handle(cx), cx);
    });
    draw(cx);
    if cx.update(|_, cx| e.read(cx).text() != edited).unwrap_or(true) {
        eprintln!("check: the detail changed under the edit");
    }
    key(cx, "cmd-enter");
    let Some(d) = doc(ws, cx) else { return false };
    let by_key = until(cx, 5, |cx| cx.update(|_, cx| d.read(cx).demo_note()).ok().flatten().is_some_and(|n| n.0 == "Saved")).await;
    if !by_key {
        let focused = cx.update(|window, cx| e.focus_handle(cx).is_focused(window)).unwrap_or(false);
        let _ = cx.update(|_, cx| d.update(cx, |p, cx| p.demo_save(cx)));
        let direct = until(cx, 10, |cx| cx.update(|_, cx| d.read(cx).demo_note()).ok().flatten().is_some()).await;
        let note = cx.update(|_, cx| d.read(cx).demo_note()).ok().flatten();
        eprintln!("check: ⌘↩ did not save (detail focused {focused}); Save itself: {direct} {note:?}");
    }
    by_key
}

/// Types into the tab's editor and runs it with ⌘↩.
async fn console(ws: &Entity<Workspace>, text: &str, cx: &mut AsyncWindowContext) -> Option<(String, Result<Output, String>)> {
    run_keys(ws, text, "cmd-enter", cx).await?;
    shown(ws, cx)
}

fn value_text(o: &Result<Output, String>) -> String {
    match o {
        Ok(Output::Value(Body::Json(v))) => v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()),
        Ok(Output::Value(b)) => b.inline(),
        Ok(Output::Docs(d)) => d.docs.first().map(|d| match &d.body {
            Body::Json(v) => v["value"].as_str().map(str::to_string).unwrap_or_else(|| v.to_string()),
            b => b.inline(),
        }).unwrap_or_default(),
        Ok(o) => format!("{o:?}"),
        Err(e) => e.clone(),
    }
}

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    redis(ws, r, cx).await;
    opensearch(ws, r, cx).await;
    elastic_and_mongo(ws, r, cx).await;
}

async fn redis(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (mut p, pw) = profile(Engine::Redis);
    p.database = "8".into();
    let core = core(ws, cx);
    for cmd in ["FLUSHDB", "SET s:greeting hello", "RPUSH l:queue a b c", "SADD set:tags x y z", "ZADD z:scores 1 ann 2 bob", "XADD st:events * kind login", "SET bin \"\\xff\\x00\""] {
        if let Err(e) = nosql(&core, &p, &pw, "8", cmd, cx).await {
            r.check("Redis: the test keys are written", false, format!("{cmd}: {e}"));
            return;
        }
    }
    let up = connect(ws, &p, &pw, cx).await;
    if !r.check("Redis connects", up.is_ok(), format!("{up:?}")) {
        return;
    }
    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&format!("c:{}", p.id), cx)));
    let db = format!("c:{}/d:8", p.id);
    until(cx, 10, |cx| cx.update(|_, cx| tree.read(cx).demo_children(&db)).unwrap_or_default().len() >= 6).await;
    let keys = cx.update(|_, cx| tree.read(cx).demo_children(&db)).unwrap_or_default();
    r.check("Redis: every type of key lists in the explorer", ["bin", "l:queue", "s:greeting", "set:tags", "st:events", "z:scores"].iter().all(|k| keys.iter().any(|l| l == k)), format!("{keys:?}"));

    for (key, kind, n) in [("l:queue", "list", 3usize), ("set:tags", "set", 3), ("z:scores", "sorted set", 2)] {
        let stmt = open(ws, &p.id, "8", key, cx).await.unwrap_or_default();
        let got = docs(ws, cx);
        r.check(&format!("Redis: a {kind} opens as its {n} entries"), got == n, format!("{stmt} {got}"));
    }
    let stmt = open(ws, &p.id, "8", "st:events", cx).await.unwrap_or_default();
    let out = shown(ws, cx).map(|s| value_text(&s.1)).unwrap_or_default();
    r.check("Redis: a stream opens as its entries' JSON (XRANGE)", stmt.starts_with("XRANGE") && out.contains("login"), format!("{stmt} {out}"));

    let stmt = open(ws, &p.id, "8", "s:greeting", cx).await.unwrap_or_default();
    let saved = edit_save(ws, "value", serde_json::json!("bonjour"), cx).await;
    let back = value_text(&nosql(&core, &p, &pw, "8", "GET s:greeting", cx).await);
    r.check("Redis: a string edited in the detail, ⌘↩ saves it", saved && back == "bonjour", format!("{stmt} saved {saved} {back}"));

    open(ws, &p.id, "8", "l:queue", cx).await;
    let _ = doc(ws, cx).map(|d| cx.update(|_, cx| d.update(cx, |d, cx| d.demo_select(1, cx))));
    let saved = edit_save(ws, "value", serde_json::json!("B"), cx).await;
    let back = value_text(&nosql(&core, &p, &pw, "8", "LINDEX l:queue 1", cx).await);
    r.check("Redis: a list entry edited and saved (LSET)", saved && back == "B", format!("saved {saved} {back}"));

    open(ws, &p.id, "8", "z:scores", cx).await;
    let saved = edit_save(ws, "score", serde_json::json!(7), cx).await;
    let back = value_text(&nosql(&core, &p, &pw, "8", "ZSCORE z:scores ann", cx).await);
    r.check("Redis: a sorted set's score edited and saved", saved && back.trim_matches('"') == "7", format!("saved {saved} {back}"));

    open(ws, &p.id, "8", "bin", cx).await;
    let text = detail(ws, cx).map(|d| d.1).unwrap_or_default();
    r.check("Redis: a binary value shows its bytes escaped", text.contains("\\xff") || text.contains("\\\\xff"), &text);
}

async fn opensearch(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (p, pw) = profile(Engine::OpenSearch);
    let core = core(ws, cx);
    const INDEX: &str = "dataman_e2e_os";
    let _ = nosql(&core, &p, &pw, "", &format!("DELETE /{INDEX}"), cx).await;
    let mut bulk = format!("POST /{INDEX}/_bulk?refresh=true\n");
    for i in 1..=30 {
        bulk.push_str(&format!("{{\"index\": {{\"_id\": \"{i}\"}}}}\n{{\"n\": {i}, \"kind\": \"{}\"}}\n", ["a", "b", "c"][i % 3]));
    }
    let seeded = nosql(&core, &p, &pw, "", &format!("PUT /{INDEX}\n{{\"mappings\": {{\"properties\": {{\"n\": {{\"type\": \"integer\"}}, \"kind\": {{\"type\": \"keyword\"}}}}}}}}"), cx).await.and(nosql(&core, &p, &pw, "", &bulk, cx).await);
    if !r.check("OpenSearch: the test index is written", seeded.is_ok(), format!("{seeded:?}")) {
        return;
    }
    let up = connect(ws, &p, &pw, cx).await;
    if !r.check("OpenSearch connects", up.is_ok(), format!("{up:?}")) {
        return;
    }
    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(&format!("c:{}", p.id), cx)));
    let listed = until(cx, 10, |cx| cx.update(|_, cx| tree.read(cx).demo_row_ids(&format!("c:{}", p.id))).unwrap_or_default().iter().any(|i| i.ends_with(INDEX))).await;
    r.check("OpenSearch: its indices list in the explorer", listed, "");
    let stmt = open(ws, &p.id, "", INDEX, cx).await.unwrap_or_default();
    let n = docs(ws, cx);
    r.check("OpenSearch: an index opens in the document view", n > 0, format!("{stmt} {n}"));
    let saved = edit_save(ws, "kind", serde_json::json!("edited"), cx).await;
    let note = doc(ws, cx).and_then(|d| cx.update(|_, cx| (d.read(cx).demo_note(), d.read(cx).has_confirm())).ok());
    let last = shown(ws, cx).map(|s| (s.0, s.1.map(|o| format!("{o:?}").chars().take(200).collect::<String>())));
    let body = detail(ws, cx).map(|d| d.1).unwrap_or_default();
    let count = value_text(&nosql(&core, &p, &pw, "", &format!("GET /{INDEX}/_count?q=kind:edited"), cx).await);
    r.check("OpenSearch: a hit edited and saved is in the index", saved && count.contains("\"count\":1"), format!("saved {saved} {note:?} {last:?} {count} {body}"));
    let out = console(ws, &format!("POST /{INDEX}/_search\n{{\"size\": 0, \"aggs\": {{\"kinds\": {{\"terms\": {{\"field\": \"kind\"}}}}}}}}"), cx).await;
    let text = detail(ws, cx).map(|d| d.1).unwrap_or_default();
    r.check("OpenSearch: an aggregation typed in the console runs; its buckets come first", text.trim_start().starts_with("{\n  \"aggregations\"") && text.contains("\"buckets\""), format!("{:?} {}", out.map(|o| o.0), text.chars().take(120).collect::<String>()));
}

async fn elastic_and_mongo(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (p, pw) = profile(Engine::Elasticsearch);
    let up = connect(ws, &p, &pw, cx).await;
    if r.check("Elasticsearch connects", up.is_ok(), format!("{up:?}")) {
        let _ = cx.update(|window, cx| {
            ws.update(cx, |w, cx| {
                w.current_conn = Some(p.id.clone());
                w.new_tab(window, cx);
            })
        });
        let out = console(ws, "GET /_cat/health?v", cx).await;
        let text = out.as_ref().map(|o| value_text(&o.1)).unwrap_or_default();
        r.check("Elasticsearch: _cat answers as text in the console", text.contains("status") && text.contains("cluster"), &text);
    }

    let (m, mpw) = profile(Engine::MongoDb);
    let up = connect(ws, &m, &mpw, cx).await;
    if !r.check("MongoDB connects", up.is_ok(), format!("{up:?}")) {
        return;
    }
    let _ = cx.update(|window, cx| {
        ws.update(cx, |w, cx| {
            w.current_conn = Some(m.id.clone());
            w.new_tab(window, cx);
        })
    });
    let out = console(ws, "db.people.find({ age: })", cx).await;
    r.check("MongoDB: a statement the shell cannot read says why", out.as_ref().is_some_and(|o| o.1.as_ref().is_err_and(|e| !e.is_empty())), format!("{out:?}"));
    let tab = active(ws, cx).unwrap();
    let _ = cx.update(|window, cx| {
        let e = tab.read(cx).editor.clone();
        e.update(cx, |e, cx| {
            e.set_text("for (let i = 0; i < 100; i++) { sleep(200) }\nprint('done')", cx);
            e.set_cursor(0, cx);
        });
        window.focus(&e.focus_handle(cx), cx);
    });
    key(cx, "cmd-enter");
    pause(cx, 600).await;
    let running = !idle(&tab, cx);
    let t0 = Instant::now();
    key(cx, "cmd-.");
    let stopped = until(cx, 5, |cx| idle(&tab, cx)).await;
    r.check("MongoDB: ⌘. stops a running script at once", running && stopped && t0.elapsed() < Duration::from_secs(3), format!("running {running} stopped {stopped} after {:?}", t0.elapsed()));
}
