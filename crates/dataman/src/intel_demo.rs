use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Focusable};

use crate::app::Workspace;
use crate::tab::QueryTab;

pub fn is_stage(s: &str) -> bool {
    matches!(s, "complete-sig" | "format" | "format-after" | "lint" | "params" | "intel-check")
}

const MESSY: &str = "with big as (select customer, sum(total) as sum_total from orders where status <> 'cancelled' group by customer having sum(total) > 1000) select c.name, c.city, b.sum_total, case when b.sum_total > 5000 then 'gold' when b.sum_total > 2000 then 'silver' else 'bronze' end as tier from big b join customer c on c.id = b.customer where c.country = 'DE' and exists (select 1 from orders p where p.customer = c.id and p.status = 'pending') order by b.sum_total desc limit 20;";

const LINT: &str = "SELECT p.id, p.totl, c.name
FROM orders p
JOIN customer c
WHERE c.country = 'DE';

DELETE FROM orders;

SELECT * FROM ordrs;

SELECT 'unterminated";

const PARAMS: &str = "-- orders of a customer, by status
SELECT p.id, p.status, p.total
FROM orders p
WHERE p.customer = :customer
  AND p.status = $1
ORDER BY p.total DESC
LIMIT :n;";

fn key(cx: &mut AsyncWindowContext, k: &str) {
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(crate::keymap::keystroke(k), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn typ(cx: &mut AsyncWindowContext, s: &str) {
    for c in s.chars() {
        let k = if c == ' ' { "space".to_string() } else { c.to_string() };
        key(cx, &k);
    }
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

fn tab(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> Entity<QueryTab> {
    cx.update(|_, cx| ws.read(cx).active_tab().unwrap().clone()).unwrap()
}

async fn until(cx: &mut AsyncWindowContext, ms: u64, ready: impl Fn(&mut gpui::App) -> bool) -> bool {
    for _ in 0..ms / 50 {
        if cx.update(|_, cx| ready(cx)).unwrap_or(false) {
            return true;
        }
        pause(cx, 50).await;
    }
    false
}

fn set(tab: &Entity<QueryTab>, text: &str, at: Option<usize>, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        let editor = tab.read(cx).editor.clone();
        editor.update(cx, |e, cx| {
            e.set_text(text, cx);
            e.set_cursor(at.unwrap_or(text.len()), cx);
        });
        window.focus(&editor.focus_handle(cx), cx);
    });
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, cx: &mut AsyncWindowContext) -> bool {
    let tab = tab(ws, cx);
    let t2 = tab.clone();
    until(cx, 15_000, move |cx| {
        let t = t2.read(cx);
        t.connection().is_some_and(|c| c.schema.snapshot(t.database()).catalog.is_some() && c.schema.snapshot(t.database()).complete)
    })
    .await;
    let _ = cx.update(|_, cx| tab.update(cx, |t, cx| t.refresh_schema(cx)));
    match stage {
        "complete-sig" => {
            let text = "SELECT p.id,\n       date_trunc('month', to_timestamp(p.c)) AS month,\n       c.name\nFROM orders p\nJOIN customer c ON c.id = p.customer;";
            set(&tab, text, Some(text.find("p.c)").unwrap() + 3), cx);
            typ(cx, "r");
            true
        }
        "format" | "format-after" => {
            set(&tab, MESSY, Some(0), cx);
            if stage == "format-after" {
                key(cx, "alt-shift-f");
            }
            true
        }
        "lint" => {
            set(&tab, LINT, Some(LINT.find("FROM orders p").unwrap()), cx);
            pause(cx, 700).await;
            let _ = cx.update(|_, cx| {
                let editor = tab.read(cx).editor.clone();
                let ix = editor.read(cx).lints().iter().position(|l| l.message.contains("totl"));
                editor.update(cx, |e, cx| e.demo_hover(ix, cx));
            });
            true
        }
        "params" => {
            set(&tab, PARAMS, Some(PARAMS.find("SELECT").unwrap()), cx);
            crate::editor::dispatch(cx, Box::new(crate::editor::RunStatement));
            pause(cx, 300).await;
            let _ = cx.update(|window, cx| {
                let Some(prompt) = tab.read(cx).params.as_ref().map(|p| p.prompt.clone()) else { return };
                let rows: Vec<_> = prompt.read(cx).rows.iter().map(|r| r.input.clone()).collect();
                for (input, v) in rows.iter().zip(["paid", "42", "20"]) {
                    input.update(cx, |t, cx| t.set_text(v, cx));
                }
                prompt.update(cx, |p, cx| p.focus_field(1, window, cx));
            });
            true
        }
        _ => check(ws, &tab, cx).await,
    }
}

async fn check(ws: &Entity<Workspace>, tab: &Entity<QueryTab>, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let editor = cx.update(|_, cx| tab.read(cx).editor.clone()).unwrap();
    let text = |cx: &mut AsyncWindowContext| cx.update(|_, cx| editor.read(cx).text().to_string()).unwrap();

    let has_catalog = cx.update(|_, cx| {
        let t = tab.read(cx);
        t.connection().is_some_and(|c| c.schema.snapshot(t.database()).catalog.as_ref().is_some_and(|k| k.has_function("date_trunc")))
    }).unwrap_or(false);
    report("the tab loads the function catalog", has_catalog, String::new());

    set(tab, "SELECT ", None, cx);
    typ(cx, "date_trunc('day', ");
    let sig = cx.update(|_, cx| editor.read(cx).signature().map(|s| (s.function.name.clone(), s.active, s.overloads))).unwrap();
    report("signature help on the second argument", sig.as_ref().is_some_and(|s| s.0 == "date_trunc" && s.1 == Some(1) && s.2 >= 2), format!("{sig:?}"));
    key(cx, "escape");

    set(tab, "SELECT * FROM orders WHERE ", None, cx);
    typ(cx, "lowe");
    key(cx, "tab");
    let (t, sig) = cx.update(|_, cx| (editor.read(cx).text().to_string(), editor.read(cx).signature().map(|s| s.function.name.clone()))).unwrap();
    report("a function completes as name() with its signature", t.ends_with("lower()") && sig.as_deref() == Some("lower"), format!("{t:?} {sig:?}"));

    set(tab, "select p.id,c.name from orders p join customer c on c.id=p.customer where p.total>10", Some(0), cx);
    key(cx, "alt-shift-f");
    let f = text(cx);
    report("⌥⇧F formats the script", f == "SELECT p.id, c.name\nFROM orders p\nJOIN customer c ON c.id = p.customer\nWHERE p.total > 10", format!("{f:?}"));
    key(cx, "cmd-z");
    let u = text(cx);
    report("one ⌘Z undoes the whole format", u.starts_with("select p.id,c.name"), format!("{u:?}"));

    set(tab, "SELECT p.totl, c.name FROM orders p JOIN customer c ON c.id = p.customer;\nSELECT p.total, c.name FROM orders p JOIN customer c USING (id) WHERE status = 'x';", None, cx);
    pause(cx, 700).await;
    let lints: Vec<String> = cx.update(|_, cx| editor.read(cx).lints().iter().map(|l| l.message.clone()).collect()).unwrap();
    report("lint: a typo against the live schema, nothing else", lints == ["column p.totl does not exist"], format!("{lints:?}"));

    set(tab, "SELECT $1::int + 1 AS n, :'name' AS name;", Some(0), cx);
    crate::editor::dispatch(cx, Box::new(crate::editor::RunStatement));
    pause(cx, 400).await;
    let up = cx.update(|window, cx| {
        let t = tab.read(cx);
        t.params.as_ref().map(|p| (p.prompt.read(cx).rows.len(), p.prompt.read(cx).rows[0].input.focus_handle(cx).is_focused(window)))
    }).unwrap_or(None);
    report("Run Statement on $1 and :'name' opens the prompt, first field focused", up == Some((2, true)), format!("{up:?}"));
    typ(cx, "41");
    key(cx, "enter");
    typ(cx, "o'brien");
    key(cx, "cmd-enter");
    let t2 = tab.clone();
    until(cx, 10_000, move |cx| {
        let t = t2.read(cx);
        t.params.is_none() && !t.is_running() && t.result().is_some_and(|r| r.phase().is_finished())
    })
    .await;
    let cells = cx.update(|_, cx| {
        let r = tab.read(cx).result().cloned();
        r.map(|r| {
            let s = r.snapshot();
            (s.text(0, 0).map(|x| x.into_owned()), s.text(0, 1).map(|x| x.into_owned()), r.sql.clone())
        })
    }).unwrap_or(None);
    report(
        "the values are bound ($1 → 42, the quote stays a value)",
        cells.as_ref().is_some_and(|c| c.0.as_deref() == Some("42") && c.1.as_deref() == Some("o'brien") && c.2.contains("$2")),
        format!("{cells:?}"),
    );
    key(cx, "cmd-enter");
    pause(cx, 400).await;
    let again = cx.update(|_, cx| tab.read(cx).params.as_ref().map(|p| p.prompt.read(cx).values(cx))).unwrap_or(None);
    report(
        "the next run offers the last values",
        again.as_ref().is_some_and(|v| v == &[("$1".to_string(), "41".to_string()), (":name".to_string(), "o'brien".to_string())]),
        format!("{again:?}"),
    );
    key(cx, "escape");
    pause(cx, 100).await;
    let closed = cx.update(|_, cx| tab.read(cx).params.is_none()).unwrap_or(false);
    report("esc cancels the prompt", closed, String::new());

    let src = "SELECT c.name FROM customer c";
    set(tab, src, Some(src.find("customer").unwrap() + 2), cx);
    key(cx, "f12");
    let (conn, db) = cx.update(|_, cx| {
        let t = tab.read(cx);
        (t.connection().map(|c| c.profile.id.clone()).unwrap_or_default(), t.database().to_string())
    }).unwrap();
    let want = format!("c:{conn}/d:{db}/s:public/r:customer");
    let ws2 = ws.clone();
    let w2 = want.clone();
    let revealed = until(cx, 5_000, move |cx| ws2.read(cx).tree.read(cx).selected_id() == Some(w2.as_str())).await;
    let sel = cx.update(|_, cx| ws.read(cx).tree.read(cx).selected_id().map(String::from));
    report("F12 selects the table in the explorer", revealed, format!("{sel:?} (want {want})"));
    let col = format!("{want}/cols/name");
    set(tab, src, Some(src.find("name").unwrap() + 1), cx);
    key(cx, "f12");
    let ws2 = ws.clone();
    let c2 = col.clone();
    let revealed = until(cx, 5_000, move |cx| ws2.read(cx).tree.read(cx).selected_id() == Some(c2.as_str())).await;
    report("F12 on a column selects the column", revealed, format!("want {col}"));
    let before = cx.update(|_, cx| ws.read(cx).tabs.len()).unwrap();
    set(tab, src, Some(src.find("customer").unwrap() + 2), cx);
    key(cx, "alt-f12");
    let ws2 = ws.clone();
    let opened = until(cx, 5_000, move |cx| {
        let w = ws2.read(cx);
        w.tabs.len() == before + 1 && w.active_tab().is_some_and(|t| t.read(cx).editor.read(cx).text().contains("CREATE TABLE"))
    })
    .await;
    report("⌥F12 opens the CREATE TABLE in a new tab", opened, String::new());
    ok
}
