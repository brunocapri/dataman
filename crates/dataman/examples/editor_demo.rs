//! The SQL editor alone in a window, with a fake schema for autocomplete.
//!
//! `EDITOR_BENCH=1` loads a ~5,000-line script, types a sample query one character per frame,
//! prints keystroke-to-next-frame latency and quits (`EDITOR_LINES` changes the script size).
//! `EDITOR_CHECK=1` drives editing actions through the real key dispatch, prints the results
//! and quits.

#[path = "../src/assets.rs"]
#[allow(dead_code)]
mod assets;
#[path = "../src/dock_icon.rs"]
#[allow(dead_code)]
mod dock_icon;
#[path = "../src/editor.rs"]
#[allow(dead_code)]
mod editor;
#[path = "../src/find.rs"]
#[allow(dead_code)]
mod find;
#[path = "../src/glass.rs"]
#[allow(dead_code)]
mod glass;
#[path = "../src/input.rs"]
#[allow(dead_code)]
mod input;
#[path = "../src/motion.rs"]
#[allow(dead_code)]
mod motion;
#[path = "../src/theme.rs"]
#[allow(dead_code)]
mod theme;
#[path = "../src/ui.rs"]
#[allow(dead_code)]
mod ui;

/// What the find bar and the editor take from the grid.
mod grid {
    /// No grid here: the editor draws its own gutter width.
    #[derive(Clone, Default)]
    pub struct GutterShare;

    impl GutterShare {
        pub fn editor(&self, need: f32) -> f32 {
            need
        }
    }

    pub fn ui_count(n: usize) -> String {
        n.to_string()
    }
}

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use editor::{CodeEditor, EditorEvent};
use gpui::{
    App, Bounds, Context, Entity, Focusable, KeyBinding, Window, WindowBounds, WindowOptions, actions, div, prelude::*,
    px, size,
};
use gpui_platform::application;
use dataman_core::schema::{Column, RelKind, Relation, SchemaSnapshot};

actions!(editor_demo, [Quit]);

const SCRIPT: &str = "\
-- Open orders
select o.id, o.customer, o.status, o.total
from orders o
where o.status = 'open'
order by o.total desc;

select c.name, count(*) as orders, sum(o.total)
from customer c
join orders o on o.customer = c.id
group by c.name
having count(*) > 1;

/* Updates the status */
update orders
    set status = 'paid'
    where id = $1;

create function customer_total(c bigint) returns numeric as $$
    select sum(total) from orders where customer = c;
$$ language sql;

select \"Odd Column\", 42, 3.14e2, now()::date from order_summary;
";

const SAMPLE: &str = "select o.id, o.customer, o.status, o.total from orders o join customer c on c.id = o.customer where o.status = 'open' and o.total > 100 order by o.total desc limit 50; select c.name, count(*) from customer c join orders o on o.customer = c.id where o.st";

fn schema() -> SchemaSnapshot {
    let col = |name: &str, ty: &str, pk: bool| Column {
        name: name.into(),
        type_name: ty.into(),
        nullable: Some(!pk),
        primary_key: pk,
        default: None,
    };
    let rel = |name: &str, kind| Relation { schema: "public".into(), name: name.into(), kind, oid: None };
    let relations =
        vec![rel("orders", RelKind::Table), rel("customer", RelKind::Table), rel("order_summary", RelKind::View)];
    let mut columns = HashMap::new();
    columns.insert(
        0,
        vec![
            col("id", "bigint", true),
            col("customer", "bigint", false),
            col("status", "text", false),
            col("total", "numeric(12,2)", false),
        ],
    );
    columns.insert(1, vec![col("id", "bigint", true), col("name", "text", false)]);
    columns.insert(
        2,
        vec![col("customer", "text", false), col("orders", "bigint", false), col("total", "numeric", false)],
    );
    let mut s = SchemaSnapshot::new(vec!["public".into()], vec!["public".into(), "sales".into()], relations, columns);
    let f = |name: &str, args: &str, result: &str| dataman_core::schema::FuncSig {
        oid: 1,
        schema: "pg_catalog".into(),
        name: name.into(),
        args: args.into(),
        result: result.into(),
        kind: 'f',
        returns_set: false,
    };
    s.catalog = Some(Arc::new(dataman_core::schema::Catalog::new(
        vec![
            f("date_trunc", "text, timestamp with time zone", "timestamp with time zone"),
            f("coalesce", "VARIADIC \"any\"", "anyelement"),
            f("now", "", "timestamp with time zone"),
        ],
        vec![],
    )));
    s
}

