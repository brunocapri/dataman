//! `errors-check`: an error can be copied whole wherever it shows. The tooltip of a failed
//! connection takes the pointer and holds its button and a bounded, scrolling text; Copy error puts
//! the complete error on the clipboard from there, from the connection sheet, the status bar's
//! notice, a query's error view and result bar, and an import that cannot read its file.

use dataman_core::config::{ConnectionProfile, Engine};
use gpui::{AsyncWindowContext, Bounds, Entity, Modifiers, Pixels, PlatformInput, Point, ScrollDelta, ScrollWheelEvent, TouchPhase, point, px};

use crate::app::Workspace;
use crate::check::*;
use crate::conns::ConnState;

const UNTOUCHED: &str = "errors-check: nothing copied";

fn snap(name: &str, cx: &mut AsyncWindowContext) {
    let Ok(dir) = std::env::var("DATAMAN_SHOT_DIR") else { return };
    let path = format!("{dir}/errors-{name}.png");
    let _ = cx.update(|window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
        crate::bench::shot(window, &path, crate::theme::ActiveTheme::theme(&*cx).is_dark());
    });
}

fn tip(cx: &mut AsyncWindowContext) -> Option<Bounds<Pixels>> {
    probed("tooltip", cx)
}

/// Rests on what shows a tooltip, and crosses to its Copy error as a person would: the tooltip must
/// outlive the crossing. What the click put on the clipboard, or where that stopped.
async fn copy_from_tip(on: Point<Pixels>, cx: &mut AsyncWindowContext) -> Result<String, String> {
    set_clipboard(UNTOUCHED, cx);
    move_to(cx, on);
    if !until(cx, 5, |cx| probed("error-tip-copy", cx).is_some()).await {
        return Err(format!("no Copy error in a tooltip; tooltip {:?}", tip(cx)));
    }
    let button = probed("error-tip-copy", cx).ok_or("Copy error went away")?;
    let to = centre(button);
    for k in 1..=5 {
        move_to(cx, point(on.x + (to.x - on.x) * (k as f32 / 5.), on.y + (to.y - on.y) * (k as f32 / 5.)));
    }
    pause(cx, 300).await;
    draw(cx);
    let button = probed("error-tip-copy", cx).ok_or("the tooltip closed on the way to its button")?;
    click(cx, centre(button));
    if !until(cx, 3, |cx| clipboard(cx) != UNTOUCHED).await {
        return Err("the click copied nothing".into());
    }
    Ok(clipboard(cx))
}

async fn leave(cx: &mut AsyncWindowContext) {
    move_to(cx, point(px(600.), px(8.)));
    until(cx, 3, |cx| tip(cx).is_none()).await;
}

fn dialog_open(ws: &Entity<Workspace>, cx: &mut AsyncWindowContext) -> bool {
    cx.update(|_, cx| ws.read(cx).dialog.is_some()).unwrap_or(true)
}

