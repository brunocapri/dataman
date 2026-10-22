use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, MouseButton,
    ScrollHandle, SharedString, Task, Window, actions, anchored, deferred, div, prelude::*,
};
use crate::theme::px;
use dataman_core::Connection;
use dataman_core::guard::{self, Finding};
use dataman_core::import::{
    ColumnMap, CsvDialect, Encoding, ImportFormat, ImportOptions, ImportProgress, ImportReport, Method, OnError, Outcome, PgType,
    Preview, auto_map, preview_task, table_name_for,
};
use dataman_core::schema::Column;

use crate::assets::{Icon, icon};
use crate::export_ui::{fmt_bytes, fmt_secs, tilde};
use crate::grid::ui_count;
use crate::input::TextInput;
use crate::theme::{ActiveTheme, Theme, metrics};
use crate::ui::{self, Btn};

actions!(data_import, [CloseDataImport, StartDataImport, StopDataImport]);

const CONTEXT: &str = "DataImport";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        crate::keymap::kb("escape", CloseDataImport, c),
        crate::keymap::kb("cmd-enter", StartDataImport, c),
        crate::keymap::kb("cmd-.", StopDataImport, c),
    ]);
}

pub enum DataImportEvent {
    Close,
    Finished(String),
    ViewData { conn: String, db: String, schema: String, table: String },
}

#[derive(Clone)]
pub struct DataTarget {
    pub conn_id: String,
    pub conn: Arc<Connection>,
    pub db: String,
    pub schema: String,
    pub table: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Delimiter,
    Quote,
    Header,
    Encoding,
    Null,
}

impl Field {
    const ALL: [Field; 5] = [Field::Delimiter, Field::Quote, Field::Header, Field::Encoding, Field::Null];

