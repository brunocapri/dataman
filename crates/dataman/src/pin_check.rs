use std::time::Duration;

use gpui::{AsyncWindowContext, Entity, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Pixels, Point};

use crate::app::Workspace;
use crate::tree::{SchemaTree, pin_id};
use crate::tree_menu::Act;

pub fn is_stage(s: &str) -> bool {
    matches!(s, "pin-check" | "pins" | "pins-menu")
}

fn input(cx: &mut AsyncWindowContext, e: PlatformInput) {
    let _ = cx.update(|window, cx| {
        window.dispatch_event(e, cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn draw(cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

/// A move first: GPUI's hit-test follows the latest hovered element.
fn click(cx: &mut AsyncWindowContext, p: Point<Pixels>, button: MouseButton, count: usize) {
    let m = Modifiers::default();
    input(cx, PlatformInput::MouseMove(MouseMoveEvent { position: p, pressed_button: None, modifiers: m }));
    for n in 1..=count {
        input(cx, PlatformInput::MouseDown(MouseDownEvent { button, position: p, modifiers: m, click_count: n, first_mouse: false }));
        input(cx, PlatformInput::MouseUp(MouseUpEvent { button, position: p, modifiers: m, click_count: n }));
    }
}

fn key(cx: &mut AsyncWindowContext, k: &str) {
    let _ = cx.update(|window, cx| {
        window.dispatch_keystroke(Keystroke::parse(k).unwrap(), cx);
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

async fn pause(cx: &mut AsyncWindowContext, ms: u64) {
    cx.background_executor().timer(Duration::from_millis(ms)).await;
}

async fn row(tree: &Entity<SchemaTree>, id: &str, cx: &mut AsyncWindowContext) -> bool {
    for _ in 0..200 {
        if cx.update(|_, cx| tree.read(cx).has_row(id)).unwrap_or(false) {
            return true;
        }
        pause(cx, 50).await;
    }
    println!("pin check: no row {id}");
    false
}

async fn expand(tree: &Entity<SchemaTree>, id: &str, cx: &mut AsyncWindowContext) {
    if row(tree, id, cx).await {
        let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.expand_id(id, cx)));
        pause(cx, 400).await;
    }
}

async fn point_of(tree: &Entity<SchemaTree>, id: &str, cx: &mut AsyncWindowContext) -> Option<Point<Pixels>> {
    row(tree, id, cx).await;
    for _ in 0..20 {
        draw(cx);
        if let Some(p) = cx.update(|_, cx| tree.read(cx).demo_row_point(id)).ok().flatten() {
            return Some(p);
        }
        pause(cx, 20).await;
    }
    None
}

async fn menu_choose(tree: &Entity<SchemaTree>, id: &str, act: Act, cx: &mut AsyncWindowContext) -> Vec<Act> {
    let Some(p) = point_of(tree, id, cx).await else { return vec![] };
    click(cx, p, MouseButton::Right, 1);
    pause(cx, 50).await;
    let acts = cx.update(|_, cx| tree.read(cx).demo_menu_acts()).unwrap_or_default();
    match cx.update(|_, cx| tree.read(cx).demo_menu_item_point(act)).ok().flatten() {
        Some(at) => click(cx, at, MouseButton::Left, 1),
        None => key(cx, "escape"),
    }
    pause(cx, 50).await;
    acts
}

pub async fn run(ws: &Entity<Workspace>, stage: &str, conn: &str, db: &str, cx: &mut AsyncWindowContext) -> bool {
    let mut ok = true;
    let mut report = |name: &str, pass: bool, detail: String| {
        println!("{} {name}{}", if pass { "PASS" } else { "FAIL" }, if pass { String::new() } else { format!(" — {detail}") });
        ok &= pass;
    };
    let Ok(tree) = cx.update(|_, cx| ws.read(cx).tree.clone()) else { return false };
    let core = cx.update(|_, cx| ws.read(cx).store.read(cx).core.clone()).unwrap();
    let s = format!("c:{conn}/d:{db}/s:public");
    let rel = |n: &str| format!("{s}/r:{n}");
    let pinned = |n: &str| pin_id(conn, db, "public", n);
    let group = format!("c:{conn}/pinned");
    expand(&tree, &s, cx).await;
    expand(&tree, &format!("{s}/tables"), cx).await;
    row(&tree, &rel("orders"), cx).await;
    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.collapse_id(&rel("orders"), cx)));

    if stage != "pin-check" {
        for n in ["orders", "customer"] {
            menu_choose(&tree, &rel(n), Act::Pin, cx).await;
        }
        if stage == "pins" {
            expand(&tree, &pinned("orders"), cx).await;
            let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.select_id(&pinned("customer"), cx)));
        } else {
            row(&tree, &pinned("customer"), cx).await;
            let _ = cx.update(|window, cx| tree.update(cx, |t, cx| t.demo_menu(&pinned("customer"), false, true, window, cx)));
        }
        return true;
    }

    let acts = menu_choose(&tree, &rel("orders"), Act::Pin, cx).await;
    report("a table's menu offers Pin table", acts.contains(&Act::Pin) && !acts.contains(&Act::Unpin), format!("{acts:?}"));
    let shown = cx.update(|_, cx| tree.read(cx).demo_pinned(conn)).unwrap();
    report(
        "pinned: in its connection's group, saying where it lives",
        shown == [("orders".to_string(), "public".to_string())] && cx.update(|_, cx| tree.read(cx).has_row(&group)).unwrap(),
        format!("{shown:?}"),
    );
    let file = std::fs::read_to_string(core.dirs.pins_file()).unwrap_or_default();
    report("saved in pins.toml", file.contains("name = \"orders\"") && file.contains(&format!("connection = \"{conn}\"")), file.clone());
    let acts = menu_choose(&tree, &rel("orders"), Act::Unpin, cx).await;
    report("its menu in the schema now says Unpin", acts.contains(&Act::Unpin) && !acts.contains(&Act::Pin), format!("{acts:?}"));
    menu_choose(&tree, &rel("orders"), Act::Pin, cx).await;
    menu_choose(&tree, &rel("customer"), Act::Pin, cx).await;
    let names: Vec<String> = cx.update(|_, cx| tree.read(cx).demo_pinned(conn)).unwrap().into_iter().map(|p| p.0).collect();
    report("pins keep the order they were pinned in", names == ["orders", "customer"], format!("{names:?}"));
    let group_ix = cx.update(|_, cx| tree.read(cx).demo_row_point(&group).map(|p| p.y)).unwrap();
    let first_db = cx.update(|_, cx| tree.read(cx).demo_row_point(&format!("c:{conn}/d:{db}")).map(|p| p.y)).unwrap();
    report("the group heads the connection", group_ix.zip(first_db).is_some_and(|(g, d)| g < d), format!("{group_ix:?} {first_db:?}"));

    if let Some(p) = point_of(&tree, &pinned("orders"), cx).await {
        click(cx, p, MouseButton::Left, 1);
    }
    let cols = format!("{}/cols", pinned("orders"));
    let opened = row(&tree, &cols, cx).await;
    let schema_row_shut = !cx.update(|_, cx| tree.read(cx).has_row(&format!("{}/cols", rel("orders")))).unwrap();
    report("a click opens a pinned table's columns (its schema's row stays shut)", opened && schema_row_shut, format!("opened={opened} schema_row_shut={schema_row_shut}"));
    let col = format!("{cols}/id");
    report("its columns are there", row(&tree, &col, cx).await, String::new());

    let before = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| t.entity_id())).ok().flatten();
    if let Some(p) = point_of(&tree, &pinned("customer"), cx).await {
        click(cx, p, MouseButton::Left, 2);
    }
    let mut title = String::new();
    for _ in 0..100 {
        pause(cx, 50).await;
        let now = cx.update(|_, cx| ws.read(cx).active_tab().map(|t| (t.entity_id(), t.read(cx).title(cx).to_string()))).ok().flatten();
        if let Some((id, t)) = now
            && Some(id) != before
        {
            title = t;
            break;
        }
    }
    report("a double click opens its data in a tab", title.contains("customer"), format!("tab title {title:?}"));

    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.set_filter("custom", cx)));
    draw(cx);
    let (in_group, in_schema, sel) = cx
        .update(|_, cx| {
            let t = tree.read(cx);
            (t.has_row(&pinned("customer")), t.has_row(&rel("customer")), t.selected_id().map(str::to_string))
        })
        .unwrap();
    report(
        "the filter finds a pinned table in the group first, and in its schema",
        in_group && in_schema && sel.as_deref() == Some(pinned("customer").as_str()),
        format!("group={in_group} schema={in_schema} selected={sel:?}"),
    );
    let _ = cx.update(|_, cx| tree.update(cx, |t, cx| t.set_filter("", cx)));
    draw(cx);

    let acts = menu_choose(&tree, &pinned("orders"), Act::Unpin, cx).await;
    report("a pinned table has its table menu, with Unpin", acts.contains(&Act::Unpin) && acts.contains(&Act::SelectTop) && acts.contains(&Act::DropTable), format!("{acts:?}"));
    let (shown, sel) = cx.update(|_, cx| (tree.read(cx).demo_pinned(conn), tree.read(cx).selected_id().map(str::to_string))).unwrap();
    let sel_exists = sel.as_ref().is_some_and(|id| cx.update(|_, cx| tree.read(cx).has_row(id)).unwrap());
    report("unpinned: out of the group, the selection on a row still there", shown.len() == 1 && shown[0].0 == "customer" && sel_exists, format!("{shown:?} selected={sel:?}"));
    menu_choose(&tree, &pinned("customer"), Act::Unpin, cx).await;
    let gone = !cx.update(|_, cx| tree.read(cx).has_row(&group)).unwrap();
    report("the last unpin takes the group and the file along", gone && !core.dirs.pins_file().exists(), format!("group gone={gone}"));
    ok
}
