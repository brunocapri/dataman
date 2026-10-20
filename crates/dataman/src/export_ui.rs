use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, KeyBinding, MouseButton, Task,
    Window, actions, anchored, deferred, div, prelude::*,
};
use crate::theme::px;
use dataman_core::export::{ExportFormat, ExportOptions, ExportProgress, ExportSummary, LineEnding, QuoteStyle, export_result, is_rerunnable};
use dataman_core::result::{Phase, ResultSet};
use dataman_core::{Connection, Core};

use crate::assets::{Icon, icon};
use crate::grid::ui_count;
use crate::input::{InputEvent, TextInput};
use crate::theme::{ActiveTheme, Theme, metrics};
use crate::ui::{self, Btn};

actions!(export_dialog, [CloseExport, StartExport, StopExport]);

const CONTEXT: &str = "ExportDialog";

pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("escape", CloseExport, c),
        KeyBinding::new("cmd-enter", StartExport, c),
        KeyBinding::new("cmd-.", StopExport, c),
    ]);
}

pub enum ExportEvent {
    Close,
    Run,
    Finished(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowScope {
    All,
    Fetched,
    Selection,
}

#[derive(Clone)]
pub struct Source {
    pub rs: Option<Arc<ResultSet>>,
    pub sql: String,
    pub columns: Option<usize>,
    pub conn: Arc<Connection>,
    pub db: String,
    pub selection: Option<(Vec<usize>, Vec<usize>)>,
}

#[derive(Clone, Debug)]
enum JobState {
    Running,
    Done(ExportSummary),
    Failed(String),
}

struct Job {
    progress: Arc<ExportProgress>,
    state: JobState,
    _task: Task<()>,
    _tick: Option<Task<()>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Header,
    Delimiter,
    Quote,
    Null,
    Eol,
    Batch,
    RowGroup,
}

impl Field {
    fn label(self) -> &'static str {
        match self {
            Field::Header => "HEADER",
            Field::Delimiter => "DELIMITER",
            Field::Quote => "QUOTING",
            Field::Null => "NULL AS",
            Field::Eol => "LINE ENDING",
            Field::Batch => "ROWS PER INSERT",
            Field::RowGroup => "ROW GROUP",
        }
    }

    fn choices(self, format: ExportFormat) -> &'static [&'static str] {
        match self {
            Field::Header => &["first line", "none"],
            Field::Delimiter => &[", comma", "; semicolon", "| pipe", "⇥ tab"],
            Field::Quote => &["when needed", "always", "non-numeric", "never"],
            Field::Null if format == ExportFormat::Tsv => &["\\N", "empty", "NULL"],
            Field::Null => &["empty", "NULL", "\\N"],
            Field::Eol => &["LF (unix)", "CRLF (windows)"],
            Field::Batch => &["100", "500", "1,000", "5,000"],
            Field::RowGroup => &["16k rows", "64k rows", "128k rows", "1M rows"],
        }
    }
}

pub struct ExportView {
    core: Arc<Core>,
    focus: FocusHandle,
    source: Option<Source>,
    format: ExportFormat,
    opts: ExportOptions,
    scope: RowScope,
    path: PathBuf,
    path_chosen: bool,
    menu: Option<Field>,
    table: gpui::Entity<TextInput>,
    job: Option<Job>,
    advanced: bool,
    close_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    pub visible: bool,
    _quit: gpui::Subscription,
}

impl EventEmitter<ExportEvent> for ExportView {}

impl Focusable for ExportView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

pub fn base_name(sql: &str) -> String {
    let toks: Vec<String> = sql.split(|c: char| c.is_whitespace() || c == ';' || c == ',').filter(|s| !s.is_empty()).map(str::to_string).collect();
    let name = toks
        .iter()
        .position(|t| t.eq_ignore_ascii_case("from"))
        .and_then(|i| toks.get(i + 1))
        .map(|t| t.rsplit('.').next().unwrap_or(t).trim_matches('"').to_string())
        .filter(|t| !t.is_empty() && !t.starts_with('('));
    let raw = name.unwrap_or_else(|| "result".into());
    let clean: String = raw.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if clean.is_empty() { "result".into() } else { clean }
}