async fn failed_connection(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) -> String {
    let core = core(ws, cx);
    let mut p = ConnectionProfile::new("errors-check-down");
    (p.host, p.port) = ("127.0.0.1".into(), 9);
    let id = p.id.clone();
    core.save_connection(p, None, None).expect("a demo store takes a connection");
    let rest: String = (1..=200).map(|i| format!("  frame {i}: {}\n", "x".repeat(i % 90))).collect();
    let wide = "y".repeat(900);
    let full = format!("FATAL:  could not connect: {wide}\nDETAIL:  the server said no\n{rest}HINT:  the very end of it");
    let _ = cx.update(|_, cx| {
        ws.update(cx, |w, cx| {
            w.store.update(cx, |s, cx| {
                s.reload(cx);
                s.states.insert(id.clone(), ConnState::Failed(full.clone()));
                cx.notify();
            })
        })
    });
    let row = format!("c:{id}");
    until(cx, 5, |cx| cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_row_point(&row)).ok().flatten().is_some()).await;
    let Some(on) = cx.update(|_, cx| ws.read(cx).tree.read(cx).demo_row_point(&row)).ok().flatten() else {
        r.check("a failed connection has its row", false, &row);
        return id;
    };

    let copied = copy_from_tip(on, cx).await;
    r.check(
        "a failed connection's tooltip copies the complete error, first line to last",
        copied.as_deref() == Ok(full.as_str()),
        match &copied {
            Ok(c) => format!("{} of {} bytes", c.len(), full.len()),
            Err(e) => e.clone(),
        },
    );
    r.check("…and the click stays in the tooltip: the row under it opens nothing", !dialog_open(ws, cx), "the connection sheet opened");
    r.check("…and says Copied in the button's place", until(cx, 2, |cx| probed("error-tip-copy-copied", cx).is_some()).await, "no Copied");
    r.check("…and the tooltip stays open", tip(cx).is_some(), "it closed");

    let (bounds, body, port) = (tip(cx), probed("error-tip-body", cx), probed("error-tip-text", cx));
    let viewport = cx.update(|window, _| window.viewport_size()).expect("the window");
    r.check(
        "the tooltip is bounded, wraps, and all of it fits the window",
        bounds
            .is_some_and(|b| b.size.width <= px(480.) && b.size.height <= px(14. * 17. + 52.) && b.right() <= viewport.width && b.bottom() <= viewport.height)
            && body.zip(port).is_some_and(|(b, p)| b.size.width <= p.size.width && b.size.height > p.size.height),
        format!("tooltip {bounds:?} text {body:?} in {port:?}, window {viewport:?}"),
    );
    snap("tooltip", cx);
    if let Some(p) = port {
        input(
            cx,
            PlatformInput::ScrollWheel(ScrollWheelEvent {
                position: centre(p),
                delta: ScrollDelta::Pixels(point(px(0.), px(-120.))),
                modifiers: Modifiers::default(),
                touch_phase: TouchPhase::Moved,
            }),
        );
    }
    let scrolled = until(cx, 2, |cx| probed("error-tip-body", cx).zip(body).is_some_and(|(now, was)| now.top() < was.top())).await;
    r.check(
        "the rest of a long error scrolls inside the tooltip, which stays",
        scrolled && tip(cx).is_some(),
        format!("{:?} from {body:?}", probed("error-tip-body", cx)),
    );
    leave(cx).await;
    r.check("…and leaving the tooltip closes it", tip(cx).is_none(), "still there");
    id
}

async fn connection_sheet(id: &str, ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.open_dialog(Some(id.to_string()), window, cx)));
    let Some(d) = cx.update(|_, cx| ws.read(cx).dialog.clone()).ok().flatten() else {
        r.check("the sheet opens on the failed connection", false, "no sheet");
        return;
    };
    let _ = cx.update(|_, cx| d.update(cx, |d, cx| d.demo_test(cx)));
    let tested = until(cx, 20, |cx| cx.update(|_, cx| d.read(cx).demo_status(cx).0.starts_with("failed: ")).unwrap_or(false)).await;
    let said = cx.update(|_, cx| d.read(cx).demo_status(cx).0).unwrap_or_default();
    let status = probed("dlg-status", cx);
    let copied = match status {
        Some(b) if tested => copy_from_tip(centre(b), cx).await,
        _ => Err(format!("no failed Test to hover: {said}, status {status:?}")),
    };
    let title = said.trim_start_matches("failed: ");
    r.check("Test's failure copies from its tooltip", copied.as_deref().is_ok_and(|c| c.starts_with(title)), format!("{copied:?} for {said}"));
    leave(cx).await;
    key(cx, "escape");
    until(cx, 3, |cx| !dialog_open(ws, cx)).await;
}

