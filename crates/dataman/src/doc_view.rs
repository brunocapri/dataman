use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding, SharedString, Window,
    actions, div, prelude::*,
};
use dataman_core::Connection;
use dataman_core::guard::WriteClass;
use dataman_core::nosql::{self, Body, Docs, Failed, Output, Page, Ran, Source};

use crate::assets::{Icon, icon};
use crate::editor::{CodeEditor, EditorEvent, Stmt};
use crate::theme::{ActiveTheme, Theme, metrics, px};
use crate::ui::{self, Btn};

actions!(doc_view, [NextDocument, PrevDocument]);

const CONTEXT: &str = "DocView";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("down", NextDocument, Some("DocList")),
        KeyBinding::new("up", PrevDocument, Some("DocList")),
    ]);
}

pub enum DocEvent {
    Use(String),
    Finished,
    Failed { stmt: std::ops::Range<usize>, at: Option<usize>, message: String },
}

struct Last {
    stmt: Stmt,
    page: Page,
    result: Result<Ran, Failed>,
    lines: Vec<(SharedString, SharedString)>,
}

struct Confirm {
    stmt: Stmt,
    why: String,
}

pub struct DocPane {
    conn: Arc<Connection>,
    pub db: String,
    focus: FocusHandle,
    list_focus: FocusHandle,
    /// Dropping the task stops it (and the server's operation with it).
    running: Option<(gpui::Task<()>, Instant, Stmt)>,
    last: Option<Last>,
    selected: Option<usize>,
    detail: Option<Entity<CodeEditor>>,
    detail_base: String,
    new_doc: bool,
    confirm: Option<Confirm>,
    delete_armed: bool,
    save_armed: bool,
    reselect: Option<Body>,
    list_scroll: gpui::ScrollHandle,
    pub zoomed: bool,
    saving: bool,
    note: Option<(String, bool)>,
    queue: Vec<Stmt>,
    agent: bool,
}

impl EventEmitter<DocEvent> for DocPane {}

impl Focusable for DocPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn summary(body: &Body, id: Option<&Body>) -> String {
    let id_json = match id {
        Some(Body::Json(j)) => Some(j.clone()),
        _ => None,
    };
    let mut out = String::new();
    let mut push = |k: &str, v: String| {
        if out.len() > 240 {
            return;
        }
        if !out.is_empty() {
            out.push_str(",  ");
        }
        out.push_str(k);
        out.push_str(": ");
        let v: String = v.chars().take(60).collect();
        out.push_str(&v);
    };
    match body {
        Body::Bson(nosql::bson::Bson::Document(d)) => {
            for (k, v) in d.iter().filter(|(k, _)| *k != "_id") {
                push(k, nosql::shell::scalar_text(v));
            }
        }
        Body::Json(serde_json::Value::Object(m)) => {
            for (k, v) in m.iter().filter(|(_, v)| id_json.as_ref() != Some(*v)) {
                push(k, Body::Json(v.clone()).inline());
            }
        }
        other => out = other.inline(),
    }
    out
}

fn line(doc: &nosql::Doc) -> (SharedString, SharedString) {
    (summary(&doc.body, doc.id.as_ref()).into(), doc.id.as_ref().map(id_text).unwrap_or_default().into())
}

enum SearchEdit {
    Saved { ix: usize, body: Body },
    Inserted(nosql::Doc),
    Deleted { ix: usize },
}

fn per_doc(source: Source, doc: Option<&nosql::Doc>) -> Source {
    match (source, doc.and_then(|d| d.index.clone())) {
        (Source::Search { .. }, Some(index)) => Source::Search { index },
        (s, _) => s,
    }
}

fn id_text(id: &Body) -> String {
    match id {
        Body::Bson(nosql::bson::Bson::ObjectId(o)) => format!("…{}", &o.to_hex()[14..]),
        Body::Bson(nosql::bson::Bson::String(s)) => s.clone(),
        Body::Json(serde_json::Value::String(s)) => s.clone(),
        other => other.inline(),
    }
}

impl DocPane {
    pub fn new(conn: Arc<Connection>, db: String, cx: &mut Context<Self>) -> Self {
        DocPane {
            conn,
            db,
            focus: cx.focus_handle(),
            list_focus: cx.focus_handle(),
            running: None,
            last: None,
            selected: None,
            detail: None,
            detail_base: String::new(),
            new_doc: false,
            confirm: None,
            delete_armed: false,
            save_armed: false,
            reselect: None,
            list_scroll: gpui::ScrollHandle::new(),
            zoomed: false,
            saving: false,
            note: None,
            queue: Vec::new(),
            agent: false,
        }
    }

    pub fn started(&self) -> Option<Instant> {
        self.running.as_ref().map(|(_, at, _)| *at)
    }