struct Bench {
    chars: Vec<char>,
    next: usize,
    started: Option<Instant>,
    latency: Vec<Duration>,
    call: Vec<Duration>,
    visible: usize,
}

struct Demo {
    editor: Entity<CodeEditor>,
    bench: Option<Bench>,
    started: bool,
}

impl Demo {
    fn step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.bench.as_mut() else { return };
        if let Some(t) = b.started.take() {
            b.latency.push(t.elapsed());
        }
        let Some(&ch) = b.chars.get(b.next) else {
            report(b);
            cx.quit();
            return;
        };
        b.next += 1;
        let t = Instant::now();
        b.started = Some(t);
        self.editor.update(cx, |e, cx| e.type_text(ch.encode_utf8(&mut [0; 4]), window, cx));
        b.call.push(t.elapsed());
        if self.editor.read(cx).completion_visible() {
            b.visible += 1;
        }
        let this = cx.entity();
        window.on_next_frame(move |window, cx| this.update(cx, |d, cx| d.step(window, cx)));
    }
}

fn pct(v: &mut [Duration], p: f64) -> f64 {
    v.sort();
    v[((v.len() as f64 - 1.0) * p).round() as usize].as_secs_f64() * 1e3
}

fn report(b: &mut Bench) {
    for (name, v) in [("keystroke_to_frame_ms", &mut b.latency), ("type_text_ms", &mut b.call)] {
        println!(
            "{name} n={} p50={:.2} p95={:.2} p99={:.2} max={:.2}",
            v.len(),
            pct(v, 0.5),
            pct(v, 0.95),
            pct(v, 0.99),
            pct(v, 1.0)
        );
    }
    println!("popup_visible={}/{}", b.visible, b.chars.len());
}

impl Render for Demo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.started && self.bench.is_some() {
            self.started = true;
            let this = cx.entity();
            window.on_next_frame(move |window, cx| this.update(cx, |d, cx| d.step(window, cx)));
        }
        if !self.started && std::env::var("EDITOR_CHECK").is_ok() {
            self.started = true;
            let ed = self.editor.clone();
            window.on_next_frame(move |window, cx| check(ed, window, cx));
        }
        div().size_full().flex().child(self.editor.clone())
    }
}

type Step = Box<dyn Fn(&Entity<CodeEditor>, &mut Window, &mut App)>;

