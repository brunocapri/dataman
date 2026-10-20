//! `tabs-check`: which connection a new tab (⌘T) gets, and the toolbar's connection picker
//! moving a tab between SQL and the document view, all with real clicks and keys.

use gpui::{AsyncWindowContext, Entity};
use dataman_core::config::Engine;

use crate::app::Workspace;
use crate::check::*;
use crate::tab::QueryTab;

/// Engine, document view, the editor's document language.
fn view(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> (Option<Engine>, bool, Option<Engine>) {
    cx.update(|_, cx| (tab.read(cx).connection().map(|c| c.engine()), tab.read(cx).doc.is_some(), tab.read(cx).editor.read(cx).doc_lang())).unwrap_or_default()
}

/// Opens the active tab's connection picker with a click and clicks `id`/`db` in it.
async fn pick(ws: &Entity<Workspace>, id: &str, db: &str, cx: &mut AsyncWindowContext) -> bool {
    draw(cx);
    let Some(b) = cx.update(|_, cx| ws.read(cx).picker_bounds.get()).ok().flatten() else { return false };
    click(cx, centre(b));
    pause(cx, 200).await;
    let ok = click_probe(&format!("pick:{id}:{db}"), cx);
    pause(cx, 300).await;
    ok
}

fn new_tab_key(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Option<Entity<QueryTab>> {
    let before = tabs(ws, cx).len();
    key(cx, "cmd-t");
    let t = active(ws, cx)?;
    (tabs(ws, cx).len() == before + 1).then_some(t)
}

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let mut ids = Vec::new();
    for e in [Engine::Postgres, Engine::MongoDb, Engine::Redis] {
        let (p, pw) = profile(e);
        let up = connect(ws, &p, &pw, cx).await;
        r.check(&format!("{} connects", e.label()), up.is_ok(), format!("{up:?}"));
        ids.push((p.id.clone(), p.database.clone()));
    }
    let [(pg, pg_db), (mongo, mongo_db), (redis, redis_db)] = <[(String, String); 3]>::try_from(ids).unwrap();
    pause(cx, 300).await;

    let t = active(ws, cx).unwrap();
    let picked = pick(ws, &pg, &pg_db, cx).await;
    r.check("the toolbar's picker puts a tab on PostgreSQL: the SQL grid", picked && view(&t, cx) == (Some(Engine::Postgres), false, None), format!("{picked} {:?}", view(&t, cx)));
    let t1 = new_tab_key(ws, cx);
    let on = t1.as_ref().map(|t| (tab_conn(t, cx), view(t, cx)));
    r.check("⌘T from a PostgreSQL tab: a SQL tab on PostgreSQL", on == Some((Some(pg.clone()), (Some(Engine::Postgres), false, None))), format!("{on:?}"));

    let picked = pick(ws, &mongo, &mongo_db, cx).await;
    let t1 = t1.unwrap_or(t);
    r.check(
        "the picker moves it to MongoDB: the document view and the shell's language",
        picked && view(&t1, cx) == (Some(Engine::MongoDb), true, Some(Engine::MongoDb)),
        format!("{picked} {:?}", view(&t1, cx)),
    );
    let t2 = new_tab_key(ws, cx);
    let on = t2.as_ref().map(|t| (tab_conn(t, cx), view(t, cx).1));
    r.check("⌘T from the MongoDB tab: a document tab on MongoDB", on == Some((Some(mongo.clone()), true)), format!("{on:?}"));

    let picked = pick(ws, &redis, &redis_db, cx).await;
    let t2 = t2.unwrap_or(t1.clone());
    r.check(
        "…then to Redis: still documents, in Redis' command language",
        picked && view(&t2, cx) == (Some(Engine::Redis), true, Some(Engine::Redis)),
        format!("{picked} {:?}", view(&t2, cx)),
    );
    let picked = pick(ws, &pg, &pg_db, cx).await;
    r.check("…and back to PostgreSQL: the grid again, no document pane", picked && view(&t2, cx) == (Some(Engine::Postgres), false, None), format!("{picked} {:?}", view(&t2, cx)));

    let _ = cx.update(|window, cx| {
        let tab = t1.clone();
        ws.update(cx, |w, cx| w.activate_tab(&tab, window, cx))
    });
    let t3 = new_tab_key(ws, cx);
    let on = t3.as_ref().and_then(|t| tab_conn(t, cx));
    r.check("activating the MongoDB tab, then ⌘T: MongoDB", on.as_deref() == Some(mongo.as_str()), format!("{on:?}"));

    let tree = cx.update(|_, cx| ws.read(cx).tree.clone()).unwrap();
    draw(cx);
    let row = cx.update(|_, cx| tree.read(cx).demo_row_point(&format!("c:{redis}"))).ok().flatten();
    if let Some(p) = row {
        click(cx, p);
    }
    pause(cx, 100).await;
    let t4 = new_tab_key(ws, cx);
    let on = t4.as_ref().map(|t| (tab_conn(t, cx), view(t, cx).1));
    r.check("clicking Redis in the explorer, then ⌘T: a document tab on Redis", row.is_some() && on == Some((Some(redis.clone()), true)), format!("{row:?} {on:?}"));

    let n = tabs(ws, cx).len();
    key(cx, "cmd-1");
    let first = active(ws, cx);
    key(cx, &format!("cmd-{}", n.min(9)));
    let last = active(ws, cx);
    let all = tabs(ws, cx);
    r.check("⌘1 and ⌘N go to the first and the Nth tab", first.as_ref() == all.first() && last.as_ref() == all.get(n.min(9) - 1), format!("{n} tabs"));

    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.request_delete_connection(redis.clone(), true, window, cx)));
    pause(cx, 200).await;
    let on_redis = tabs(ws, cx).iter().filter(|t| tab_conn(t, cx).as_deref() == Some(redis.as_str())).count();
    let t5 = new_tab_key(ws, cx);
    let on = t5.as_ref().and_then(|t| tab_conn(t, cx));
    r.check(
        "deleting Redis: no tab stays on it, and ⌘T falls back to a live connection",
        on_redis == 0 && on.as_ref().is_some_and(|c| *c == pg || *c == mongo),
        format!("{on_redis} tabs on Redis, new tab on {on:?}"),
    );
}