pub fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let p = dir.join(format!("{stem}.{ext}"));
    if !p.exists() {
        return p;
    }
    (2..1000).map(|i| dir.join(format!("{stem} ({i}).{ext}"))).find(|p| !p.exists()).unwrap_or(p)
}

pub fn strip_copy_suffix(stem: &str) -> &str {
    if let Some(open) = stem.rfind(" (")
        && let Some(n) = stem[open + 2..].strip_suffix(')')
        && !n.is_empty()
        && n.chars().all(|c| c.is_ascii_digit())
    {
        return &stem[..open];
    }
    stem
}

pub fn with_format(chosen: &Path, ext: &str) -> (PathBuf, bool) {
    let p = chosen.with_extension(ext);
    if p == chosen || !p.exists() {
        return (p, false);
    }
    let dir = chosen.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = chosen.file_stem().and_then(|s| s.to_str()).unwrap_or("result");
    (unique_path(&dir, strip_copy_suffix(stem), ext), true)
}

fn finished_line(state: &JobState) -> String {
    match state {
        JobState::Done(s) if s.cancelled => "Export stopped · nothing written".to_string(),
        JobState::Done(s) => {
            let name = s.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let rows = ui_count(s.rows as usize);
            if s.complete {
                format!("Exported {rows} rows to {name} · {}", fmt_bytes(s.bytes))
            } else {
                format!("Exported only {rows} rows to {name} (the result had stopped early: not every row) · {}", fmt_bytes(s.bytes))
            }
        }
        JobState::Failed(e) => format!("Export failed: {e}"),
        JobState::Running => String::new(),
    }
}

pub fn fmt_bytes(b: u64) -> String {
    let b = b as f64;
    if b < 1024. {
        format!("{b:.0} B")
    } else if b < 1024. * 1024. {
        format!("{:.1} KB", b / 1024.)
    } else if b < 1024. * 1024. * 1024. {
        format!("{:.1} MB", b / 1024. / 1024.)
    } else {
        format!("{:.2} GB", b / 1024. / 1024. / 1024.)
    }
}


pub(crate) fn fmt_secs(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 60. { format!("{s:.1} s") } else { format!("{}m {:02}s", (s / 60.) as u64, (s % 60.) as u64) }
}

impl ExportView {
    pub fn new(core: Arc<Core>, source: Option<Source>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let name = source.as_ref().map(|s| base_name(&s.sql)).unwrap_or_else(|| "result".into());
        let table = cx.new(|cx| {
            let mut i = TextInput::new(cx, "table");
            i.set_text(name.clone(), cx);
            i
        });
        cx.subscribe(&table, |v: &mut ExportView, _, ev: &InputEvent, cx| {
            if let InputEvent::Submit = ev {
                v.start(cx);
            }
        })
        .detach();
        let format = ExportFormat::Csv;
        let mut opts = ExportOptions::new(format);
        opts.dialect = source.as_ref().map(|s| s.conn.dialect()).unwrap_or_default();
        let scope = match &source {
            Some(s) if s.selection.as_ref().is_some_and(|(r, c)| r.len() * c.len() > 1) => RowScope::Selection,
            _ => RowScope::All,
        };
        let mut v = ExportView {
            core,
            focus,
            source,
            format,
            opts,
            scope,
            path: PathBuf::new(),
            path_chosen: false,
            menu: None,
            table,
            job: None,
            advanced: false,
            close_bounds: Default::default(),
            visible: true,
            // Quitting mid-export stops it, so the writer removes its temporary file.
            _quit: cx.on_app_quit(|v: &mut ExportView, cx| {
                let wait = v.is_running();
                v.stop(cx);
                let timer = cx.background_executor().timer(Duration::from_millis(if wait { 400 } else { 0 }));
                async move { timer.await }
            }),
        };
        v.path = v.default_path(&name);
        v
    }

    fn export_dir(&self) -> PathBuf {
        match self.core.settings.get().export_dir {
            Some(d) => PathBuf::from(d),
            None => std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads")).unwrap_or_else(std::env::temp_dir),
        }
    }

    fn default_path(&self, stem: &str) -> PathBuf {
        unique_path(&self.export_dir(), stem, self.format.extension())
    }

    pub fn is_running(&self) -> bool {
        self.job.as_ref().is_some_and(|j| matches!(j.state, JobState::Running))
    }