    pub fn elapsed(&self) -> Option<Duration> {
        match (&self.running, &self.last) {
            (Some((_, at, _)), _) => Some(at.elapsed()),
            (None, Some(Last { result: Ok(r), .. })) => Some(r.elapsed),
            _ => None,
        }
    }

    pub fn set_db(&mut self, db: String, cx: &mut Context<Self>) {
        self.db = db;
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub fn shown(&self) -> Option<(&str, Result<&Ran, &Failed>)> {
        self.last.as_ref().map(|l| (l.stmt.sql.as_str(), l.result.as_ref()))
    }

    pub fn list_focus(&self) -> FocusHandle {
        self.list_focus.clone()
    }

    #[cfg(feature = "checks")]
    pub fn demo_selected(&self) -> Option<usize> {
        self.selected
    }

    #[cfg(feature = "checks")]
    pub fn demo_select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.select(ix, cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_list_bounds(&self, ix: usize) -> Option<(gpui::Bounds<gpui::Pixels>, gpui::Bounds<gpui::Pixels>)> {
        // A child's bounds are its place in the content: the scroll moves it by the offset.
        let row = self.list_scroll.bounds_for_item(ix)?;
        Some((self.list_scroll.bounds(), gpui::Bounds { origin: row.origin + self.list_scroll.offset(), size: row.size }))
    }

    #[cfg(feature = "checks")]
    pub fn demo_detail(&self) -> Option<Entity<CodeEditor>> {
        self.detail.clone()
    }

    #[cfg(feature = "checks")]
    pub fn demo_save(&mut self, cx: &mut Context<Self>) {
        self.save(cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_page(&mut self, skip: u64, cx: &mut Context<Self>) {
        self.go_page(skip, cx);
    }

    #[cfg(feature = "checks")]
    pub fn has_confirm(&self) -> bool {
        self.confirm.is_some()
    }

    #[cfg(feature = "checks")]
    pub fn demo_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_document(window, cx);
    }

    #[cfg(feature = "checks")]
    pub fn demo_note(&self) -> Option<(String, bool)> {
        self.note.clone()
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.queue.clear();
        if self.running.take().is_some() {
            cx.emit(DocEvent::Finished);
            cx.notify();
        }
    }

    pub fn run(&mut self, stmts: Vec<Stmt>, cx: &mut Context<Self>) {
        let mut stmts: Vec<Stmt> = stmts.into_iter().filter(|s| !s.sql.trim().is_empty()).collect();
        if stmts.is_empty() {
            return;
        }
        let (mut reasons, mut writes) = (Vec::new(), false);
        for s in &stmts {
            match nosql::classify(self.conn.engine(), &s.sql) {
                Ok(WriteClass::Destructive { reasons: r }) => {
                    writes = true;
                    reasons.extend(r.into_iter().filter(|r| !reasons.contains(r)).collect::<Vec<_>>());
                }
                Ok(WriteClass::SafeWrite) => writes = true,
                _ => {}
            }
        }
        let what = if stmts.len() > 1 { "This run" } else if nosql::is_script(self.conn.engine(), &stmts[0].sql) { "This script" } else { "This statement" };
        let why = if !reasons.is_empty() {
            Some(format!("{what} {}.", reasons.join(", ")))
        } else if writes && self.conn.profile.env.is_production() {
            Some(format!("{what} writes to production."))
        } else {
            None
        };
        let first = stmts.remove(0);
        self.queue = stmts;
        if let Some(why) = why {
            self.confirm = Some(Confirm { stmt: first, why });
            cx.notify();
            return;
        }
        self.start(first, Page::default(), false, cx);
    }

    pub fn resolve_confirm(&mut self, run: bool, cx: &mut Context<Self>) {
        match self.confirm.take() {
            Some(c) if run => self.start(c.stmt, Page::default(), false, cx),
            _ => self.queue.clear(),
        }
        cx.notify();
    }

    pub fn show_pending(&mut self, stmt: Stmt, cx: &mut Context<Self>) {
        self.queue.clear();
        self.confirm = None;
        self.note = None;
        self.agent = true;
        self.running = Some((gpui::Task::ready(()), Instant::now(), stmt));
        cx.emit(DocEvent::Finished);
        cx.notify();
    }

    pub fn show_done(&mut self, ran: Result<Ran, String>, cx: &mut Context<Self>) {
        let Some((_, _, stmt)) = &self.running else { return };
        let stmt = stmt.clone();
        let limit = match &ran {
            Ok(Ran { output: Output::Docs(d), .. }) if d.more && !d.docs.is_empty() => d.docs.len(),
            _ => nosql::PAGE,
        };
        let r = ran.map_err(|m| anyhow::Error::new(Failed { message: m, at: None }));
        self.finished(stmt, Page { skip: 0, limit }, r, cx);
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.queue.clear();
        self.running = None;
        self.confirm = None;
        self.last = None;
        self.selected = None;
        self.note = None;
        self.load_detail(cx);
        cx.emit(DocEvent::Finished);
        cx.notify();
    }

    /// `again`: a script's final cursor is read again, not the script.
    fn start(&mut self, stmt: Stmt, page: Page, again: bool, cx: &mut Context<Self>) {
        self.confirm = None;
        self.agent = false;
        if self.last.as_ref().is_none_or(|l| l.stmt.sql != stmt.sql) {
            self.note = None;
        }
        let started = Instant::now();
        let rerun = self.last.as_ref().filter(|l| again && l.stmt.sql == stmt.sql).and_then(|l| l.result.as_ref().ok()).and_then(|r| r.rerun.clone());
        let run = match rerun {
            Some(r) => self.conn.rerun_nosql(&r, page),
            None => self.conn.run_nosql(&self.db, &stmt.sql, page),
        };
        let s = stmt.clone();
        let task = cx.spawn(async move |this, cx| {
            let r = run.await;
            let _ = this.update(cx, |p, cx| p.finished(s, page, r, cx));
        });
        self.running = Some((task, started, stmt));
        cx.emit(DocEvent::Finished);
        cx.notify();
    }

    fn finished(&mut self, stmt: Stmt, page: Page, r: anyhow::Result<Ran>, cx: &mut Context<Self>) {
        let elapsed = self.running.take().map(|(_, at, _)| at.elapsed()).unwrap_or_default();
        let result = r.map_err(|e| e.downcast_ref::<Failed>().cloned().unwrap_or_else(|| Failed { message: format!("{e:#}"), at: None }));
        self.record_history(&stmt, &result, elapsed);
        self.agent = false;
        match &result {
            Ok(Ran { output: Output::Use(db), .. }) | Ok(Ran { moved: Some(db), .. }) => cx.emit(DocEvent::Use(db.clone())),
            Err(f) => cx.emit(DocEvent::Failed { stmt: stmt.at..stmt.at + stmt.sql.len(), at: f.at.map(|a| stmt.at + a), message: f.message.clone() }),
            _ => {}
        }
        let next = if result.is_ok() && !self.queue.is_empty() { Some(self.queue.remove(0)) } else { None };
        if result.is_err() {
            self.queue.clear();
        }
        let lines = match &result {
            Ok(Ran { output: Output::Docs(d), .. }) => d
                .docs
                .iter()
                .map(line)
                .collect(),
            _ => Vec::new(),
        };
        let same = self.last.as_ref().is_some_and(|l| l.stmt.sql == stmt.sql && l.page == page);
        let n = lines.len();
        // The document just saved keeps the selection wherever the page puts it now (a search's
        // order, MongoDB's natural order, can move it); else the same place.
        let again = self.reselect.take().and_then(|id| match &result {
            Ok(Ran { output: Output::Docs(d), .. }) => d.docs.iter().position(|doc| doc.id.as_ref() == Some(&id)),
            _ => None,
        });
        self.last = Some(Last { stmt, page, result, lines });
        self.selected = if n == 0 { None } else { Some(again.unwrap_or(if same { self.selected.unwrap_or(0).min(n - 1) } else { 0 })) };
        if let Some(ix) = self.selected {
            self.list_scroll.scroll_to_item(ix);
        }
        self.new_doc = false;
        self.delete_armed = false;
        self.load_detail(cx);
        if let Some(next) = next {
            self.start(next, Page::default(), false, cx);
            return;
        }
        cx.emit(DocEvent::Finished);
        cx.notify();
    }

    fn record_history(&self, stmt: &Stmt, r: &Result<Ran, Failed>, elapsed: Duration) {
        let (rows, outcome, error) = match r {
            Ok(Ran { output: Output::Docs(d), .. }) => (Some(d.docs.len() as u64), dataman_core::Outcome::Ok, None),
            Ok(Ran { output: Output::Written { affected, .. }, .. }) => (Some(*affected), dataman_core::Outcome::Ok, None),
            Ok(_) => (None, dataman_core::Outcome::Ok, None),
            Err(f) => (None, dataman_core::Outcome::Error, Some(f.message.clone())),
        };
        self.conn.record_history(&dataman_core::HistoryEntry {
            ts: chrono::Utc::now(),
            database: self.db.clone(),
            sql: stmt.sql.clone(),
            duration_ms: elapsed.as_secs_f64() * 1e3,
            rows,
            outcome,
            error,
            source: self.agent.then(|| dataman_core::history::AGENT.to_string()),
        });
    }

    fn go_page(&mut self, skip: u64, cx: &mut Context<Self>) {
        let Some(l) = &self.last else { return };
        let page = Page { skip, limit: l.page.limit };
        self.start(l.stmt.clone(), page, true, cx);
    }

    fn docs(&self) -> Option<&Docs> {
        match &self.last.as_ref()?.result {
            Ok(Ran { output: Output::Docs(d), .. }) => Some(d),
            _ => None,
        }
    }

    fn detail_text(&self) -> String {
        let Some(l) = &self.last else { return String::new() };
        match &l.result {
            Ok(Ran { output: Output::Docs(d), .. }) => self.selected.and_then(|i| d.docs.get(i)).map(|d| d.body.text()).unwrap_or_default(),
            Ok(Ran { output: Output::Value(v), .. }) => v.text(),
            Ok(Ran { output: Output::Written { detail: Some(v), .. }, .. }) => v.text(),
            _ => String::new(),
        }
    }

    fn load_detail(&mut self, cx: &mut Context<Self>) {
        let text = self.detail_text();
        self.detail_base = text.clone();
        if let Some(d) = &self.detail {
            d.update(cx, |e, cx| {
                e.set_text(&text, cx);
                e.set_cursor(0, cx);
            });
        }
    }

    fn dirty(&self, cx: &App) -> bool {
        self.new_doc || self.detail.as_ref().is_some_and(|d| d.read(cx).text() != self.detail_base)
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let n = self.docs().map_or(0, |d| d.docs.len());
        if n == 0 {
            return;
        }
        let ix = ix.min(n - 1);
        if self.selected == Some(ix) && !self.new_doc {
            return;
        }
        self.selected = Some(ix);
        self.list_scroll.scroll_to_item(ix);
        self.new_doc = false;
        self.delete_armed = false;
        self.save_armed = false;
        self.note = None;
        self.load_detail(cx);
        cx.notify();
    }

    fn new_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_doc = true;
        self.delete_armed = false;
        self.note = None;
        let text = self.docs().and_then(|d| d.source.as_ref()).map(nosql::new_template).unwrap_or_else(|| "{\n  \n}".to_string());
        self.detail_base = String::new();
        if let Some(d) = &self.detail {
            d.update(cx, |e, cx| {
                e.set_text(&text, cx);
                e.set_cursor(4, cx);
            });
            window.focus(&d.focus_handle(cx), cx);
        }
        cx.notify();
    }

    fn revert(&mut self, cx: &mut Context<Self>) {
        self.new_doc = false;
        self.note = None;
        self.load_detail(cx);
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.docs().and_then(|d| d.source.clone()) else { return };
        let Some(text) = self.detail.as_ref().map(|d| d.read(cx).text().to_string()) else { return };
        if self.saving || !self.dirty(cx) {
            return;
        }
        if self.conn.profile.env.is_production() && !std::mem::take(&mut self.save_armed) {
            self.save_armed = true;
            self.note = Some(("This is production: Save again to write it".into(), true));
            cx.notify();
            return;
        }
        let doc = if self.new_doc { None } else { self.selected.and_then(|i| self.docs()?.docs.get(i)).cloned() };
        let id = doc.as_ref().and_then(|d| d.id.clone());
        let source = per_doc(source, doc.as_ref());
        if !self.new_doc && id.is_none() {
            self.note = Some(("This document has no _id: it cannot be saved back.".into(), true));
            cx.notify();
            return;
        }
        self.saving = true;
        let task = self.conn.save_document(&source, id, &text);
        let inserted = self.new_doc;
        let at = self.selected;
        let search_index = match &source {
            Source::Search { index } => Some(index.clone()),
            _ => None,
        };
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |p, cx| {
                p.saving = false;
                match r {
                    Ok(id) => {
                        p.new_doc = false;
                        let body = search_index.as_ref().and_then(|_| nosql::shell::parse(&text).ok()).and_then(|v| nosql::shell::to_json(&v).ok()).map(Body::Json);
                        let edit = match (body, &search_index, inserted, at) {
                            (Some(body), Some(index), true, _) => Some(SearchEdit::Inserted(nosql::Doc { id: Some(id.clone()), index: Some(index.clone()), body })),
                            (Some(body), Some(_), false, Some(ix)) => Some(SearchEdit::Saved { ix, body }),
                            _ => None,
                        };
                        if !edit.is_some_and(|e| p.edit_search_page(e, cx)) {
                            p.reselect = Some(id);
                            p.refresh(cx);
                        }
                        p.note = Some((if inserted { "Inserted".into() } else { "Saved".into() }, false));
                    }
                    Err(e) => {
                        let f = e.downcast_ref::<Failed>().cloned();
                        if let (Some(at), Some(d)) = (f.as_ref().and_then(|f| f.at), &p.detail) {
                            d.update(cx, |e, cx| e.set_cursor(at, cx));
                        }
                        p.note = Some((f.map(|f| f.message).unwrap_or_else(|| format!("{e:#}")), true));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        if !self.delete_armed {
            self.delete_armed = true;
            cx.notify();
            return;
        }
        self.delete_armed = false;
        let Some(source) = self.docs().and_then(|d| d.source.clone()) else { return };
        let Some(doc) = self.selected.and_then(|i| self.docs()?.docs.get(i)).cloned() else { return };
        let Some(id) = doc.id.clone() else { return };
        let source = per_doc(source, Some(&doc));
        let search = matches!(source, Source::Search { .. });
        let at = self.selected;
        let task = self.conn.delete_document(&source, id);
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |p, cx| {
                match r {
                    Ok(n) => {
                        if !(search && at.is_some_and(|ix| p.edit_search_page(SearchEdit::Deleted { ix }, cx))) {
                            p.refresh(cx);
                        }
                        p.note = Some((if n == 1 { "Deleted".into() } else { "Already gone".into() }, false));
                    }
                    Err(e) => p.note = Some((format!("{e:#}"), true)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn edit_search_page(&mut self, edit: SearchEdit, cx: &mut Context<Self>) -> bool {
        let Some(l) = &mut self.last else { return false };
        let Ok(Ran { output: Output::Docs(d), .. }) = &mut l.result else { return false };
        let selected = match edit {
            SearchEdit::Saved { ix, body } => {
                let Some(doc) = d.docs.get_mut(ix) else { return false };
                doc.body = body;
                l.lines[ix] = line(doc);
                ix
            }
            SearchEdit::Inserted(doc) => {
                l.lines.insert(0, line(&doc));
                d.docs.insert(0, doc);
                d.total = d.total.map(|t| t + 1);
                0
            }
            SearchEdit::Deleted { ix } => {
                if ix >= d.docs.len() {
                    return false;
                }
                d.docs.remove(ix);
                l.lines.remove(ix);
                d.total = d.total.map(|t| t.saturating_sub(1));
                if d.docs.is_empty() {
                    self.selected = None;
                    self.load_detail(cx);
                    return true;
                }
                ix.min(d.docs.len() - 1)
            }
        };
        self.selected = Some(selected);
        self.list_scroll.scroll_to_item(selected);
        self.load_detail(cx);
        true
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(l) = &self.last {
            let (stmt, page) = (l.stmt.clone(), l.page);
            self.start(stmt, page, true, cx);
        }
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        let text = match (self.docs(), self.selected) {
            (Some(d), _) if d.docs.len() > 1 && self.selected.is_none() => {
                serde_json::to_string_pretty(&d.docs.iter().map(|d| d.body.to_json()).collect::<Vec<_>>()).unwrap_or_default()
            }
            _ => self.detail.as_ref().map(|d| d.read(cx).text().to_string()).unwrap_or_default(),
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn ensure_detail(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<CodeEditor> {
        if let Some(d) = &self.detail {
            return d.clone();
        }
        let engine = self.conn.engine();
        let text = self.detail_text();
        let d = cx.new(|cx| {
            let mut e = CodeEditor::new(window, cx);
            e.set_doc_lang(Some(engine), cx);
            // A document, not a statement: ⌘↩ saves it however the console would read it.
            e.set_runs_whole_text(true);
            e.set_text(&text, cx);
            e.set_cursor(0, cx);
            e
        });
        cx.subscribe(&d, |p, _, ev: &EditorEvent, cx| match ev {
            EditorEvent::Run(_) | EditorEvent::RunAll(_) => p.save(cx),
            EditorEvent::Changed => cx.notify(),
            _ => {}
        })
        .detach();
        self.detail_base = text;
        self.detail = Some(d.clone());
        d
    }
}

impl DocPane {
    fn render_bar(&self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let ghost = |id: &'static str| ui::button(t, id, Btn::Ghost).h(px(24.)).px(px(7.)).gap(px(5.)).text_size(px(11.5));
        let mut left: Vec<gpui::AnyElement> = Vec::new();
        let metric = |k: &str, v: String| {
            div().flex().flex_row().gap(px(5.)).child(div().text_color(t.tx4).child("·")).child(div().text_color(t.tx3).child(k.to_string())).child(div().text_color(t.tx).child(v))
        };
        let count = |n: String, what: &str| {
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap(px(5.))
                .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(t.tx).child(n))
                .child(div().text_color(t.tx3).child(what.to_string()))
        };
        let mut pager = None;
        if let Some((_, at, _)) = &self.running {
            left.push(ui::pill("◌", "RUNNING", t.acc, t.acc_s).into_any_element());
            left.push(metric("elapsed", crate::grid::fmt_clock(at.elapsed())).into_any_element());
        } else if let Some(l) = &self.last {
            match &l.result {
                Ok(ran) => {
                    let total = metric("total", crate::app::fmt_dur(ran.elapsed));
                    match &ran.output {
                        Output::Docs(d) => {
                            left.push(ui::pill("▦", "DOCS", t.ok, t.ok_s).into_any_element());
                            let n = d.docs.len();
                            let redis = matches!(d.source, Some(Source::Redis { .. }));
                            let what = match (redis, n == 1) {
                                (true, true) => "entry",
                                (true, false) => "entries",
                                (false, true) => "document",
                                (false, false) => "documents",
                            };
                            left.push(count(crate::grid::ui_count(n), what).into_any_element());
                            if let Some(all) = d.total {
                                left.push(metric("of", crate::grid::ui_count(all as usize)).into_any_element());
                            }
                            left.push(total.into_any_element());
                            if d.skip > 0 || d.more {
                                let (skip, limit) = (d.skip, l.page.limit as u64);
                                let range = if n == 0 { "–".to_string() } else { format!("{}–{}", skip + 1, skip + n as u64) };
                                let prev_on = skip > 0;
                                let next_on = d.more;
                                pager = Some(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap(px(2.))
                                        .child(
                                            ui::icon_button(t, "doc-prev", Icon::ChevronLeft, 22., 12.)
                                                .when(!prev_on, |d| d.opacity(0.35))
                                                .on_click(cx.listener(move |p, _, _, cx| {
                                                    if prev_on {
                                                        p.go_page(skip.saturating_sub(limit), cx);
                                                    }
                                                })),
                                        )
                                        .child(div().text_size(px(11.)).text_color(t.tx2).child(range))
                                        .child(
                                            ui::icon_button(t, "doc-next", Icon::ChevronRight, 22., 12.)
                                                .when(!next_on, |d| d.opacity(0.35))
                                                .on_click(cx.listener(move |p, _, _, cx| {
                                                    if next_on {
                                                        p.go_page(skip + limit, cx);
                                                    }
                                                })),
                                        ),
                                );
                            }
                        }
                        Output::Value(_) => {
                            left.push(ui::pill("✓", "DONE", t.tx2, t.hov).into_any_element());
                            left.push(total.into_any_element());
                        }
                        Output::Written { affected, summary, .. } => {
                            left.push(ui::pill("✓", "WRITTEN", t.ok, t.ok_s).into_any_element());
                            let _ = affected;
                            left.push(div().text_color(t.tx).child(summary.clone()).into_any_element());
                            left.push(total.into_any_element());
                        }
                        Output::Use(db) => {
                            left.push(ui::pill("✓", "DONE", t.tx2, t.hov).into_any_element());
                            left.push(div().text_color(t.tx2).child(format!("switched to {db}")).into_any_element());
                        }
                    }
                }
                Err(_) => left.push(ui::pill("✕", "ERROR", t.err, t.err_s).into_any_element()),
            }
        }
        let can_new = self.docs().is_some_and(|d| d.source.is_some()) && self.running.is_none();
        div()
            .h(px(metrics::RESULT_BAR_H))
            .flex_none()
            .pl(px(10.))
            .pr(px(8.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .border_b_1()
            .border_color(t.line)
            .text_color(t.tx2)
            .text_size(px(11.5))
            .whitespace_nowrap()
            .child(div().flex_1().min_w_0().overflow_hidden().flex().flex_row().items_center().gap(px(10.)).children(left).children(pager))
            .when(can_new, |d| {
                d.child(
                    ghost("doc-new")
                        .child(icon(Icon::Plus).size(px(13.)).text_color(t.tx2))
                        .child("New")
                        .on_click(cx.listener(|p, _, window, cx| p.new_document(window, cx))),
                )
            })
            .child(ui::vsep(t, 16.))
            .child(ghost("doc-copy").child(icon(Icon::Copy).size(px(13.)).text_color(t.tx2)).child("Copy").on_click(cx.listener(|p, _, _, cx| p.copy(cx))))
            .child({
                let zoomed = self.zoomed;
                let key = if zoomed { Some("esc".to_string()) } else { crate::keymap::key("zoom_result") };
                ui::button(t, "doc-zoom", Btn::Ghost)
                    .h(px(24.))
                    .w(px(26.))
                    .px(px(0.))
                    .justify_center()
                    .when(zoomed, |d| d.bg(t.acc_s).text_color(t.acc_t).border_1().border_color(t.acc))
                    .child(icon(if zoomed { Icon::Collapse } else { Icon::Expand }).size(px(13.)).text_color(if zoomed { t.acc_t } else { t.tx2 }))
                    .tooltip(ui::tooltip(if zoomed { "Exit zoom" } else { "Zoom result" }, None, key.as_deref()))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(crate::app::ToggleResultZoom), cx))
            })
    }

    fn render_confirm(&self, c: &Confirm, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let prod = self.conn.profile.env.is_production();
        div()
            .mx(px(12.))
            .mt(px(12.))
            .px(px(12.))
            .py(px(10.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .bg(if prod { t.err_s } else { t.warn_s })
            .border_1()
            .border_color(if prod { t.err } else { t.warn })
            .text_size(px(11.5))
            .child(div().flex_none().text_color(if prod { t.err } else { t.warn }).child("!"))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(div().text_color(t.tx).child(c.why.clone()))
                    .child(div().text_color(t.tx3).overflow_hidden().whitespace_nowrap().text_ellipsis().child(c.stmt.sql.lines().next().unwrap_or("").to_string())),
            )
            .child(ui::button(t, "doc-confirm-cancel", Btn::Ghost).h(px(26.)).child("Cancel").on_click(cx.listener(|p, _, _, cx| p.resolve_confirm(false, cx))))
            .child(
                ui::button(t, "doc-confirm-run", if prod { Btn::Danger } else { Btn::Primary })
                    .h(px(26.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Run")
                    .on_click(cx.listener(|p, _, _, cx| p.resolve_confirm(true, cx))),
            )
    }

    fn render_list(&self, d: &Docs, lines: &[(SharedString, SharedString)], t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (hov, sel_bg, focused) = (t.hov, t.acc_s, true);
        let base = d.skip as usize;
        let rows = lines.iter().enumerate().map(|(i, (sum, id))| {
            let sel = self.selected == Some(i) && !self.new_doc;
            div()
                .id(("doc-row", i))
                .h(px(26.))
                .px(px(10.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .text_size(px(11.5))
                .when(sel && focused, |d| d.bg(sel_bg))
                .when(!sel, |d| d.hover(move |s| s.bg(hov)))
                .child(div().w(px(28.)).flex_none().text_right().text_color(t.tx4).child((base + i + 1).to_string()))
                .when(!id.is_empty(), |d| d.child(div().flex_none().max_w(px(240.)).overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.num).child(id.clone())))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx2).child(sum.clone()))
                .on_click(cx.listener(move |p, _, window, cx| {
                    window.focus(&p.list_focus, cx);
                    p.select(i, cx);
                }))
        });
        div()
            .id("doc-list")
            .key_context("DocList")
            .track_focus(&self.list_focus)
            .track_scroll(&self.list_scroll)
            .on_action(cx.listener(|p, _: &NextDocument, _, cx| p.select(p.selected.map_or(0, |s| s + 1), cx)))
            .on_action(cx.listener(|p, _: &PrevDocument, _, cx| p.select(p.selected.map_or(0, |s| s.saturating_sub(1)), cx)))
            .size_full()
            .overflow_y_scroll()
            .py(px(4.))
            .children(rows)
            .when(lines.is_empty(), |d| d.child(div().pt(px(40.)).flex().justify_center().text_color(t.tx3).text_size(px(12.)).child("No documents")))
    }

    fn render_detail(&self, editor: Entity<CodeEditor>, editable: bool, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let dirty = self.dirty(cx);
        let title: String = if self.new_doc {
            "New document".into()
        } else {
            match (self.docs(), self.selected) {
                (Some(d), Some(i)) if matches!(d.source, Some(Source::Redis { .. })) => format!("Entry {}", d.skip as usize + i + 1),
                (Some(d), Some(i)) => format!("Document {}", d.skip as usize + i + 1),
                _ => String::new(),
            }
        };
        let header = editable.then(|| {
            let save = ui::button(t, "doc-save", if dirty { Btn::Primary } else { Btn::Secondary })
                .h(px(24.))
                .px(px(8.))
                .text_size(px(11.5))
                .when(!dirty || self.saving, |d| d.opacity(0.5))
                .child(if self.saving {
                    "Saving…"
                } else if self.save_armed {
                    "Save — sure?"
                } else if self.new_doc {
                    "Insert"
                } else {
                    "Save"
                })
                .on_click(cx.listener(|p, _, _, cx| p.save(cx)));
            let revert = (dirty && !self.saving).then(|| ui::button(t, "doc-revert", Btn::Ghost).h(px(24.)).px(px(8.)).text_size(px(11.5)).child("Revert").on_click(cx.listener(|p, _, _, cx| p.revert(cx))));
            let delete = (!self.new_doc && self.selected.is_some()).then(|| {
                ui::button(t, "doc-delete", if self.delete_armed { Btn::Danger } else { Btn::Ghost })
                    .h(px(24.))
                    .px(px(8.))
                    .text_size(px(11.5))
                    .when(!self.delete_armed, |d| d.text_color(t.err))
                    .child(icon(Icon::Trash).size(px(12.)).text_color(t.err))
                    .child(if self.delete_armed { "Delete — sure?" } else { "Delete" })
                    .on_click(cx.listener(|p, _, _, cx| p.delete(cx)))
            });
            div()
                .h(px(34.))
                .flex_none()
                .px(px(12.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .border_b_1()
                .border_color(t.line)
                .child(div().text_size(px(11.5)).font_weight(FontWeight::SEMIBOLD).text_color(t.tx).child(title))
                .children(self.note.clone().map(|(n, err)| div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.)).text_color(if err { t.err } else { t.ok }).child(n)))
                .child(div().flex_1())
                .children(delete)
                .children(revert)
                .child(save)
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .children(header)
            .child(div().flex_1().min_h_0().relative().overflow_hidden().child(editor))
    }

    fn render_printed(&self, lines: &[String], t: &Theme) -> impl IntoElement + use<> {
        div()
            .id("doc-printed")
            .flex_none()
            .max_h(px(110.))
            .overflow_y_scroll()
            .px(px(12.))
            .py(px(6.))
            .border_b_1()
            .border_color(t.line)
            .bg(t.panel)
            .font_family(crate::theme::mono())
            .text_size(px(11.5))
            .text_color(t.tx2)
            .children(lines.iter().map(|l| div().whitespace_nowrap().child(l.clone())))
    }

    fn render_error(&self, f: &Failed, stmt: &Stmt, t: &Theme) -> impl IntoElement + use<> {
        let place = f.at.map(|a| {
            let before = &stmt.sql[..crate::tab::floor_char(&stmt.sql, a.min(stmt.sql.len()))];
            let line = before.matches('\n').count() + 1;
            let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
            format!("line {line}, col {col}")
        });
        div()
            .p(px(20.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(10.))
                    .text_size(px(13.))
                    .child(div().text_color(t.err).child("✕"))
                    .child(div().text_color(t.tx).child(f.message.clone())),
            )
            .children(place.map(|p| div().pl(px(22.)).text_size(px(11.5)).text_color(t.tx3).child(p)))
    }
}

impl Render for DocPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let editor = self.ensure_detail(window, cx);
        let bar = self.render_bar(&t, cx);
        let confirm = self.confirm.as_ref().map(|c| self.render_confirm(c, &t, cx).into_any_element());
        let body: gpui::AnyElement = match self.last.as_ref().map(|l| (&l.result, &l.stmt, &l.lines)) {
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(t.tx3)
                .text_size(px(12.))
                .child(if self.running.is_some() { "Running…" } else { "" })
                .into_any_element(),
            Some((Err(f), stmt, _)) => self.render_error(f, stmt, &t).into_any_element(),
            Some((Ok(Ran { output: Output::Docs(d), .. }), _, lines)) => {
                let (d, lines) = (d.clone(), lines.clone());
                let editable = d.source.is_some();
                let show_detail = self.selected.is_some() || self.new_doc;
                div()
                    .size_full()
                    .flex()
                    .flex_row()
                    .child(div().flex_1().min_w(px(220.)).h_full().child(self.render_list(&d, &lines, &t, cx)))
                    .when(show_detail, |row| {
                        row.child(
                            div()
                                .w(gpui::relative(0.5))
                                .flex_none()
                                .h_full()
                                .border_l_1()
                                .border_color(t.line)
                                .child(self.render_detail(editor.clone(), editable, &t, cx)),
                        )
                    })
                    .into_any_element()
            }
            Some((Ok(Ran { output: Output::Written { detail: None, summary, .. }, .. }), _, _)) => div()
                .p(px(20.))
                .flex()
                .flex_row()
                .gap(px(10.))
                .text_size(px(13.))
                .child(div().text_color(t.ok).child("✓"))
                .child(div().text_color(t.tx).child(summary.clone()))
                .into_any_element(),
            Some((Ok(_), _, _)) => div().size_full().relative().overflow_hidden().child(editor).into_any_element(),
        };
        let printed = match self.last.as_ref().map(|l| &l.result) {
            Some(Ok(r)) if !r.printed.is_empty() && self.running.is_none() => Some(self.render_printed(&r.printed, &t)),
            _ => None,
        };
        div()
            .id("doc-pane")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .child(bar)
            .children(confirm)
            .children(printed)
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_skip_the_id() {
        let d = nosql::bson::doc! { "_id": 1, "name": "Ana", "tags": ["a"], "n": nosql::bson::Bson::Int64(3) };
        assert_eq!(summary(&Body::doc(d), None), "name: \"Ana\",  tags: [1],  n: NumberLong(3)");
        let j = serde_json::json!({ "a": 1, "b": { "c": 2 } });
        assert_eq!(summary(&Body::Json(j), None), "a: 1,  b: {1}");
        let e = serde_json::json!({ "field": "f1", "value": "v" });
        assert_eq!(summary(&Body::Json(e), Some(&Body::Json(serde_json::json!("f1")))), "value: \"v\"");
        assert_eq!(id_text(&Body::Json(serde_json::json!("f1"))), "f1");
        let oid = nosql::bson::oid::ObjectId::parse_str("6ad671d80bb7c7ed855e5c57").unwrap();
        assert_eq!(id_text(&Body::Bson(nosql::bson::Bson::ObjectId(oid))), "…ed855e5c57");
        assert_eq!(id_text(&Body::Bson(nosql::bson::Bson::Int32(7))), "7");
    }
}
