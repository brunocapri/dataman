//! `format-check`: the toolbar's Format button, clicked in a SQLite tab and in a MongoDB one. The
//! text (or only the selection) comes out formatted, one ⌘Z takes it back, and text that cannot
//! be formatted stays as it is, with a notice.

use gpui::{AsyncWindowContext, Entity};
use dataman_core::config::Engine;

use crate::app::Workspace;
use crate::check::{Report, active, click_probe, connect, draw, pause, profile, until};
use crate::tab::QueryTab;

async fn on(ws: &Entity<Workspace>, engine: Engine, r: &mut Report, cx: &mut AsyncWindowContext) -> Option<Entity<QueryTab>> {
    let (prof, pw) = profile(engine);
    if let Some(path) = prof.path.as_deref() {
        let _ = dataman_core::import::create_database(engine, std::path::Path::new(path));
    }
    if let Err(e) = connect(ws, &prof, &pw, cx).await {
        r.check(&format!("the {} connection connects", engine.label()), false, e);
        return None;
    }
    let tab = active(ws, cx)?;
    let _ = cx.update(|_, cx| {
        let conn = ws.read(cx).store.read(cx).connection(&prof.id);
        tab.update(cx, |t, cx| conn.map(|c| t.adopt(c, cx)))
    });
    let lang = (!engine.is_sql()).then_some(engine);
    until(cx, 10, |cx| cx.update(|_, cx| tab.read(cx).connection().is_some_and(|c| c.engine() == engine) && tab.read(cx).editor.read(cx).doc_lang() == lang).unwrap_or(false)).await;
    draw(cx);
    Some(tab)
}

fn text(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> String {
    cx.update(|_, cx| tab.read(cx).editor.read(cx).text().to_string()).unwrap_or_default()
}

fn put(tab: &Entity<QueryTab>, s: &str, sel: Option<(usize, usize)>, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|_, cx| {
        tab.read(cx).editor.clone().update(cx, |e, cx| {
            e.set_text(s, cx);
            match sel {
                Some((a, b)) => e.demo_select(a, b, cx),
                None => e.set_cursor(0, cx),
            }
        })
    });
    draw(cx);
}

fn notice(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> String {
    cx.update(|_, cx| ws.read(cx).flash_notice.as_ref().map(|n| n.0.clone())).ok().flatten().unwrap_or_default()
}

/// Clicks Format and returns the text after it.
async fn format(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> Option<String> {
    let clicked = click_probe("tb-format", cx);
    pause(cx, 100).await;
    clicked.then(|| text(tab, cx))
}

async fn undo(tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> String {
    crate::editor::dispatch(cx, Box::new(crate::editor::Undo));
    pause(cx, 50).await;
    text(tab, cx)
}

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let Some(tab) = on(ws, Engine::Sqlite, r, cx).await else { return };
    let messy = "select a,b from t where a=1 and b>2";
    put(&tab, messy, None, cx);
    let out = format(&tab, cx).await;
    r.check("Format is on the toolbar of a SQL tab", out.is_some(), "no tb-format");
    let want = "SELECT a, b\nFROM t\nWHERE a = 1\n  AND b > 2";
    r.check("clicking it formats the SQL", out.as_deref() == Some(want), format!("{out:?}"));
    let back = undo(&tab, cx).await;
    r.check("one ⌘Z takes the SQL back", back == messy, format!("{back:?}"));
    let two = "select 1;\nselect a,b from t";
    put(&tab, two, Some((10, two.len())), cx);
    let out = format(&tab, cx).await;
    r.check("with a selection, only the selection is formatted", out.as_deref() == Some("select 1;\nSELECT a, b\nFROM t"), format!("{out:?}"));

    // MongoDB: the same button, the tab's own language.
    let Some(tab) = on(ws, Engine::MongoDb, r, cx).await else { return };
    let mongo = until(cx, 10, |cx| cx.update(|_, cx| tab.read(cx).editor.read(cx).doc_lang() == Some(Engine::MongoDb)).unwrap_or(false)).await;
    r.check("the tab speaks MongoDB", mongo, "");
    let messy = "db.people.find({age:{$gt:30},name:'Ada'}).sort({age:-1})";
    put(&tab, messy, None, cx);
    let out = format(&tab, cx).await;
    r.check("Format is on the toolbar of a MongoDB tab", out.is_some(), "no tb-format");
    r.check(
        "clicking it formats the query",
        out.as_deref() == Some("db.people.find({ age: { $gt: 30 }, name: 'Ada' }).sort({ age: -1 })"),
        format!("{out:?}"),
    );
    let back = undo(&tab, cx).await;
    r.check("one ⌘Z takes the query back", back == messy, format!("{back:?}"));
    let nested = "db.people.insertOne({\n      name: 'Ada',   // first\n  tags: ['a',  'b'],\n address: {city: 'London'}\n})";
    put(&tab, nested, None, cx);
    let out = format(&tab, cx).await;
    r.check(
        "objects over several lines are indented, the comment kept",
        out.as_deref() == Some("db.people.insertOne({\n  name: 'Ada', // first\n  tags: ['a', 'b'],\n  address: { city: 'London' }\n})"),
        format!("{out:?}"),
    );
    let again = format(&tab, cx).await;
    r.check("formatting again changes nothing", again == out, format!("{again:?}"));
    let broken = "db.people.find({name: 'Ada)";
    put(&tab, broken, None, cx);
    let out = format(&tab, cx).await;
    let said = notice(ws, cx);
    r.check("text that cannot be formatted stays as it is", out.as_deref() == Some(broken), format!("{out:?}"));
    r.check("and a notice says why", said.starts_with("Not formatted"), said);
}