    fn set_format(&mut self, f: ExportFormat, cx: &mut Context<Self>) {
        self.format = f;
        self.opts.format = f;
        self.opts.null_text = None;
        self.menu = None;
        if !self.path_chosen {
            let stem = self.path.file_stem().and_then(|s| s.to_str()).map(|s| strip_copy_suffix(s).to_string()).unwrap_or_else(|| "result".into());
            self.path = self.default_path(&stem);
        } else {
            self.path = with_format(&self.path, f.extension()).0;
        }
        cx.notify();
    }

    fn field_value(&self, f: Field) -> usize {
        let o = &self.opts;
        match f {
            Field::Header => usize::from(!o.header),
            Field::Delimiter => match o.delimiter {
                ';' => 1,
                '|' => 2,
                '\t' => 3,
                _ => 0,
            },
            Field::Quote => match o.quote {
                QuoteStyle::Necessary => 0,
                QuoteStyle::Always => 1,
                QuoteStyle::NonNumeric => 2,
                QuoteStyle::Never => 3,
            },
            Field::Null => {
                let choices = f.choices(self.format);
                match o.null_text.as_deref() {
                    None => 0,
                    Some(t) => choices.iter().position(|c| *c == t || (*c == "empty" && t.is_empty())).unwrap_or(0),
                }
            }
            Field::Eol => usize::from(o.line_ending == LineEnding::CrLf),
            Field::Batch => [100, 500, 1000, 5000].iter().position(|b| *b == o.batch_size).unwrap_or(1),
            Field::RowGroup => [16 * 1024, 64 * 1024, 128 * 1024, 1024 * 1024].iter().position(|b| *b == o.row_group_rows).unwrap_or(1),
        }
    }

    fn set_field(&mut self, f: Field, ix: usize, cx: &mut Context<Self>) {
        let o = &mut self.opts;
        match f {
            Field::Header => o.header = ix == 0,
            Field::Delimiter => o.delimiter = [',', ';', '|', '\t'][ix.min(3)],
            Field::Quote => o.quote = [QuoteStyle::Necessary, QuoteStyle::Always, QuoteStyle::NonNumeric, QuoteStyle::Never][ix.min(3)],
            Field::Null => {
                o.null_text = if ix == 0 {
                    None
                } else {
                    Some(match f.choices(self.format)[ix] {
                        "empty" => String::new(),
                        other => other.to_string(),
                    })
                }
            }
            Field::Eol => o.line_ending = if ix == 0 { LineEnding::Lf } else { LineEnding::CrLf },
            Field::Batch => o.batch_size = [100, 500, 1000, 5000][ix.min(3)],
            Field::RowGroup => o.row_group_rows = [16 * 1024, 64 * 1024, 128 * 1024, 1024 * 1024][ix.min(3)],
        }
        self.menu = None;
        cx.notify();
    }

    fn fields(&self) -> Vec<Field> {
        match self.format {
            ExportFormat::Csv => vec![Field::Header, Field::Delimiter, Field::Quote, Field::Null, Field::Eol],
            ExportFormat::Tsv => vec![Field::Header, Field::Null, Field::Eol],
            ExportFormat::SqlInsert => vec![Field::Batch],
            ExportFormat::Parquet => vec![Field::RowGroup],
            ExportFormat::Json | ExportFormat::Ndjson => vec![],
        }
    }