    fn label(self) -> &'static str {
        match self {
            Field::Delimiter => "DELIMITER",
            Field::Quote => "QUOTE",
            Field::Header => "HEADER",
            Field::Encoding => "ENCODING",
            Field::Null => "NULL AS",
        }
    }

    fn choices(self) -> &'static [&'static str] {
        match self {
            Field::Delimiter => &[", comma", "; semicolon", "⇥ tab", "| pipe"],
            Field::Quote => &["\" double", "' single", "none"],
            Field::Header => &["first line", "none"],
            Field::Encoding => &["UTF-8", "Latin-1", "Windows-1252"],
            Field::Null => &["empty", "NULL", "\\N"],
        }
    }

    fn get(self, d: &CsvDialect) -> usize {
        match self {
            Field::Delimiter => [b',', b';', b'\t', b'|'].iter().position(|c| *c == d.delimiter).unwrap_or(0),
            Field::Quote => match d.quote {
                Some(b'\'') => 1,
                None => 2,
                _ => 0,
            },
            Field::Header => usize::from(!d.header),
            Field::Encoding => Encoding::ALL.iter().position(|e| *e == d.encoding).unwrap_or(0),
            Field::Null => ["", "NULL", "\\N"].iter().position(|n| *n == d.null_text).unwrap_or(0),
        }
    }

    fn set(self, d: &mut CsvDialect, ix: usize) {
        match self {
            Field::Delimiter => d.delimiter = [b',', b';', b'\t', b'|'][ix.min(3)],
            Field::Quote => d.quote = [Some(b'"'), Some(b'\''), None][ix.min(2)],
            Field::Header => d.header = ix == 0,
            Field::Encoding => d.encoding = Encoding::ALL[ix.min(2)],
            Field::Null => d.null_text = ["", "NULL", "\\N"][ix.min(2)].to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Menu {
    Field(Field),
    Column(usize),
}

enum JobState {
    Running,
    Done(ImportReport),
}

struct Job {
    progress: Arc<ImportProgress>,
    started: Instant,
    state: JobState,
    table: String,
    _task: Option<Task<()>>,
    _tick: Option<Task<()>>,
}

pub struct DataImportView {
    focus: FocusHandle,
    target: DataTarget,
    path: PathBuf,
    format: Option<ImportFormat>,
    dialect: Option<CsvDialect>,
    preview: Option<Preview>,
    loading: bool,
    error: Option<String>,
    target_cols: Option<Vec<Column>>,
    map: Vec<Option<usize>>,
    types: Vec<PgType>,
    table_name: Entity<TextInput>,
    truncate: bool,
    skip: bool,
    one_tx: bool,
    menu: Option<Menu>,
    guard: Option<(Vec<Finding>, Vec<String>)>,
    job: Option<Job>,
    grid_scroll: ScrollHandle,
    pub visible: bool,
    _preview: Option<Task<()>>,
    _quit: gpui::Subscription,
}

impl EventEmitter<DataImportEvent> for DataImportView {}

impl Focusable for DataImportView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

const COL_W: f32 = 148.;
const ROW_H: f32 = 22.;
const HEAD_H: f32 = 44.;
const GRID_H: f32 = 236.;

impl DataImportView {
    pub fn new(target: DataTarget, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let name = table_name_for(&path);
        let table_name = cx.new(|cx| {
            let mut i = TextInput::new(cx, "table");
            i.set_text(name, cx);
            i
        });
        let mut v = DataImportView {
            focus,
            target,
            path,
            format: None,
            dialect: None,
            preview: None,
            loading: false,
            error: None,
            target_cols: None,
            map: Vec::new(),
            types: Vec::new(),
            table_name,
            truncate: false,
            skip: false,
            one_tx: true,
            menu: None,
            guard: None,
            job: None,
            grid_scroll: ScrollHandle::new(),
            visible: true,
            _preview: None,
            _quit: cx.on_app_quit(|v: &mut DataImportView, cx| {
                let wait = v.is_running();
                v.stop(cx);
                let timer = cx.background_executor().timer(Duration::from_millis(if wait { 400 } else { 0 }));
                async move { timer.await }
            }),
        };
        if let Some(t) = v.target.table.clone() {
            let task = v.target.conn.columns(&v.target.db, &v.target.schema, &t);
            cx.spawn(async move |this, cx| {
                let r = task.await;
                let _ = this.update(cx, |v, cx| {
                    match r {
                        Ok(cols) => v.target_cols = Some(cols),
                        Err(e) => v.error = Some(format!("{e:#}")),
                    }
                    v.apply_defaults();
                    cx.notify();
                });
            })
            .detach();
        }
        v.reload(cx);
        v
    }

    pub fn is_running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| matches!(j.state, JobState::Running))
    }

    fn new_table(&self) -> bool {
        self.target.table.is_none()
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.menu = None;
        let task = preview_task(self.target.conn.handle(), self.path.clone(), self.format, self.dialect.clone());
        self._preview = Some(cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |v, cx| {
                v.loading = false;
                match r {
                    Ok(p) => {
                        let same_cols = v.preview.as_ref().is_some_and(|old| old.columns.len() == p.columns.len());
                        v.preview = Some(p);
                        v.error = None;
                        if !same_cols {
                            v.map.clear();
                        }
                        v.apply_defaults();
                    }
                    Err(e) => {
                        v.preview = None;
                        v.error = Some(format!("{e:#}"));
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn apply_defaults(&mut self) {
        let Some(p) = &self.preview else { return };
        let n = p.columns.len();
        if self.types.len() != n {
            self.types = p.columns.iter().map(|c| c.ty).collect();
        }
        if self.map.len() == n {
            return;
        }
        self.map = if self.new_table() {
            vec![Some(0); n]
        } else {
            let Some(cols) = &self.target_cols else { return };
            let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
            auto_map(&p.columns, &names, p.dialect.header || !p.format.is_delimited())
        };
    }

    fn set_format(&mut self, f: ImportFormat, cx: &mut Context<Self>) {
        if self.job.is_some() {
            return;
        }
        self.format = Some(f);
        self.dialect = None;
        self.map.clear();
        self.types.clear();
        self.reload(cx);
    }

    fn set_field(&mut self, f: Field, ix: usize, cx: &mut Context<Self>) {
        let Some(p) = &self.preview else { return };
        let mut d = self.dialect.clone().unwrap_or_else(|| p.dialect.clone());
        f.set(&mut d, ix);
        self.dialect = Some(d);
        if f == Field::Header || f == Field::Delimiter || f == Field::Quote {
            self.map.clear();
            self.types.clear();
        }
        self.reload(cx);
    }

    fn choose_file(&mut self, cx: &mut Context<Self>) {
        if self.job.is_some() {
            return;
        }
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
        cx.spawn(async move |this, cx| {
            let Some(paths) = crate::sql_picker::picked(rx.await, cx) else { return };
            let Some(p) = paths.into_iter().next() else { return };
            let _ = this.update(cx, |v, cx| {
                if v.new_table() {
                    v.table_name.update(cx, |i, cx| i.set_text(table_name_for(&p), cx));
                }
                v.path = p;
                v.format = None;
                v.dialect = None;
                v.map.clear();
                v.types.clear();
                v.reload(cx);
            });
        })
        .detach();
    }

    fn options(&self, cx: &App) -> Result<ImportOptions, String> {
        let p = self.preview.as_ref().ok_or("the file is still being read")?;
        let columns: Vec<ColumnMap> = if self.new_table() {
            self.map
                .iter()
                .enumerate()
                .filter(|(_, m)| m.is_some())
                .map(|(i, _)| ColumnMap { source: i, target: p.columns[i].name.clone(), type_name: self.types.get(i).copied().unwrap_or(PgType::Text).sql().into() })
                .collect()
        } else {
            let cols = self.target_cols.as_ref().ok_or("the table's columns are still being read")?;
            self.map
                .iter()
                .enumerate()
                .filter_map(|(i, m)| m.map(|j| ColumnMap { source: i, target: cols[j].name.clone(), type_name: cols[j].type_name.clone() }))
                .collect()
        };
        let table = match &self.target.table {
            Some(t) => t.clone(),
            None => self.table_name.read(cx).text().trim().to_string(),
        };
        let o = ImportOptions {
            format: p.format,
            dialect: p.dialect.clone(),
            source_names: p.columns.iter().map(|c| c.name.clone()).collect(),
            schema: self.target.schema.clone(),
            table,
            create: self.new_table(),
            columns,
            truncate: self.truncate && !self.new_table(),
            on_error: if self.skip { OnError::Skip } else { OnError::Stop },
            one_transaction: self.one_tx,
            method: Method::Auto,
            insert_rows: 0,
        };
        o.check()?;
        Ok(o)
    }

    pub fn start(&mut self, cx: &mut Context<Self>) {
        if self.job.is_some() {
            return;
        }
        let o = match self.options(cx) {
            Ok(o) => o,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        if self.target.conn.read_only_for(&self.target.db) {
            self.error = Some("read-only database".into());
            cx.notify();
            return;
        }
        if self.guard.is_none() {
            let stmts = o.statements();
            let findings = guard::review(&stmts, self.target.conn.profile.env);
            if !findings.is_empty() {
                self.menu = None;
                self.guard = Some((findings, stmts));
                cx.notify();
                return;
            }
        }
        self.guard = None;
        self.run(o, cx);
    }

    fn run(&mut self, o: ImportOptions, cx: &mut Context<Self>) {
        let table = o.table.clone();
        let imp = self.target.conn.import(Some(&self.target.db), self.path.clone(), o);
        let progress = imp.progress.clone();
        let task = imp.task;
        let job_task = cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |v, cx| {
                let Some(job) = v.job.as_mut() else { return };
                job._tick = None;
                let report = r.unwrap_or_else(|e| failed_report(format!("{e:#}")));
                if !v.visible {
                    cx.emit(DataImportEvent::Finished(finished_line(&report, &job.table)));
                }
                job.state = JobState::Done(report);
                cx.notify();
            });
        });
        let tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        self.menu = None;
        self.error = None;
        self.job = Some(Job { progress, started: Instant::now(), state: JobState::Running, table, _task: Some(job_task), _tick: Some(tick) });
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(j) = &self.job
            && matches!(j.state, JobState::Running)
        {
            j.progress.cancel();
        }
        cx.notify();
    }

    fn back_to_options(&mut self, cx: &mut Context<Self>) {
        self.job = None;
        cx.notify();
    }

    pub fn status_line(&self) -> Option<String> {
        let j = self.job.as_ref()?;
        if !matches!(j.state, JobState::Running) {
            return None;
        }
        let pct = j.progress.fraction().map(|f| format!(" {}%", (f * 100.) as u32)).unwrap_or_default();
        Some(format!("importing{pct} · {} rows", ui_count(j.progress.rows() as usize)))
    }

    #[cfg(feature = "checks")]
    pub fn demo_progress(&mut self, cx: &mut Context<Self>) {
        let total = self.preview.as_ref().map(|p| p.file_bytes).unwrap_or(0).max(86 << 20);
        let progress = ImportProgress::fixed(412_300, total * 41 / 100, total);
        self.job = Some(Job {
            progress,
            started: Instant::now() - Duration::from_millis(1480),
            state: JobState::Running,
            table: self.target.table.clone().unwrap_or_default(),
            _task: None,
            _tick: None,
        });
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub fn demo_failed(&mut self, cx: &mut Context<Self>) {
        let mut report = failed_report(String::new());
        report.outcome = Outcome::Failed(dataman_core::import::ImportFailure {
            error: dataman_core::result::ErrorInfo {
                code: Some("22P02".into()),
                message: "invalid input syntax for type bigint: \"12a\"".into(),
                ..Default::default()
            },
            line: Some(1204),
            lines: None,
        });
        report.rolled_back = true;
        report.bytes = 96 << 10;
        report.elapsed = Duration::from_millis(310);
        self.job = Some(Job {
            progress: ImportProgress::fixed(0, 96 << 10, 400 << 10),
            started: Instant::now(),
            state: JobState::Done(report),
            table: self.target.table.clone().unwrap_or_default(),
            _task: None,
            _tick: None,
        });
        cx.notify();
    }

    #[cfg(feature = "checks")]
    pub fn demo_menu(&mut self, col: usize, cx: &mut Context<Self>) {
        self.menu = Some(Menu::Column(col));
        cx.notify();
    }

    /// The finished import: rows written, rows skipped, rolled back.
    #[cfg(feature = "checks")]
    pub fn demo_report(&self) -> Option<(u64, u64, bool)> {
        match &self.job.as_ref()?.state {
            JobState::Done(r) => Some((r.rows, r.skipped, r.rolled_back)),
            JobState::Running => None,
        }
    }

    #[cfg(feature = "checks")]
    pub fn demo_set_skip(&mut self, on: bool) {
        self.skip = on;
    }

    #[cfg(feature = "checks")]
    pub fn demo_set_truncate(&mut self, on: bool) {
        self.truncate = on;
    }

    #[cfg(feature = "checks")]
    pub fn demo_ready(&self) -> bool {
        self.preview.is_some() && !self.loading && (self.new_table() || self.target_cols.is_some())
    }

    fn render_progress(&self, j: &Job, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let p = &j.progress;
        let fmt = self.preview.as_ref().map(|p| p.format).unwrap_or(ImportFormat::Csv);
        let (pill, fg, bg): (&str, Hsla, Hsla) = match &j.state {
            JobState::Running => ("▶ IMPORTING", t.acc, t.acc_s),
            JobState::Done(r) => match &r.outcome {
                Outcome::Done => ("✓ DONE", t.ok, t.ok_s),
                Outcome::Cancelled => ("⊘ STOPPED", t.warn, t.warn_s),
                Outcome::Failed(_) => ("✕ FAILED", t.err, t.err_s),
            },
        };
        let elapsed = j.started.elapsed();
        let (rows, bytes) = match &j.state {
            JobState::Done(r) => (r.rows, r.bytes),
            JobState::Running => (p.rows(), p.bytes()),
        };
        let total_bytes = p.total_bytes();
        let right = match &j.state {
            JobState::Running => {
                let rate = p.read() as f64 / elapsed.as_secs_f64().max(1e-3);
                let mut s = format!("{} / {} · {} rows/s", fmt_bytes(bytes), fmt_bytes(total_bytes), ui_count(rate as usize));
                if let Some(f) = p.fraction().filter(|f| *f > 0.01 && *f < 1.) {
                    let left = elapsed.as_secs_f64() * (1. - f) / f;
                    s.push_str(&format!(" · {} left", fmt_secs(Duration::from_secs_f64(left))));
                }
                s
            }
            JobState::Done(r) => format!("{} in {}", fmt_bytes(r.bytes), fmt_secs(r.elapsed)),
        };
        let frac = match &j.state {
            JobState::Done(r) if r.outcome == Outcome::Done => 1.,
            _ => p.fraction().unwrap_or(0.),
        };
        let lit = (frac * 25.).floor() as usize;
        let color = match &j.state {
            JobState::Running => t.acc,
            JobState::Done(r) => match r.outcome {
                Outcome::Done => t.ok,
                Outcome::Cancelled => t.warn,
                Outcome::Failed(_) => t.err,
            },
        };
        let running = matches!(j.state, JobState::Running);
        let blocks = (0..25).map(|i| {
            let b = div().flex_1().h(px(12.));
            if i < lit {
                b.bg(color)
            } else if i == lit && running {
                b.bg(t.acc_s).border_1().border_color(t.acc)
            } else {
                b.bg(t.hov).border_1().border_color(t.line)
            }
        });
        let word = fmt.location_word();
        let (note, note_err): (SharedString, bool) = match &j.state {
            JobState::Running if p.skipped() > 0 => (format!("{} skipped", ui_count(p.skipped() as usize)).into(), false),
            JobState::Running => (SharedString::default(), false),
            JobState::Done(r) => {
                let skipped = if r.skipped > 0 {
                    let lines: Vec<String> = r.issues.iter().take(3).map(|i| ui_count(i.line as usize)).collect();
                    let more = if r.skipped as usize > lines.len() { ", …" } else { "" };
                    format!(" · {} skipped ({word} {}{more})", ui_count(r.skipped as usize), lines.join(", "))
                } else {
                    String::new()
                };
                match &r.outcome {
                    Outcome::Done => (format!("{}.{}{skipped}", self.target.schema, j.table).into(), false),
                    Outcome::Cancelled if r.rolled_back => ("rolled back".into(), false),
                    Outcome::Cancelled => (format!("{} rows kept", ui_count(r.rows as usize)).into(), false),
                    Outcome::Failed(f) => {
                        let at = match (f.line, f.lines) {
                            (Some(l), _) => format!("{word} {} · ", ui_count(l as usize)),
                            (None, Some((a, b))) => format!("{word}s {}–{} · ", ui_count(a as usize), ui_count(b as usize)),
                            _ => String::new(),
                        };
                        let code = f.error.code.as_ref().map(|c| format!("{c} ")).unwrap_or_default();
                        let kept = if r.rolled_back || r.rows == 0 { String::new() } else { format!(" · {} rows kept", ui_count(r.rows as usize)) };
                        (format!("{at}{code}{}{kept}", f.error.message).into(), true)
                    }
                }
            }
        };
        let total_txt = match (&j.state, p.total_rows()) {
            (JobState::Running, Some(tot)) => format!("/ {} rows", ui_count(tot as usize)),
            _ => "rows".into(),
        };
        let mut buttons: Vec<AnyElement> = Vec::new();
        match &j.state {
            JobState::Running => {
                buttons.push(
                    ui::button(t, "di-bg", Btn::Secondary).h(px(28.)).child("Run in background").on_click(cx.listener(|_, _, _, cx| cx.emit(DataImportEvent::Close))).into_any_element(),
                );
                buttons.push(ui::button(t, "di-stop", Btn::Danger).h(px(28.)).child("Stop").child(ui::key_hint("⌘.")).on_click(cx.listener(|v, _, _, cx| v.stop(cx))).into_any_element());
            }
            JobState::Done(r) if r.outcome == Outcome::Done => {
                let ev = DataImportEvent::ViewData {
                    conn: self.target.conn_id.clone(),
                    db: self.target.db.clone(),
                    schema: self.target.schema.clone(),
                    table: j.table.clone(),
                };
                let ev = std::cell::Cell::new(Some(ev));
                buttons.push(
                    ui::button(t, "di-view", Btn::Secondary)
                        .h(px(28.))
                        .child("View data")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            if let Some(e) = ev.take() {
                                cx.emit(e)
                            }
                        }))
                        .into_any_element(),
                );
                buttons.push(ui::button(t, "di-close", Btn::Primary).h(px(28.)).child("Close").child(ui::key_hint("esc")).on_click(cx.listener(|_, _, _, cx| cx.emit(DataImportEvent::Close))).into_any_element());
            }
            JobState::Done(_) => {
                buttons.push(ui::button(t, "di-back", Btn::Secondary).h(px(28.)).child("Back to options").on_click(cx.listener(|v, _, _, cx| v.back_to_options(cx))).into_any_element());
                buttons.push(ui::button(t, "di-close", Btn::Ghost).h(px(28.)).child("Close").child(ui::key_hint("esc")).on_click(cx.listener(|_, _, _, cx| cx.emit(DataImportEvent::Close))).into_any_element());
            }
        }
        div()
            .px(px(20.))
            .pt(px(14.))
            .pb(px(16.))
            .border_t_1()
            .border_color(t.line)
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(10.))
                    .child(ui::pill("", pill, fg, bg))
                    .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child(ui_count(rows as usize)))
                    .child(div().text_color(t.tx3).child(total_txt))
                    .child(div().flex_1())
                    .child(div().text_color(t.tx2).child(right)),
            )
            .child(div().flex().flex_row().gap(px(3.)).children(blocks))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(div().flex_1().min_w_0().text_size(px(11.)).line_height(px(16.)).text_color(if note_err { t.err } else { t.tx3 }).child(note))
                    .children(buttons),
            )
            .into_any_element()
    }

    fn render_grid(&self, p: &Preview, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let n = p.columns.len();
        let total_w = COL_W * n as f32;
        let offset_x = self.grid_scroll.offset().x;
        let locked = self.job.is_some();
        let new_table = self.new_table();
        let menu_open = self.menu;
        let heads = p.columns.iter().enumerate().map(|(i, c)| {
            let mapped = self.map.get(i).copied().flatten();
            let (label, on): (String, bool) = if new_table {
                match mapped {
                    Some(_) => (self.types.get(i).copied().unwrap_or(c.ty).sql().to_string(), true),
                    None => ("skip".into(), false),
                }
            } else {
                match (mapped, &self.target_cols) {
                    (Some(j), Some(cols)) => (format!("→ {}", cols[j].name), true),
                    _ => ("skip".into(), false),
                }
            };
            let open = menu_open == Some(Menu::Column(i));
            let hov = t.hov;
            div()
                .flex_none()
                .w(px(COL_W))
                .h(px(HEAD_H))
                .px(px(8.))
                .flex()
                .flex_col()
                .justify_center()
                .gap(px(2.))
                .border_r_1()
                .border_color(t.line)
                .child(div().text_color(if on { t.tx } else { t.tx3 }).font_weight(FontWeight::SEMIBOLD).whitespace_nowrap().overflow_hidden().text_ellipsis().child(c.name.clone()))
                .child(
                    div()
                        .id(("di-map", i))
                        .h(px(18.))
                        .mx(px(-4.))
                        .px(px(4.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.))
                        .text_size(px(11.))
                        .when(open, |d| d.bg(t.acc_s))
                        .when(!open && !locked, move |d| d.hover(move |s| s.bg(hov)))
                        .child(div().flex_1().min_w_0().whitespace_nowrap().overflow_hidden().text_ellipsis().text_color(if on { t.acc_t } else { t.tx4 }).child(label))
                        .child(ui::dropdown_arrow(("di-map-arrow", i), open, 9., t.tx4))
                        .when(!locked, |d| {
                            d.on_click(cx.listener(move |v, _, _, cx| {
                                if v.menu.is_none() && ui::menu_just_shut(&format!("import-{:?}", Menu::Column(i))) {
                                    return;
                                }
                                v.menu = if v.menu == Some(Menu::Column(i)) { None } else { Some(Menu::Column(i)) };
                                cx.notify();
                            }))
                        }),
                )
        });
        let header = div()
            .flex_none()
            .h(px(HEAD_H))
            .overflow_hidden()
            .relative()
            .border_b_1()
            .border_color(t.line)
            .bg(t.card)
            .child(div().absolute().top_0().left(offset_x).w(px(total_w)).flex().flex_row().children(heads));
        let skipped: Vec<bool> = (0..n).map(|i| self.map.get(i).copied().flatten().is_none()).collect();
        let rows = p.rows.iter().enumerate().map(|(r, row)| {
            div()
                .flex_none()
                .h(px(ROW_H))
                .w(px(total_w))
                .flex()
                .flex_row()
                .when(r % 2 == 1, |d| d.bg(t.hov.opacity(0.5)))
                .children(row.iter().enumerate().map(|(i, v)| {
                    let cell = div().flex_none().w(px(COL_W)).px(px(8.)).flex().items_center().whitespace_nowrap().overflow_hidden().text_ellipsis();
                    let cell = if skipped.get(i).copied().unwrap_or(false) { cell.opacity(0.4) } else { cell };
                    match v {
                        Some(s) => cell.text_color(t.tx2).child(one_line(s)),
                        None => cell.text_color(t.tx4).child("NULL"),
                    }
                }))
        });
        let body = div()
            .id("di-rows")
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            .track_scroll(&self.grid_scroll)
            .child(div().w(px(total_w)).flex().flex_col().children(rows));
        let menu = match self.menu {
            Some(Menu::Column(i)) if i < n => Some(self.render_column_menu(i, offset_x, t, cx)),
            _ => None,
        };
        div()
            .relative()
            .h(px(GRID_H))
            .flex()
            .flex_col()
            .border_1()
            .border_color(t.line)
            .bg(t.field)
            .overflow_hidden()
            .text_size(px(11.5))
            .child(header)
            .child(body)
            .children(menu)
            .into_any_element()
    }

    fn render_column_menu(&self, i: usize, offset_x: gpui::Pixels, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let current = self.map.get(i).copied().flatten();
        let item = |id: (&'static str, usize), label: String, right: Option<String>, checked: bool, t: &Theme| {
            let (acc, on_acc) = (t.acc, t.on_acc);
            div()
                .id(id)
                .h(px(24.))
                .px(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.))
                .text_color(t.tx)
                .hover(move |s| s.bg(acc).text_color(on_acc))
                .child(div().w(px(12.)).text_size(px(11.)).child(if checked { "✓" } else { "" }))
                .child(div().flex_1().min_w_0().whitespace_nowrap().overflow_hidden().text_ellipsis().child(label))
                .children(right.map(|r| div().text_size(px(11.)).opacity(0.6).child(r)))
        };
        let mut items: Vec<AnyElement> = Vec::new();
        if self.new_table() {
            let ty = self.types.get(i).copied().unwrap_or(PgType::Text);
            for (k, pt) in PgType::ALL.into_iter().enumerate() {
                items.push(
                    item(("di-type", k), pt.sql().to_string(), None, current.is_some() && pt == ty, t)
                        .on_click(cx.listener(move |v, _, _, cx| {
                            if let Some(slot) = v.types.get_mut(i) {
                                *slot = pt;
                            }
                            if let Some(m) = v.map.get_mut(i) {
                                *m = Some(0);
                            }
                            v.menu = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
            items.push(ui::menu_sep(t).into_any_element());
            items.push(
                item(("di-skip", 0), "skip".into(), None, current.is_none(), t)
                    .on_click(cx.listener(move |v, _, _, cx| {
                        if let Some(m) = v.map.get_mut(i) {
                            *m = None;
                        }
                        v.menu = None;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        } else if let Some(cols) = &self.target_cols {
            items.push(
                item(("di-skip", 0), "skip".into(), None, current.is_none(), t)
                    .on_click(cx.listener(move |v, _, _, cx| {
                        if let Some(m) = v.map.get_mut(i) {
                            *m = None;
                        }
                        v.menu = None;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            items.push(ui::menu_sep(t).into_any_element());
            for (j, c) in cols.iter().enumerate() {
                items.push(
                    item(("di-col", j), c.name.clone(), Some(c.type_name.clone()), current == Some(j), t)
                        .on_click(cx.listener(move |v, _, _, cx| {
                            for (k, m) in v.map.iter_mut().enumerate() {
                                if k != i && *m == Some(j) {
                                    *m = None;
                                }
                            }
                            if let Some(m) = v.map.get_mut(i) {
                                *m = Some(j);
                            }
                            v.menu = None;
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
        }
        let left = offset_x + px(COL_W * i as f32);
        let left = if left < px(0.) { px(0.) } else { left };
        div().absolute().top_0().left_0().child(deferred(
            anchored().child(ui::menu_in("di-col-menu-in", 
                div().pl(left).pt(px(HEAD_H - 2.)).child(
                    ui::popover(t)
                        .id("di-col-menu")
                        .w(px(220.))
                        .max_h(px(300.))
                        .overflow_y_scroll()
                        .on_mouse_down_out(cx.listener(|v, _, _, cx| {
                            if let Some(m) = v.menu.take() {
                                ui::menu_shut(format!("import-{m:?}"));
                            }
                            cx.notify();
                        }))
                        .children(items),
                ),
            )),
        )
        .priority(3))
        .into_any_element()
    }

    fn render_guard(&self, findings: &[Finding], stmts: &[String], t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let n = findings.len();
        let title: SharedString = match findings.first() {
            Some(f) if n == 1 && matches!(f.rule, "write" | "ddl") => format!("{} on production", f.label).into(),
            Some(f) if n == 1 => f.label.clone().into(),
            _ => format!("Confirm {n} statements").into(),
        };
        let body = findings.iter().take(4).map(|f| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .when(n > 1, |d| d.child(div().text_color(t.err).font_weight(FontWeight::SEMIBOLD).child(f.label.clone())))
                .child(crate::chrome::code_block(t, stmts.get(f.index).map(String::as_str).unwrap_or(""), f.target.as_deref()))
        });
        let p = &self.target.conn.profile;
        let env = p.env;
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .min_w_0()
            .text_size(px(11.))
            .child(ui::chip(t.profile_color(p), 8.))
            .child(div().text_color(t.tx2).overflow_hidden().whitespace_nowrap().text_ellipsis().child(p.name.clone()))
            .when(!env.is_none(), |d| d.child(div().text_color(if env.is_production() { t.err } else { t.tx3 }).child(env.label())));
        div()
            .absolute()
            .inset_0()
            .bg(t.scrim)
            .flex()
            .items_center()
            .justify_center()
            .child(
                ui::modal(t, "di-guard")
                    .w(px(520.))
                    .child(ui::modal_header(
                        t,
                        None,
                        title,
                        ui::modal_close(t, "di-guard-close").on_click(cx.listener(|v, _, _, cx| {
                            v.guard = None;
                            cx.notify();
                        })),
                    ))
                    .child(div().px(px(20.)).py(px(16.)).flex().flex_col().gap(px(10.)).children(body))
                    .child(
                        ui::modal_footer(t)
                            .child(footer)
                            .child(div().flex_1())
                            .child(
                                ui::button(t, "di-guard-cancel", Btn::Secondary)
                                    .h(px(28.))
                                    .child("Cancel")
                                    .child(ui::key_hint("esc"))
                                    .on_click(cx.listener(|v, _, _, cx| {
                                        v.guard = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                ui::button(t, "di-guard-run", Btn::Danger)
                                    .h(px(28.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Run")
                                    .child(ui::key_hint("⌘↩"))
                                    .on_click(cx.listener(|v, _, _, cx| v.start(cx))),
                            ),
                    ),
            )
            .into_any_element()
    }
}

fn one_line(s: &str) -> String {
    let s: String = s.chars().take(200).collect();
    s.replace(['\n', '\r'], " ↵ ").replace('\t', " ")
}

fn failed_report(message: String) -> ImportReport {
    ImportReport {
        outcome: Outcome::Failed(dataman_core::import::ImportFailure {
            error: dataman_core::result::ErrorInfo { message, ..Default::default() },
            line: None,
            lines: None,
        }),
        rows: 0,
        skipped: 0,
        issues: Vec::new(),
        method: Method::Auto,
        elapsed: Duration::ZERO,
        bytes: 0,
        rolled_back: false,
    }
}

pub fn finished_line(r: &ImportReport, table: &str) -> String {
    match &r.outcome {
        Outcome::Done if r.skipped > 0 => format!("Imported {} rows into {table} · {} skipped", ui_count(r.rows as usize), ui_count(r.skipped as usize)),
        Outcome::Done => format!("Imported {} rows into {table}", ui_count(r.rows as usize)),
        Outcome::Cancelled if r.rolled_back => "Import stopped · rolled back".to_string(),
        Outcome::Cancelled => format!("Import stopped · {} rows kept", ui_count(r.rows as usize)),
        Outcome::Failed(f) => match f.line {
            Some(l) => format!("Import failed at line {}: {}", ui_count(l as usize), f.error.message),
            None => format!("Import failed: {}", f.error.message),
        },
    }
}

impl Render for DataImportView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let locked = self.job.is_some();
        let sheet = ui::modal(&t, "di-sheet").w(px(780.)).overflow_hidden();
        let conn = &self.target.conn.profile;
        let target_label = match &self.target.table {
            Some(tb) => format!("{}.{tb}", self.target.schema),
            None => format!("{} · new table", self.target.schema),
        };
        let header = ui::modal_header_with(
            &t,
            Some(icon(Icon::Import).size(px(16.)).text_color(t.tx2).into_any_element()),
            "Import data",
            Some(
                div()
                    .flex_none()
                    .max_w(px(320.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(ui::chip(t.profile_color(conn), 8.))
                    .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx2).child(target_label))
                    .into_any_element(),
            ),
            ui::modal_close(&t, "di-close").on_click(cx.listener(|_, _, _, cx| cx.emit(DataImportEvent::Close))),
        );

        let detected = self.preview.as_ref().map(|p| p.format);
        let shown_format = self.format.or(detected);
        let file = div().flex_1().min_w_0().flex().flex_col().gap(px(6.)).child(ui::caps(&t, "FILE")).child(
            div()
                .h(px(28.))
                .px(px(10.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .bg(t.field)
                .border_1()
                .border_color(t.field_b)
                .child(div().flex_none().max_w(px(260.)).overflow_hidden().whitespace_nowrap().text_ellipsis().child(
                    self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                ))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(t.tx3).child(
                    self.path.parent().map(tilde).unwrap_or_default(),
                ))
                .children(self.preview.as_ref().map(|p| div().flex_none().text_color(t.tx3).child(fmt_bytes(p.file_bytes))))
                .child(div().id("di-choose").flex_none().text_color(t.acc_t).child("Choose…").when(!locked, |d| d.on_click(cx.listener(|v, _, _, cx| v.choose_file(cx))))),
        );
        let formats = div().flex_none().flex().flex_col().gap(px(6.)).child(ui::caps(&t, "FORMAT")).child(
            ui::segmented(&t).w(px(330.)).children(ImportFormat::ALL.into_iter().map(|f| {
                ui::segment(&t, f.label(), f.label(), shown_format == Some(f))
                    .when(locked, |d| d.opacity(0.6))
                    .when(!locked, |d| d.on_click(cx.listener(move |v, _, _, cx| v.set_format(f, cx))))
            })),
        );
        let top = div().flex().flex_row().gap(px(16.)).child(file).child(formats);

        let menu_open = self.menu;
        let dialect = self.preview.as_ref().filter(|p| p.format.is_delimited()).map(|p| {
            let d = self.dialect.clone().unwrap_or_else(|| p.dialect.clone());
            div().flex().flex_row().gap(px(10.)).children(Field::ALL.into_iter().map(|f| {
                let ix = f.get(&d);
                let open = menu_open == Some(Menu::Field(f));
                let choices = f.choices();
                let menu = open.then(|| {
                    deferred(
                        anchored().child(ui::menu_in("di-field-menu-in", 
                            ui::popover(&t)
                                .mt(px(52.))
                                .w(px(180.))
                                .on_mouse_down_out(cx.listener(|v, _, _, cx| {
                                    if let Some(m) = v.menu.take() {
                                        ui::menu_shut(format!("import-{m:?}"));
                                    }
                                    cx.notify();
                                }))
                                .children(choices.iter().enumerate().map(|(i, c)| {
                                    ui::menu_item(&t, ("di-opt", i), *c, if i == ix { Some("✓") } else { None }, true)
                                        .on_click(cx.listener(move |v, _, _, cx| v.set_field(f, i, cx)))
                                })),
                        )),
                    )
                    .priority(3)
                });
                div()
                    .flex_1()
                    .min_w_0()
                    .relative()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(ui::caps(&t, f.label()))
                    .child(
                        div()
                            .id(f.label())
                            .h(px(28.))
                            .px(px(10.))
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .bg(t.field)
                            .border_1()
                            .border_color(if open { t.acc } else { t.field_b })
                            .text_color(t.tx)
                            .child(div().min_w_0().whitespace_nowrap().overflow_hidden().text_ellipsis().child(choices.get(ix).copied().unwrap_or("")))
                            .child(ui::dropdown_arrow(gpui::ElementId::Name(format!("{}-arrow", f.label()).into()), open, 9., t.tx3))
                            .when(!locked, |d| {
                                d.on_click(cx.listener(move |v, _, _, cx| {
                                    if v.menu.is_none() && ui::menu_just_shut(&format!("import-{:?}", Menu::Field(f))) {
                                        return;
                                    }
                                    v.menu = if v.menu == Some(Menu::Field(f)) { None } else { Some(Menu::Field(f)) };
                                    cx.notify();
                                }))
                            }),
                    )
                    .children(menu)
            }))
        });

        let table = self.new_table().then(|| {
            div()
                .flex()
                .flex_row()
                .gap(px(16.))
                .child(
                    div().w(px(180.)).flex().flex_col().gap(px(6.)).child(ui::caps(&t, "SCHEMA")).child(
                        div().h(px(28.)).px(px(10.)).flex().items_center().bg(t.field).border_1().border_color(t.field_b).text_color(t.tx2).child(self.target.schema.clone()),
                    ),
                )
                .child(div().flex_1().flex().flex_col().gap(px(6.)).child(ui::caps(&t, "TABLE")).child(div().h(px(30.)).child(self.table_name.clone())))
        });

        let preview: AnyElement = match &self.preview {
            Some(p) => {
                let count = match (p.total_rows, p.rows_exact) {
                    (Some(n), true) => format!("{} rows", ui_count(n as usize)),
                    (Some(n), false) => format!("~{} rows", ui_count(n as usize)),
                    _ => String::new(),
                };
                let imported = self.map.iter().filter(|m| m.is_some()).count();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.))
                            .child(ui::caps(&t, "PREVIEW"))
                            .child(div().flex_1())
                            .child(div().text_size(px(11.)).text_color(t.tx3).child(format!("{imported} of {} columns", p.columns.len())))
                            .child(div().text_size(px(11.)).text_color(t.tx3).child(count)),
                    )
                    .child(self.render_grid(p, &t, cx))
                    .into_any_element()
            }
            None => div()
                .h(px(GRID_H + 20.))
                .flex()
                .items_center()
                .justify_center()
                .border_1()
                .border_color(t.line)
                .bg(t.field)
                .child(match &self.error {
                    Some(e) => div().px(px(20.)).text_color(t.err).child(e.clone()),
                    None => ui::pixel(if self.loading { "[ READING ]" } else { "[ NO FILE ]" }, 18., t.tx4),
                })
                .into_any_element(),
        };

        let check = |id: &'static str, label: &'static str, on: bool, danger: bool, enabled: bool, t: &Theme| {
            div()
                .id(id)
                .h(px(26.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(ui::checkbox(t, on))
                .child(div().text_color(if !enabled { t.tx4 } else if danger && on { t.err } else { t.tx }).child(label))
        };
        let can_truncate = !self.new_table() && !locked;
        let options = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(20.))
            .when(!self.new_table(), |d| {
                d.child(check("di-truncate", "Truncate first", self.truncate, true, can_truncate, &t).when(can_truncate, |d| {
                    d.on_click(cx.listener(|v, _, _, cx| {
                        v.truncate = !v.truncate;
                        cx.notify();
                    }))
                }))
            })
            .child(check("di-tx", "One transaction", self.one_tx, false, !locked, &t).when(!locked, |d| {
                d.on_click(cx.listener(|v, _, _, cx| {
                    v.one_tx = !v.one_tx;
                    cx.notify();
                }))
            }))
            .child(div().flex_1())
            .child(div().text_size(px(10.5)).text_color(t.tx3).child("ON ERROR"))
            .child(ui::segmented(&t).w(px(190.)).children([("Stop", false), ("Skip rows", true)].into_iter().map(|(label, skip)| {
                ui::segment(&t, label, label, self.skip == skip).when(!locked, |d| {
                    d.on_click(cx.listener(move |v, _, _, cx| {
                        v.skip = skip;
                        cx.notify();
                    }))
                })
            })));

        let body = div()
            .id("di-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(20.))
            .py(px(16.))
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(top)
            .children(dialect)
            .children(table)
            .child(preview)
            .child(options);
        let ready = self.preview.is_some() && !self.loading && (self.new_table() || self.target_cols.is_some());
        let footer: AnyElement = match &self.job {
            Some(j) => self.render_progress(j, &t, cx),
            None => ui::modal_footer(&t)
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(11.)).text_color(t.err).children(self.error.clone().filter(|_| self.preview.is_some())))
                .child(ui::button(&t, "di-cancel", Btn::Ghost).h(px(28.)).child("Cancel").on_click(cx.listener(|_, _, _, cx| cx.emit(DataImportEvent::Close))))
                .child(
                    ui::button(&t, "di-start", if ready { Btn::Primary } else { Btn::Ghost })
                        .h(px(28.))
                        .px(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Import")
                        .child(div().text_size(px(10.5)).font_weight(FontWeight::NORMAL).opacity(0.75).child(crate::ui::keys_text("⌘↩")))
                        .when(ready, |d| d.on_click(cx.listener(|v, _, _, cx| v.start(cx)))),
                )
                .into_any_element(),
        };
        let guard = self.guard.as_ref().map(|(f, s)| self.render_guard(f, s, &t, cx));
        div()
            .id("di-scrim")
            .absolute()
            .top(px(metrics::TITLE_H))
            .left_0()
            .right_0()
            .bottom_0()
            .bg(t.scrim)
            .key_context("DataImport Modal")
            .track_focus(&self.focus)
            .on_action(cx.listener(|v, _: &CloseDataImport, _, cx| {
                if v.guard.take().is_some() || v.menu.take().is_some() {
                    cx.notify();
                } else {
                    cx.emit(DataImportEvent::Close)
                }
            }))
            .on_action(cx.listener(|v, _: &StartDataImport, _, cx| v.start(cx)))
            .on_action(cx.listener(|v, _: &StopDataImport, _, cx| v.stop(cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(|v, _, _, cx| {
                if v.guard.take().is_some() {
                    cx.notify();
                } else {
                    cx.emit(DataImportEvent::Close)
                }
            }))
            .child(div().absolute().top(px(40.)).left_0().right_0().bottom(px(12.)).flex().items_start().justify_center().child(sheet.child(header).child(body).child(footer)))
            .children(guard)
    }
}

use crate::app::Workspace;
use crate::tree_menu::Target;

impl Workspace {
    pub(crate) fn import_target(&self, cx: &App) -> Result<DataTarget, &'static str> {
        let store = self.store.read(cx);
        let from_tree = self.tree.read(cx).selected_target(cx);
        let (conn_id, db, schema, table) = match from_tree {
            Some(Target::Rel { conn, db, schema, name, kind, .. }) if !kind.is_view() => (conn, db, schema, Some(name)),
            Some(Target::Rel { conn, db, schema, .. }) | Some(Target::Schema { conn, db, schema, .. }) => (conn, db, schema, None),
            _ => {
                let tab = self.active_tab().ok_or("Select a table or a schema in the explorer first")?.read(cx);
                let c = tab.connection().ok_or("Select a table or a schema in the explorer first")?;
                let schema = c.info.search_path.iter().find(|s| !s.starts_with("pg_") && *s != "$user").cloned().unwrap_or_else(|| "public".into());
                (c.profile.id.clone(), tab.database().to_string(), schema, None)
            }
        };
        let conn = store.connection(&conn_id).ok_or("Connect first: the import writes to that server")?;
        Ok(DataTarget { conn_id, conn, db, schema, table })
    }

    pub(crate) fn import_data(&mut self, target: Option<DataTarget>, path: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(v) = self.data_import.clone()
            && v.read(cx).is_running()
        {
            v.update(cx, |v, cx| {
                v.visible = true;
                cx.notify();
            });
            window.focus(&v.focus_handle(cx), cx);
            cx.notify();
            return;
        }
        if self.modal_open(cx) {
            return;
        }
        let target = match target.map(Ok).unwrap_or_else(|| self.import_target(cx)) {
            Ok(t) => t,
            Err(e) => {
                self.notice(e, cx);
                return;
            }
        };
        if !target.conn.caps().import {
            return;
        }
        if target.conn.read_only_for(&target.db) {
            self.notice("Read-only database: import is refused", cx);
            return;
        }
        match path {
            Some(p) => self.show_data_import(target, p, window, cx),
            None => {
                let rx = cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
                cx.spawn_in(window, async move |this, cx| {
                    let Some(paths) = crate::sql_picker::picked(rx.await, cx) else { return };
                    let Some(p) = paths.into_iter().next() else { return };
                    let _ = this.update_in(cx, |w, window, cx| w.show_data_import(target, p, window, cx));
                })
                .detach();
            }
        }
    }

    pub(crate) fn drop_files(&mut self, paths: &gpui::ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        // A database file opens as a connection; a .sql file in a tab.
        if let Some(db) = paths.paths().iter().find(|p| p.is_file() && dataman_core::import::detect_database(p).is_some()).cloned() {
            self.open_database_path(&db, window, cx);
            return;
        }
        let sql: Vec<PathBuf> = paths.paths().iter().filter(|p| crate::sql_picker::is_sql(p)).cloned().collect();
        if !sql.is_empty() {
            self.open_sql_paths(&sql, window, cx);
            return;
        }
        let Some(p) = paths.paths().iter().find(|p| ImportFormat::is_importable(p)).cloned() else {
            if !paths.paths().is_empty() {
                self.notice("Drop a CSV, TSV, JSON, NDJSON or Parquet file to import it", cx);
            }
            return;
        };
        self.import_data(None, Some(p), window, cx);
    }

    fn show_data_import(&mut self, target: DataTarget, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open(cx) {
            return;
        }
        self.modal_return = window.focused(cx);
        let v = cx.new(|cx| DataImportView::new(target, path, window, cx));
        cx.subscribe_in(&v, window, |w, v, ev: &DataImportEvent, window, cx| match ev {
            DataImportEvent::Close => {
                v.update(cx, |v, cx| {
                    v.visible = false;
                    cx.notify();
                });
                if !v.read(cx).is_running() {
                    w.data_import = None;
                }
                match w.modal_return.take() {
                    Some(f) => window.focus(&f, cx),
                    None => w.focus_editor(window, cx),
                }
                cx.notify();
            }
            DataImportEvent::Finished(line) => {
                w.data_import = None;
                w.notice(line.clone(), cx);
            }
            DataImportEvent::ViewData { conn, db, schema, table } => {
                w.data_import = None;
                w.modal_return = None;
                let (conn, db, schema, table) = (conn.clone(), db.clone(), schema.clone(), table.clone());
                if let Some(c) = w.store.read(cx).connection(&conn) {
                    c.schema.invalidate_schema(&db, &schema);
                }
                w.open_relation(&conn, &db, &schema, &table, window, cx);
            }
        })
        .detach();
        cx.observe(&v, |_, _, cx| cx.notify()).detach();
        self.data_import = Some(v);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialect_fields_round_trip() {
        let mut d = CsvDialect::default();
        for f in Field::ALL {
            for i in 0..f.choices().len() {
                f.set(&mut d, i);
                assert_eq!(f.get(&d), i, "{f:?} {i}");
            }
        }
        let tsv = CsvDialect::tsv();
        assert_eq!(Field::Quote.get(&tsv), 2);
        assert_eq!(Field::Null.get(&tsv), 2);
        assert_eq!(Field::Delimiter.get(&tsv), 2);
    }

    #[test]
    fn status_lines() {
        let mut r = failed_report("boom".into());
        assert_eq!(finished_line(&r, "t"), "Import failed: boom");
        r.outcome = Outcome::Done;
        r.rows = 1200;
        assert_eq!(finished_line(&r, "orders"), "Imported 1,200 rows into orders");
        r.skipped = 3;
        assert!(finished_line(&r, "orders").ends_with("3 skipped"));
        r.outcome = Outcome::Cancelled;
        r.rolled_back = true;
        assert_eq!(finished_line(&r, "orders"), "Import stopped · rolled back");
        assert_eq!(one_line("a\nb\tc"), "a ↵ b c");
    }
}