thread_local! {
    /// A step asks the runner to wait this long (ms) before the next one.
    static DELAY: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn wait(ms: u64) -> Step {
    Box::new(move |_, _, _| DELAY.with(|d| d.set(ms)))
}

/// Prints PASS or FAIL for a condition on the editor, with what it saw.
fn ok(label: &'static str, f: impl Fn(&CodeEditor) -> (bool, String) + 'static) -> Step {
    Box::new(move |ed, _, cx| {
        let (pass, seen) = f(ed.read(cx));
        println!("{} {label}: {seen}", if pass { "PASS" } else { "FAIL" });
    })
}

fn check(ed: Entity<CodeEditor>, window: &mut Window, cx: &mut App) {
    use editor::*;
    fn t(ed: &Entity<CodeEditor>, cx: &App) -> String {
        format!("{:?}", ed.read(cx).text())
    }
    fn a<A: gpui::Action + Clone>(x: A) -> Step {
        Box::new(move |_, w, cx| w.dispatch_action(Box::new(x.clone()), cx))
    }
    fn p(label: &'static str) -> Step {
        Box::new(move |ed, _, cx| {
            println!("{label}: {} popup={}", t(ed, cx), ed.read(cx).completion_visible())
        })
    }
    fn f(g: impl Fn(&mut CodeEditor, &mut Window, &mut Context<CodeEditor>) + 'static) -> Step {
        Box::new(move |ed, w, cx| ed.update(cx, |e, cx| g(e, w, cx)))
    }
    let steps: Vec<Step> = vec![
        f(|e, w, cx| {
            e.set_text("    select 1", cx);
            e.type_text("\n", w, cx);
        }),
        p("newline-indent"),
        f(|e, _, cx| e.set_text("select 1\n  from t\n", cx)),
        a(SelectAll),
        a(ToggleComment),
        p("comment"),
        a(ToggleComment),
        p("uncomment"),
        a(Tab),
        p("indent"),
        a(Outdent),
        p("outdent"),
        a(Undo),
        p("undo"),
        a(Redo),
        p("redo"),
        f(|e, w, cx| {
            e.set_text("select * from orders o where o.", cx);
            e.type_text("st", w, cx);
        }),
        p("popup"),
        a(Newline),
        p("accepted"),
        f(|e, w, cx| {
            e.set_text("select * from ", cx);
            e.type_text("ord", w, cx);
        }),
        p("popup2"),
        a(MoveDown),
        a(Tab),
        p("accepted2"),
        f(|e, w, cx| {
            e.set_text("select\n 1;\nselect 2;\n\n", cx);
            e.set_cursor(3, cx);
            e.run_statement(w, cx);
        }),
        p("run"),
        f(|e, _, cx| e.set_cursor(14, cx)),
        a(RunStatement),
        p("run2"),
        a(SelectAll),
        a(RunStatement),
        p("run-selection"),
        f(|e, w, cx| {
            e.set_text("abc def", cx);
            e.type_text("x", w, cx);
            e.type_text("y", w, cx);
        }),
        a(MoveWordLeft),
        a(DeleteWordLeft),
        p("wordleft"),
        a(Undo),
        p("undo1"),
        a(Undo),
        p("undo2"),
        f(|e, _, cx| {
            e.set_text("ab\ncdef\ngh", cx);
            e.set_cursor(7, cx);
        }),
        a(MoveUp),
        a(MoveDown),
        a(SelectDown),
        a(Backspace),
        p("goalcol"),
        f(|e, _, cx| {
            e.set_text("  select 1", cx);
            e.set_cursor(6, cx);
        }),
        a(MoveLineStart),
        a(SelectLineEnd),
        p("home"),
        a(Cut),
        p("cut"),
        a(Paste),
        a(Paste),
        p("paste"),
        f(|e, w, cx| {
            e.set_text("select 'a", cx);
            e.type_text("b", w, cx);
        }),
        p("no-popup-in-string"),
        f(|e, w, cx| {
            e.set_text("", cx);
            e.type_text("select", w, cx);
        }),
        p("kw"),
        a(Cancel),
        p("escape"),
        // Editor intelligence (item 8).
        f(|e, _, cx| {
            e.set_text("select o.id,o.total from orders o join customer c on c.id=o.customer where o.status='open' and o.total>10", cx);
            e.set_cursor(0, cx);
        }),
        a(FormatSql),
        ok("format", |e| {
            let want = "SELECT o.id, o.total\nFROM orders o\nJOIN customer c ON c.id = o.customer\nWHERE o.status = 'open'\n  AND o.total > 10";
            (e.text() == want, format!("{:?}", e.text()))
        }),
        a(FormatSql),
        ok("format-idempotent", |e| (e.text().starts_with("SELECT o.id, o.total\nFROM"), format!("{:?}", &e.text()[..20]))),
        a(Undo),
        ok("format-one-undo", |e| (e.text().starts_with("select o.id,o.total"), format!("{:?}", &e.text()[..19]))),
        f(|e, w, cx| {
            e.set_text("", cx);
            e.type_text("select date_tr", w, cx);
        }),
        a(Tab),
        ok("function-accept", |e| {
            let s = e.signature().map(|s| (s.function.name.clone(), s.active));
            (e.text() == "select date_trunc()" && s == Some(("date_trunc".into(), Some(0))), format!("{:?} {s:?}", e.text()))
        }),
        f(|e, w, cx| e.type_text("'day', ", w, cx)),
        ok("signature-second-arg", |e| {
            let s = e.signature().map(|s| s.active);
            (s == Some(Some(1)), format!("{:?} {s:?}", e.text()))
        }),
        f(|e, _, cx| e.set_text("select * from ordrs o join customer c where c.id = 1;\nupdate orders set status = 'x';", cx)),
        wait(500),
        ok("lint", |e| {
            let m: Vec<&str> = e.lints().iter().map(|l| l.message.as_str()).collect();
            (m == ["relation \"ordrs\" does not exist", "JOIN without ON", "UPDATE without WHERE"], format!("{m:?}"))
        }),
        f(|e, w, cx| {
            e.set_cursor(0, cx);
            e.type_text("x", w, cx);
        }),
        ok("lint-moves-with-typing", |e| {
            let first = e.lints().first().map(|l| e.text()[l.range.clone()].to_string());
            (first.as_deref() == Some("ordrs"), format!("{first:?}"))
        }),
        f(|e, _, cx| {
            e.set_text("with big as (select 1) select * from big", cx);
            let end = e.text().len();
            e.set_cursor(end - 1, cx);
        }),
        a(GoToDefinition),
        ok("go-to-cte", |e| (e.demo_selection() == (5..8), format!("{:?}", e.demo_selection()))),
    ];
    run_steps(ed, steps, 0, window, cx);
}

fn run_steps(ed: Entity<CodeEditor>, steps: Vec<Step>, i: usize, window: &mut Window, cx: &mut App) {
    let Some(s) = steps.get(i) else { return cx.quit() };
    s(&ed, window, cx);
    let ms = DELAY.with(|d| d.replace(0));
    if ms > 0 {
        window
            .spawn(cx, async move |cx| {
                cx.background_executor().timer(Duration::from_millis(ms)).await;
                let _ = cx.update(|w, cx| run_steps(ed, steps, i + 1, w, cx));
            })
            .detach();
        return;
    }
    window.on_next_frame(move |w, cx| run_steps(ed, steps, i + 1, w, cx));
}

fn main() {
    application().run(|cx: &mut App| {
        editor::bind_keys(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        let bench = std::env::var("EDITOR_BENCH").is_ok_and(|v| v == "1");
        let bounds = Bounds::centered(None, size(px(1100.), px(750.)), cx);
        let window = cx
            .open_window(
                WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() },
                |window, cx| {
                    let editor = cx.new(|cx| {
                        let mut e = CodeEditor::new(window, cx);
                        let lines: usize =
                            std::env::var("EDITOR_LINES").ok().and_then(|v| v.parse().ok()).unwrap_or(5000);
                        let text = if bench {
                            format!("{}\n", SCRIPT.repeat(lines / SCRIPT.lines().count() + 1))
                        } else {
                            SCRIPT.to_string()
                        };
                        e.set_text(&text, cx);
                        e.set_schema(Arc::new(schema()), cx);
                        if !bench {
                            // Show the error underline on a token, as after a failed statement.
                            if let Some(at) = text.find("\"Odd Column") {
                                let d = editor::Diagnostic { stmt: at..at, at: Some(at), message: "42703 column \"Odd Column\" does not exist".into() };
                                e.set_diagnostic(Some(d), cx);
                            }
                            e.set_cursor(0, cx);
                        }
                        e
                    });
                    cx.new(|cx| {
                        cx.subscribe(&editor, |_, _, ev: &EditorEvent, _| match ev {
                            EditorEvent::Run(stmts) => println!("Run {:?}", stmts.iter().map(|s| (s.at, s.sql.as_str())).collect::<Vec<_>>()),
                            EditorEvent::RunAll(stmts) => println!("RunAll ({}) {:?}", stmts.len(), stmts.iter().map(|s| s.sql.as_str()).collect::<Vec<_>>()),
                            EditorEvent::Changed => {}
                            EditorEvent::Definition { def, peek } => println!("Definition {def:?} peek={peek}"),
                        })
                        .detach();
                        Demo {
                            editor,
                            bench: bench.then(|| Bench {
                                chars: SAMPLE.chars().collect(),
                                next: 0,
                                started: None,
                                latency: vec![],
                                call: vec![],
                                visible: 0,
                            }),
                            started: false,
                        }
                    })
                },
            )
            .unwrap();
        window
            .update(cx, |demo, window, cx| {
                if bench {
                    println!("lines={}", demo.editor.read(cx).text().lines().count());
                }
                window.focus(&demo.editor.focus_handle(cx), cx);
                cx.activate(true);
            })
            .unwrap();
    });
}