    fn choose_path(&mut self, cx: &mut Context<Self>) {
        let dir = self.path.parent().map(Path::to_path_buf).unwrap_or_else(|| self.export_dir());
        let name = self.path.file_name().and_then(|n| n.to_str()).map(str::to_string);
        let rx = cx.prompt_for_new_path(&dir, name.as_deref());
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(p))) = rx.await else { return };
            let _ = this.update(cx, |v, cx| {
                if let Some(f) = ExportFormat::from_path(&p)
                    && f != v.format
                {
                    v.format = f;
                    v.opts.format = f;
                    v.opts.null_text = None;
                }
                v.path = p;
                v.path_chosen = true;
                cx.notify();
            });
        })
        .detach();
    }

    fn facts(&self) -> Option<(usize, bool, Option<(usize, usize)>)> {
        let s = self.source.as_ref()?;
        let sel = s.selection.as_ref().map(|(r, c)| (r.len(), c.len()));
        Some(match &s.rs {
            Some(rs) => (rs.rows(), rs.phase() == Phase::Paused, sel),
            None => (0, true, None),
        })
    }

    pub fn start(&mut self, cx: &mut Context<Self>) {
        if self.is_running() {
            return;
        }
        let Some(src) = self.source.clone() else { return };
        // Another export from the same sheet (another format, the same one again): never over
        // the file just written unless the user picked that name.
        if self.job.take().is_some() && !self.path_chosen {
            let stem = self.path.file_stem().and_then(|s| s.to_str()).map(|s| strip_copy_suffix(s).to_string()).unwrap_or_else(|| "result".into());
            self.path = self.default_path(&stem);
        }
        let mut opts = self.opts.clone();
        opts.format = self.format;
        opts.table = {
            let t = self.table.read(cx).text().trim().to_string();
            if t.is_empty() { base_name(&src.sql) } else { t }
        };
        let Some(rs) = src.rs.clone() else {
            let export = src.conn.export_query(Some(&src.db), src.sql.clone(), self.path.clone(), opts, None);
            self.track(export, cx);
            return;
        };
        let paused = rs.phase() == Phase::Paused;
        let export = match self.scope {
            // A sorted grid is written from the result, in the order it shows: streaming re-runs
            // the statement on its own connection.
            RowScope::All if paused && is_rerunnable(&src.sql) && rs.sort_state().is_none() => {
                src.conn.export_query(Some(&src.db), src.sql.clone(), self.path.clone(), opts, None)
            }
            RowScope::All => export_result(src.conn.handle(), rs.clone(), self.path.clone(), opts),
            RowScope::Fetched => {
                opts.rows = Some((0..rs.rows()).collect());
                export_result(src.conn.handle(), rs.clone(), self.path.clone(), opts)
            }
            RowScope::Selection => {
                let real = rs.rows();
                let (rows, cols) = src.selection.clone().unwrap_or_default();
                opts.rows = Some(rows.into_iter().filter(|r| *r < real).collect());
                opts.columns = Some(cols);
                export_result(src.conn.handle(), rs.clone(), self.path.clone(), opts)
            }
        };
        self.track(export, cx);
    }

    fn track(&mut self, export: dataman_core::export::Export, cx: &mut Context<Self>) {
        let progress = export.progress.clone();
        let task = export.task;
        let job_task = cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |v, cx| {
                let Some(job) = v.job.as_mut() else { return };
                job._tick = None;
                job.state = match r {
                    Ok(s) => JobState::Done(s),
                    Err(e) => JobState::Failed(format!("{e:#}")),
                };
                if !v.visible {
                    cx.emit(ExportEvent::Finished(finished_line(&job.state)));
                }
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
        self.job = Some(Job { progress, state: JobState::Running, _task: job_task, _tick: Some(tick) });
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

    pub fn demo_start(&mut self, format: ExportFormat, cx: &mut Context<Self>) {
        self.set_format(format, cx);
        self.scope = RowScope::All;
        self.path = std::env::temp_dir().join(format!("dataman-demo-export-{}.{}", std::process::id(), format.extension()));
        self.path_chosen = true;
        self.start(cx);
    }

    pub fn demo_outcome(&self) -> Option<Result<(PathBuf, u64), String>> {
        match &self.job.as_ref()?.state {
            JobState::Running => None,
            JobState::Done(s) => Some(Ok((s.path.clone(), s.rows))),
            JobState::Failed(e) => Some(Err(e.clone())),
        }
    }

    pub fn demo_close_point(&self) -> Option<gpui::Point<gpui::Pixels>> {
        self.close_bounds.get().map(|b| b.center())
    }

    pub fn demo_status(&self, cx: &App) -> Option<String> {
        self.status(&cx.theme()).map(|s| s.0)
    }

    pub fn status_line(&self) -> Option<String> {
        let j = self.job.as_ref()?;
        if !matches!(j.state, JobState::Running) {
            return None;
        }
        let rows = j.progress.rows();
        let pct = j.progress.total_rows().map(|t| format!(" {}%", (rows * 100 / t.max(1)).min(100))).unwrap_or_default();
        let key = crate::keymap::key("export").map(|k| format!(" · {k}")).unwrap_or_default();
        Some(format!("exporting{pct} · {} rows{key}", ui_count(rows as usize)))
    }

    fn status(&self, t: &Theme) -> Option<(String, Hsla)> {
        let j = self.job.as_ref()?;
        Some(match &j.state {
            JobState::Running => {
                let rows = ui_count(j.progress.rows() as usize);
                match j.progress.total_rows() {
                    Some(tot) => (format!("Exporting… {rows} of {} rows", ui_count(tot as usize)), t.tx2),
                    None => (format!("Exporting… {rows} rows"), t.tx2),
                }
            }
            JobState::Done(s) if s.cancelled => ("Stopped · nothing written".into(), t.warn),
            JobState::Done(s) => {
                let partial = if s.complete { "" } else { " (partial result)" };
                (format!("Exported {} rows{partial} · {}", ui_count(s.rows as usize), fmt_secs(s.elapsed)), t.tx2)
            }
            JobState::Failed(e) => (e.lines().next().unwrap_or(e).to_string(), t.err),
        })
    }
}

impl Render for ExportView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let running = self.is_running();
        let locked = running;
        let scrim = |inner: gpui::AnyElement, cx: &mut Context<Self>| {
            div()
                .id("export-scrim")
                .absolute()
                .top(px(metrics::TITLE_H))
                .left_0()
                .right_0()
                .bottom_0()
                .bg(t.scrim)
                .key_context("ExportDialog Modal")
                .track_focus(&self.focus)
                .on_action(cx.listener(|v, _: &CloseExport, _, cx| {
                    if v.menu.take().is_some() {
                        cx.notify();
                    } else {
                        cx.emit(ExportEvent::Close)
                    }
                }))
                .on_action(cx.listener(|v, _: &StartExport, _, cx| v.start(cx)))
                .on_action(cx.listener(|v, _: &StopExport, _, cx| v.stop(cx)))
                .on_mouse_down(MouseButton::Left, cx.listener(|_, _, _, cx| cx.emit(ExportEvent::Close)))
                .child(div().absolute().top(px(70.)).left_0().right_0().bottom(px(12.)).flex().items_start().justify_center().child(inner))
        };
        let close_at = self.close_bounds.clone();
        let header = ui::modal_header(
            &t,
            Some(icon(Icon::Export).size(px(16.)).text_color(t.tx2).into_any_element()),
            "Export result",
            div()
                .on_children_prepainted(move |b, _, _| close_at.set(b.first().copied()))
                .child(ui::modal_close(&t, "ex-close").on_click(cx.listener(|_, _, _, cx| cx.emit(ExportEvent::Close)))),
        );
        let sheet = ui::modal(&t, "export-sheet").w(px(640.)).overflow_hidden().child(header);
        let Some((loaded, paused, sel)) = self.facts() else {
            let card = sheet.child(
                div()
                    .p(px(28.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(10.))
                    .gap(px(18.))
                    .child(ui::pixel("[ NO RESULT ]", 22., t.tx4))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.))
                            .child(ui::button(&t, "ex-run", Btn::Primary).child("Run").child(ui::key_hint("⌘↩")).on_click(cx.listener(|_, _, _, cx| cx.emit(ExportEvent::Run))))
                            .child(ui::button(&t, "ex-cancel", Btn::Ghost).child("Close").child(ui::key_hint("esc")).on_click(cx.listener(|_, _, _, cx| cx.emit(ExportEvent::Close)))),
                    ),
            );
            return scrim(card.into_any_element(), cx).into_any_element();
        };
        let src = self.source.clone().unwrap();
        let ncols = src.rs.as_ref().and_then(|rs| rs.columns().map(|c| c.len())).or(src.columns);
        let formats = div().flex().flex_col().gap(px(8.)).child(ui::caps(&t, "FORMAT")).child(div().flex().flex_row().gap(px(6.)).children(ExportFormat::ALL.into_iter().map(|f| {
            let on = f == self.format;
            let label = if f == ExportFormat::SqlInsert { "SQL" } else { f.label() };
            let hov = t.prs;
            ui::probe(format!("ex-fmt-{label}"), div()
                .id(label)
                .flex_1()
                .h(px(34.))
                .px(px(10.))
                .flex()
                .items_center()
                .justify_center()
                .border_1()
                .when(on, |d| d.bg(t.acc_s).text_color(t.tx).border_color(t.acc))
                .when(!on, |d| d.bg(t.hov).text_color(t.tx2).border_color(t.line).hover(move |s| s.bg(hov)))
                .when(locked, |d| d.opacity(0.6))
                .child(div().text_size(px(12.5)).font_weight(FontWeight::SEMIBOLD).child(label))
                .when(!locked, |d| d.on_click(cx.listener(move |v, _, _, cx| v.set_format(f, cx)))))
        })));
        let menu_open = self.menu;
        let field = |v: &Self, f: Field, t: &Theme, cx: &mut Context<Self>| {
            let ix = v.field_value(f);
            let choices = f.choices(v.format);
            let value = choices.get(ix).copied().unwrap_or("");
            let open = menu_open == Some(f);
            let menu = open.then(|| {
                deferred(
                    anchored().child(ui::menu_in("ex-field-menu-in", 
                        ui::popover(t)
                            .mt(px(52.))
                            .w(px(280.))
                            .on_mouse_down_out(cx.listener(|v, _, _, cx| {
                                v.menu = None;
                                cx.notify();
                            }))
                            .children(choices.iter().enumerate().map(|(i, c)| {
                                ui::menu_item(t, ("ex-opt", i), *c, if i == ix { Some("✓") } else { None }, true)
                                    .on_click(cx.listener(move |v, _, _, cx| v.set_field(f, i, cx)))
                            })),
                    )),
                )
                .priority(3)
            });
            div()
                .relative()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(ui::caps(t, f.label()))
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
                        .child(value)
                        .child(ui::dropdown_arrow(gpui::ElementId::Name(format!("{}-arrow", f.label()).into()), open, 9., t.tx3))
                        .when(!locked, |d| {
                            d.on_click(cx.listener(move |v, _, _, cx| {
                                v.menu = if v.menu == Some(f) { None } else { Some(f) };
                                cx.notify();
                            }))
                        }),
                )
                .children(menu)
        };
        let info = |label: &'static str, value: String, t: &Theme| {
            div().flex().flex_col().gap(px(6.)).child(ui::caps(t, label)).child(
                div().h(px(28.)).px(px(10.)).flex().items_center().bg(t.field).border_1().border_color(t.field_b).text_color(t.tx2).child(value),
            )
        };
        let mut cells: Vec<gpui::AnyElement> = self.fields().into_iter().map(|f| field(self, f, &t, cx).into_any_element()).collect();
        if self.format == ExportFormat::SqlInsert {
            cells.insert(0, div().flex().flex_col().gap(px(6.)).child(ui::caps(&t, "TABLE")).child(div().h(px(30.)).child(self.table.clone())).into_any_element());
        }
        let cols_txt = match (self.scope, sel) {
            (RowScope::Selection, Some((_, c))) => format!("the selection's {c}"),
            _ => ncols.map(|n| format!("all {n}")).unwrap_or_else(|| "all".into()),
        };
        cells.push(info("COLUMNS", cols_txt, &t).into_any_element());
        let mut grid_rows: Vec<gpui::AnyElement> = Vec::new();
        let mut it = cells.into_iter();
        while let Some(a) = it.next() {
            let b = it.next();
            grid_rows.push(div().flex().flex_row().gap(px(16.)).child(div().flex_1().min_w_0().child(a)).child(div().flex_1().min_w_0().children(b)).into_any_element());
        }
        let table = src.rs.is_none();
        let all_desc = if table || paused { String::new() } else { ui_count(loaded) };
        let scopes = [
            (RowScope::All, if table { "All rows of the table" } else { "All rows of the query" }, all_desc, true),
            (RowScope::Fetched, "Rows fetched so far", ui_count(loaded), paused),
            (
                RowScope::Selection,
                "Current selection",
                sel.filter(|(r, c)| r * c > 0).map(|(r, c)| format!("{} × {c}", ui_count(r))).unwrap_or_default(),
                sel.is_some_and(|(r, c)| r * c > 0),
            ),
        ];
        let scope_list = div().flex().flex_col().gap(px(6.)).child(ui::caps(&t, "ROWS")).children(scopes.into_iter().map(|(s, name, desc, enabled)| {
            let on = s == self.scope;
            div()
                .id(name)
                .h(px(26.))
                .px(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .when(on, |d| d.bg(t.hov).text_color(t.tx))
                .when(!on, |d| d.text_color(if enabled { t.tx2 } else { t.tx4 }))
                .child(div().size(px(12.)).rounded(px(6.)).when(on, |d| d.border_4().border_color(t.acc)).when(!on, |d| d.border_1().border_color(t.line2)))
                .child(div().flex_none().child(name))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_right().text_size(px(11.)).text_color(t.tx3).child(desc))
                .when(enabled && !locked, |d| {
                    d.on_click(cx.listener(move |v, _, _, cx| {
                        v.scope = s;
                        cx.notify();
                    }))
                })
        }));
        let save = div().flex().flex_col().gap(px(6.)).child(ui::caps(&t, "SAVE TO")).child(
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
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(tilde(&self.path)))
                .child(
                    div()
                        .id("ex-choose")
                        .text_color(t.acc_t)
                        .child("Choose…")
                        .when(!locked, |d| d.on_click(cx.listener(|v, _, _, cx| v.choose_path(cx)))),
                ),
        );
        let open = self.advanced;
        let advanced = div()
            .id("ex-advanced")
            .h(px(20.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .cursor_pointer()
            .child(ui::disclosure("ex-advanced-chev", open, 10., t.tx3))
            .child(ui::caps(&t, "ADVANCED"))
            .on_click(cx.listener(|v, _, _, cx| {
                v.advanced = !v.advanced;
                v.menu = None;
                cx.notify();
            }));
        let body = div()
            .id("ex-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(20.))
            .py(px(16.))
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(formats)
            .child(scope_list)
            .child(save)
            .child(advanced)
            .when(open, |d| d.children(grid_rows));
        let status = self.status(&t);
        let done_path = match self.job.as_ref().map(|j| &j.state) {
            Some(JobState::Done(s)) if !s.cancelled => Some(s.path.clone()),
            _ => None,
        };
        let mut footer = ui::modal_footer(&t).child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(11.5))
                .when_some(status, |d, (s, c)| d.text_color(c).child(s)),
        );
        if running {
            footer = footer
                .child(ui::button(&t, "ex-bg", Btn::Ghost).h(px(28.)).child("Run in background").on_click(cx.listener(|_, _, _, cx| cx.emit(ExportEvent::Close))))
                .child(ui::button(&t, "ex-stop", Btn::Danger).h(px(28.)).child("Stop").child(ui::key_hint("⌘.")).on_click(cx.listener(|v, _, _, cx| v.stop(cx))));
        } else {
            if let Some(p) = done_path {
                footer = footer.child(ui::button(&t, "ex-reveal", Btn::Ghost).h(px(28.)).child("Reveal in Finder").on_click(move |_, _, cx| cx.reveal_path(&p)));
            }
            let close = if self.job.is_some() { "Close" } else { "Cancel" };
            footer = footer.child(ui::button(&t, "ex-cancel", Btn::Ghost).h(px(28.)).child(close).on_click(cx.listener(|_, _, _, cx| cx.emit(ExportEvent::Close)))).child(ui::probe(
                "ex-start",
                ui::button(&t, "ex-start", Btn::Primary)
                    .h(px(28.))
                    .px(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Export")
                    .child(div().text_size(px(10.5)).font_weight(FontWeight::NORMAL).opacity(0.75).child("⌘↩"))
                    .on_click(cx.listener(|v, _, _, cx| v.start(cx))),
            ));
        }
        scrim(sheet.child(body).child(footer).into_any_element(), cx).into_any_element()
    }
}