async fn notice(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let full = "Could not save the folders: permission denied\ncaused by: the disk said no";
    let _ = cx.update(|_, cx| ws.update(cx, |w, cx| w.notice_err(full, cx)));
    draw(cx);
    let copied = match probed("notice-error", cx) {
        Some(b) => copy_from_tip(centre(b), cx).await,
        None => Err("no error notice in the status bar".into()),
    };
    r.check("a notice that failed copies whole, past its first line", copied.as_deref() == Ok(full), format!("{copied:?}"));
    leave(cx).await;
    let _ = cx.update(|_, cx| ws.update(cx, |w, _| w.flash_notice = None));
    draw(cx);
}

async fn query_error(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let (p, pw) = profile(Engine::Postgres);
    if let Err(e) = connect(ws, &p, &pw, cx).await {
        r.check("connected to PostgreSQL", false, e);
        return;
    }
    // PostgreSQL answers with a message and a HINT: the copy must hold both.
    let Some(tab) = run_keys(ws, "SELECT relnam FROM pg_class", "cmd-enter", cx).await else {
        r.check("a query tab", false, "none");
        return;
    };
    let error = cx.update(|_, cx| tab.read(cx).result().and_then(|rs| rs.status().error)).ok().flatten();
    let Some(want) = error.map(|e| e.to_string()) else {
        r.check("the query fails", false, "no error");
        return;
    };
    until(cx, 3, |cx| probed("ev-copy", cx).is_some()).await;
    set_clipboard(UNTOUCHED, cx);
    let clicked = click_probe("ev-copy", cx);
    until(cx, 3, |cx| clipboard(cx) != UNTOUCHED).await;
    let got = clipboard(cx);
    r.check("the error view's Copy error takes the message and its HINT", clicked && got == want && got.contains("HINT:"), format!("{got:?} want {want:?}"));
    snap("query", cx);
    r.check("…through the shared button, which says Copied", until(cx, 2, |cx| probed("ev-copy-copied", cx).is_some()).await, "no Copied");

    let copied = match probed("rb-note", cx) {
        Some(b) => copy_from_tip(centre(b), cx).await,
        None => Err("no note on the result bar".into()),
    };
    r.check("the result bar's error code copies the same text from its tooltip", copied.as_deref() == Ok(want.as_str()), format!("{copied:?}"));
    leave(cx).await;
}

async fn import_failure(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let missing = std::env::temp_dir().join("errors-check-no-such-file.csv");
    let _ = cx.update(|window, cx| ws.update(cx, |w, cx| w.import_data(None, Some(missing.clone()), window, cx)));
    if !until(cx, 10, |cx| probed("di-error", cx).is_some()).await {
        r.check("an import of a missing file shows its error with Copy error", false, "no di-error");
        return;
    }
    // The sheet is still sliding in when the button first lays out.
    let mut last = None;
    until(cx, 5, |cx| {
        let now = probed("di-error", cx);
        std::mem::replace(&mut last, now) == now
    })
    .await;
    snap("import", cx);
    let (button, text) = (probed("di-error", cx), probed("di-error-text", cx));
    r.check(
        "its error wraps beside its button",
        button.zip(text).is_some_and(|(b, t)| b.left() >= t.right() && t.size.width > px(200.)),
        format!("button {button:?} text {text:?}"),
    );
    set_clipboard(UNTOUCHED, cx);
    let clicked = click_probe("di-error", cx);
    until(cx, 3, |cx| clipboard(cx) != UNTOUCHED).await;
    let got = clipboard(cx);
    r.check("an import that cannot read its file copies its error from the sheet", clicked && got != UNTOUCHED && !got.is_empty(), format!("{got:?}"));
    // A button pushed past the sheet's edge would have pressed the scrim, which closes it.
    r.check("…and the click stays in the sheet", cx.update(|_, cx| ws.read(cx).data_import.is_some()).unwrap_or(false), "the sheet closed");
    key(cx, "escape");
}

pub async fn run(ws: &Entity<Workspace>, r: &mut Report, cx: &mut AsyncWindowContext) {
    let id = failed_connection(ws, r, cx).await;
    connection_sheet(&id, ws, r, cx).await;
    notice(ws, r, cx).await;
    query_error(ws, r, cx).await;
    import_failure(ws, r, cx).await;
}