pub(crate) fn tilde(p: &Path) -> String {
    let s = p.display().to_string();
    match std::env::var("HOME") {
        Ok(h) if s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}

use crate::app::Workspace;

impl Workspace {
    pub(crate) fn open_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(e) = self.export.clone()
            && e.read(cx).is_running()
        {
            e.update(cx, |e, cx| {
                e.visible = true;
                cx.notify();
            });
            window.focus(&e.focus_handle(cx), cx);
            cx.notify();
            return;
        }
        // One modal at a time (they share `modal_return`).
        if self.modal_open(cx) {
            return;
        }
        let source = self.active_tab().and_then(|tab| {
            let tab = tab.read(cx);
            let rs = tab.result()?.clone();
            if rs.columns().is_none_or(|c| c.is_empty()) {
                return None;
            }
            let conn = tab.connection()?.clone();
            let selection = tab.grid.read(cx).selection();
            Some(Source { sql: rs.sql.clone(), rs: Some(rs), columns: None, conn, db: tab.database().to_string(), selection })
        });
        self.show_export(source, window, cx);
    }

    pub(crate) fn export_table(&mut self, conn: &str, db: &str, schema: &str, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.export.as_ref().is_some_and(|e| e.read(cx).is_running()) {
            self.notice("An export is running: wait for it, or open it with ⌘E", cx);
            return;
        }
        if self.modal_open(cx) {
            return;
        }
        let Some(c) = self.store.read(cx).connection(conn) else {
            self.notice("Connect first: the export reads the table from that server", cx);
            return;
        };
        let columns = c.schema.cached_columns(db, schema, name).map(|c| c.len());
        let sql = dataman_core::sqlgen::select_star_in(c.dialect(), schema, name, None);
        let source = Source { rs: None, sql, columns, conn: c, db: db.to_string(), selection: None };
        self.show_export(Some(source), window, cx);
    }

    fn show_export(&mut self, source: Option<Source>, window: &mut Window, cx: &mut Context<Self>) {
        self.modal_return = window.focused(cx);
        let core = self.store.read(cx).core.clone();
        let v = cx.new(|cx| ExportView::new(core, source, window, cx));
        cx.subscribe_in(&v, window, |w, v, ev: &ExportEvent, window, cx| match ev {
            ExportEvent::Close => {
                v.update(cx, |v, cx| {
                    v.visible = false;
                    cx.notify();
                });
                if !v.read(cx).is_running() {
                    w.export = None;
                }
                match w.modal_return.take() {
                    Some(f) => window.focus(&f, cx),
                    None => w.focus_editor(window, cx),
                }
                cx.notify();
            }
            ExportEvent::Run => {
                w.export = None;
                w.run_active(window, cx);
            }
            ExportEvent::Finished(line) => {
                w.export = None;
                w.notice(line.clone(), cx);
            }
        })
        .detach();
        cx.observe(&v, |_, _, cx| cx.notify()).detach();
        self.export = Some(v);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_paths() {
        assert_eq!(base_name("SELECT * FROM public.orders ORDER BY id"), "orders");
        assert_eq!(base_name("select a from \"Orders\" p"), "Orders");
        assert_eq!(base_name("SELECT 1"), "result");
        assert_eq!(base_name("SELECT * FROM (SELECT 1) x"), "result");
        let dir = std::env::temp_dir().join(format!("dataman-export-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = unique_path(&dir, "orders", "csv");
        assert_eq!(p, dir.join("orders.csv"));
        std::fs::write(&p, "x").unwrap();
        assert_eq!(unique_path(&dir, "orders", "csv"), dir.join("orders (2).csv"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn format_switch_keeps_names() {
        assert_eq!(strip_copy_suffix("events_2024"), "events_2024");
        assert_eq!(strip_copy_suffix("orders (2)"), "orders");
        assert_eq!(strip_copy_suffix("orders (v2)"), "orders (v2)");
        assert_eq!(strip_copy_suffix("q (12)"), "q");
        let dir = std::env::temp_dir().join(format!("dataman-export-fmt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let chosen = dir.join("events_2024.csv");
        assert_eq!(with_format(&chosen, "json"), (dir.join("events_2024.json"), false));
        std::fs::write(dir.join("events_2024.json"), "x").unwrap();
        assert_eq!(with_format(&chosen, "json"), (dir.join("events_2024 (2).json"), true));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn partial_exports_say_so() {
        let done = |complete| {
            JobState::Done(ExportSummary { rows: 10, bytes: 100, path: PathBuf::from("/tmp/x.csv"), elapsed: Duration::from_secs(1), cancelled: false, complete })
        };
        assert!(finished_line(&done(true)).starts_with("Exported 10 rows to x.csv"));
        assert!(finished_line(&done(false)).contains("not every row"), "{}", finished_line(&done(false)));
    }

    #[test]
    fn progress_numbers() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(16 * 1024 * 1024 + 400 * 1024), "16.4 MB");
    }
}
