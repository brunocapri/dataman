use std::collections::VecDeque;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use futures_util::SinkExt;
use tokio_postgres::Client;

use super::source::{self, Flow, Row, Spec};
use super::{ImportFailure, ImportOptions, ImportProgress, ImportReport, Method, OnError, Outcome, kind_of_type, refused};
use crate::backend::Dedicated;
use crate::backend::pg::error_info;
use crate::result::ErrorInfo;
use crate::value::Kind;

const CHUNK_ROWS: usize = 50_000;
const CHUNK_BYTES: usize = 4 << 20;
const INSERT_MAX_ROWS: usize = 5_000;
const INSERT_MAX_BYTES: usize = 1 << 20;
const SAVEPOINT: &str = "dataman_import";

pub(crate) struct Chunk {
    data: Bytes,
    ends: Vec<u32>,
    lines: Vec<u64>,
}

impl Chunk {
    fn rows(&self) -> usize {
        self.ends.len()
    }

    fn span(&self, r: &Range<usize>) -> Bytes {
        let start = if r.start == 0 { 0 } else { self.ends[r.start - 1] as usize };
        self.data.slice(start..self.ends[r.end - 1] as usize)
    }
}

enum Msg {
    Chunk(Chunk),
    Bad(u64, String),
    Failed(String),
}

#[derive(Clone)]
pub(crate) struct Columns {
    map: Vec<usize>,
    kinds: Vec<Kind>,
    text: Vec<bool>,
}

impl Columns {
    pub(crate) fn of(opts: &ImportOptions) -> Columns {
        let kinds: Vec<Kind> = opts.columns.iter().map(|c| kind_of_type(&c.type_name)).collect();
        let text = kinds.iter().map(|k| *k == Kind::Text).collect();
        Columns { map: opts.columns.iter().map(|c| c.source).collect(), kinds, text }
    }

    fn value<'r>(&self, row: &'r Row, j: usize) -> Option<&'r str> {
        row.get(self.map[j]).filter(|v| self.text[j] || !v.is_empty())
    }
}

pub(crate) fn copy_escape(out: &mut BytesMut, v: &str) {
    let b = v.as_bytes();
    if !b.iter().any(|c| matches!(c, b'\\' | b'\t' | b'\n' | b'\r')) {
        out.extend_from_slice(b);
        return;
    }
    for &c in b {
        match c {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            c => out.extend_from_slice(&[c]),
        }
    }
}

pub(crate) fn copy_line(out: &mut BytesMut, cols: &Columns, row: &Row) {
    for j in 0..cols.map.len() {
        if j > 0 {
            out.extend_from_slice(b"\t");
        }
        match cols.value(row, j) {
            None => out.extend_from_slice(b"\\N"),
            Some(v) => copy_escape(out, v),
        }
    }
    out.extend_from_slice(b"\n");
}

pub(crate) fn insert_tuple(out: &mut String, cols: &Columns, row: &Row) {
    out.push('(');
    for j in 0..cols.map.len() {
        if j > 0 {
            out.push_str(", ");
        }
        crate::sqlgen::push_value_literal(out, cols.kinds[j], cols.value(row, j));
    }
    out.push(')');
}

struct Encoder {
    cols: Columns,
    copy: bool,
    data: BytesMut,
    text: String,
    ends: Vec<u32>,
    lines: Vec<u64>,
}

impl Encoder {
    fn push(&mut self, row: &Row, line: u64) {
        if self.copy {
            copy_line(&mut self.data, &self.cols, row);
            self.ends.push(self.data.len() as u32);
        } else {
            self.text.clear();
            insert_tuple(&mut self.text, &self.cols, row);
            self.data.extend_from_slice(self.text.as_bytes());
            self.ends.push(self.data.len() as u32);
        }
        self.lines.push(line);
    }

    fn full(&self) -> bool {
        self.ends.len() >= CHUNK_ROWS || self.data.len() >= CHUNK_BYTES
    }

    fn take(&mut self) -> Option<Chunk> {
        if self.ends.is_empty() {
            return None;
        }
        Some(Chunk { data: self.data.split().freeze(), ends: std::mem::take(&mut self.ends), lines: std::mem::take(&mut self.lines) })
    }
}

fn produce(path: PathBuf, opts: ImportOptions, copy: bool, progress: Arc<ImportProgress>, tx: tokio::sync::mpsc::Sender<Msg>) {
    let cols = Columns::of(&opts);
    let enc = std::cell::RefCell::new(Encoder {
        cols,
        copy,
        data: BytesMut::with_capacity(CHUNK_BYTES + (64 << 10)),
        text: String::new(),
        ends: Vec::new(),
        lines: Vec::new(),
    });
    let spec = Spec { format: opts.format, dialect: &opts.dialect, names: &opts.source_names };
    let bytes = Arc::new(AtomicU64::new(0));
    let skip = opts.on_error == OnError::Skip;
    let p = progress.clone();
    let b = bytes.clone();
    let send = |tx: &tokio::sync::mpsc::Sender<Msg>, m: Msg| tx.blocking_send(m).is_ok();
    let r = source::read_rows(
        &path,
        &spec,
        bytes.clone(),
        &mut |row, line| {
            if p.is_cancelled() {
                return Ok(Flow::Stop);
            }
            let mut e = enc.borrow_mut();
            e.push(row, line);
            p.read.fetch_add(1, Ordering::Relaxed);
            if e.full() {
                p.bytes.store(b.load(Ordering::Relaxed), Ordering::Relaxed);
                let c = e.take().unwrap();
                drop(e);
                if !send(&tx, Msg::Chunk(c)) {
                    return Ok(Flow::Stop);
                }
            }
            Ok(Flow::Go)
        },
        &mut |line, why| {
            if skip {
                p.skip(line, why);
                return Ok(Flow::Go);
            }
            let pending = enc.borrow_mut().take();
            if let Some(c) = pending {
                send(&tx, Msg::Chunk(c));
            }
            send(&tx, Msg::Bad(line, why));
            Ok(Flow::Stop)
        },
    );
    progress.bytes.store(bytes.load(Ordering::Relaxed), Ordering::Relaxed);
    match r {
        Ok(()) => {
            let last = enc.borrow_mut().take();
            if let Some(c) = last {
                send(&tx, Msg::Chunk(c));
            }
        }
        Err(e) => {
            send(&tx, Msg::Failed(format!("{e:#}")));
        }
    }
}

struct Fail {
    error: ErrorInfo,
    bad: Option<Range<usize>>,
}

impl Fail {
    fn of(e: &tokio_postgres::Error, bad: Option<Range<usize>>) -> Fail {
        Fail { error: error_info(e), bad }
    }

    fn copy(e: &tokio_postgres::Error) -> Fail {
        let line = e.as_db_error().and_then(|d| d.where_()).and_then(copy_line_of);
        Fail::of(e, line.filter(|l| *l > 0).map(|l| (l as usize - 1)..l as usize))
    }
}

pub(crate) fn copy_line_of(where_: &str) -> Option<u64> {
    let at = where_.find("line ")? + 5;
    let digits: String = where_[at..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

struct Loader<'a> {
    client: &'a Client,
    opts: &'a ImportOptions,
    copy: Option<tokio_postgres::Statement>,
    insert_head: String,
    one_tx: bool,
    savepoints: bool,
}

fn row_error(e: &ErrorInfo) -> bool {
    e.code.as_deref().is_some_and(|c| c.starts_with("22") || c.starts_with("23"))
}

impl Loader<'_> {
    async fn exec(&self, sql: &str) -> Result<(), Fail> {
        self.client.batch_execute(sql).await.map_err(|e| Fail::of(&e, None))
    }

    async fn send(&self, c: &Chunk, r: Range<usize>) -> Result<(), Fail> {
        if let Some(stmt) = &self.copy {
            let sink = self.client.copy_in::<_, Bytes>(stmt).await.map_err(|e| Fail::copy(&e))?;
            futures_util::pin_mut!(sink);
            sink.send(c.span(&r)).await.map_err(|e| Fail::copy(&e))?;
            sink.as_mut().finish().await.map_err(|e| Fail::copy(&e))?;
            return Ok(());
        }
        let max_rows = if self.opts.insert_rows > 0 { self.opts.insert_rows } else { INSERT_MAX_ROWS };
        let mut at = r.start;
        let mut sql = String::new();
        while at < r.end {
            sql.clear();
            sql.push_str(&self.insert_head);
            let from = at;
            while at < r.end && at - from < max_rows && sql.len() < INSERT_MAX_BYTES {
                if at > from {
                    sql.push_str(", ");
                }
                let row = c.span(&(at..at + 1));
                sql.push_str(std::str::from_utf8(&row).unwrap_or(""));
                at += 1;
            }
            self.client.batch_execute(&sql).await.map_err(|e| Fail::of(&e, Some((from - r.start)..(at - r.start))))?;
        }
        Ok(())
    }

    async fn atomic(&self, c: &Chunk, r: Range<usize>) -> Result<(), Fail> {
        if self.one_tx {
            if !self.savepoints {
                return self.send(c, r).await;
            }
            self.exec(&format!("SAVEPOINT {SAVEPOINT}")).await?;
            match self.send(c, r).await {
                Ok(()) => self.exec(&format!("RELEASE SAVEPOINT {SAVEPOINT}")).await,
                Err(f) => {
                    let _ = self.exec(&format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}; RELEASE SAVEPOINT {SAVEPOINT}")).await;
                    Err(f)
                }
            }
        } else if self.copy.is_some() || self.one_statement(c, &r) {
            self.send(c, r).await
        } else {
            self.exec("BEGIN").await?;
            match self.send(c, r).await {
                Ok(()) => self.exec("COMMIT").await,
                Err(f) => {
                    let _ = self.exec("ROLLBACK").await;
                    Err(f)
                }
            }
        }
    }

    fn one_statement(&self, c: &Chunk, r: &Range<usize>) -> bool {
        let max_rows = if self.opts.insert_rows > 0 { self.opts.insert_rows } else { INSERT_MAX_ROWS };
        r.len() <= max_rows && self.insert_head.len() + c.span(r).len() + 2 * r.len() < INSERT_MAX_BYTES
    }

    async fn try_prefix(&self, c: &Chunk, r: &Range<usize>, k: usize) -> Result<Option<Fail>, Fail> {
        self.exec("SAVEPOINT dataman_locate").await?;
        let res = self.send(c, r.start..r.start + k).await;
        self.exec("ROLLBACK TO SAVEPOINT dataman_locate; RELEASE SAVEPOINT dataman_locate").await?;
        Ok(res.err())
    }

    async fn locate(&self, c: &Chunk, r: Range<usize>) -> Option<(usize, ErrorInfo)> {
        if !self.one_tx && self.exec("BEGIN").await.is_err() {
            return None;
        }
        let found = async {
            let (mut lo, mut hi) = (1usize, r.len());
            let mut err = None;
            while lo < hi {
                let mid = (lo + hi) / 2;
                match self.try_prefix(c, &r, mid).await.ok()? {
                    Some(f) => {
                        hi = mid;
                        err = Some(f.error);
                    }
                    None => lo = mid + 1,
                }
            }
            let err = match err {
                Some(e) if hi == lo => e,
                _ => self.try_prefix(c, &r, lo).await.ok()??.error,
            };
            Some((r.start + lo - 1, err))
        }
        .await;
        if !self.one_tx {
            let _ = self.exec("ROLLBACK").await;
        }
        found
    }
}

async fn cancelled(p: &ImportProgress) {
    while !p.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Sent as a simple query: through `copy_in` (extended protocol) an error before the
/// CopyInResponse leaves tokio-postgres one ReadyForQuery out of step (it sends CopyFail and a
/// second Sync), and the next statement fails with "unexpected message".
async fn copy_supported(client: &Client) -> bool {
    match client.simple_query("COPY \"dataman import probe 7f3a\" FROM STDIN").await {
        Ok(_) => true,
        Err(e) => match e.as_db_error() {
            Some(d) => !matches!(d.code().code(), "0A000" | "42601" | "XX000"),
            None => true,
        },
    }
}

pub(crate) async fn run(
    conn: Arc<crate::Connection>,
    db: Option<String>,
    path: PathBuf,
    opts: ImportOptions,
    progress: Arc<ImportProgress>,
) -> ImportReport {
    let t0 = Instant::now();
    let mut report = ImportReport {
        outcome: Outcome::Done,
        rows: 0,
        skipped: 0,
        issues: Vec::new(),
        method: Method::Auto,
        elapsed: Duration::ZERO,
        bytes: 0,
        rolled_back: false,
    };
    let finish = |mut report: ImportReport, progress: &ImportProgress| {
        report.elapsed = t0.elapsed();
        report.skipped = progress.skipped();
        report.issues = progress.issues();
        report.bytes = progress.bytes();
        progress.finished.store(true, Ordering::Release);
        report
    };
    // A read-only connection never writes, LAR included (it accepts READ ONLY without
    // enforcing it).
    if conn.read_only_for(db.as_deref().unwrap_or(conn.database())) {
        report.outcome = Outcome::Failed(refused("25006", "read-only database: the import is not run"));
        return finish(report, &progress);
    }
    if let Err(e) = opts.check() {
        report.outcome = Outcome::Failed(ImportFailure::plain(e));
        return finish(report, &progress);
    }
    if let Ok(m) = std::fs::metadata(&path) {
        progress.total_bytes.store(m.len(), Ordering::Relaxed);
    }
    if opts.format == super::ImportFormat::Parquet
        && let Ok(f) = std::fs::File::open(&path)
        && let Ok(b) = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(f)
    {
        progress.total_rows.store(b.metadata().file_metadata().num_rows().max(0) as u64, Ordering::Relaxed);
    }
    let d: Dedicated = match conn.dedicated(db.as_deref()).await {
        Ok(d) => d,
        Err(e) => {
            report.outcome = Outcome::Failed(ImportFailure::plain(format!("{e:#}")));
            return finish(report, &progress);
        }
    };
    let Some(pg) = d.pg() else {
        report.outcome = Outcome::Failed(ImportFailure::plain(format!("imports into {} are not available", conn.engine().label())));
        return finish(report, &progress);
    };
    let client: &Client = &pg.client;
    let method = match opts.method {
        Method::Auto => {
            if conn.caps().copy_import && copy_supported(client).await {
                Method::Copy
            } else {
                Method::Insert
            }
        }
        m => m,
    };
    report.method = method;
    let one_tx = opts.one_transaction;
    let skip = opts.on_error == OnError::Skip;
    let mut loader = Loader {
        client,
        opts: &opts,
        copy: None,
        insert_head: opts.insert_head(),
        one_tx,
        savepoints: one_tx && (skip || method == Method::Insert || conn.info.is_lar),
    };

    let setup = async {
        if one_tx {
            loader.exec("BEGIN").await?;
        }
        if opts.create {
            loader.exec(&opts.create_sql()).await?;
        } else if opts.truncate {
            loader.exec(&opts.truncate_sql()).await?;
        }
        if method == Method::Copy {
            loader.copy = Some(client.prepare(&opts.copy_sql()).await.map_err(|e| Fail::of(&e, None))?);
        }
        Ok::<(), Fail>(())
    };
    if let Err(f) = setup.await {
        if one_tx {
            let _ = loader.exec("ROLLBACK").await;
            report.rolled_back = true;
        }
        report.outcome = Outcome::Failed(ImportFailure { error: f.error, line: None, lines: None });
        return finish(report, &progress);
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Msg>(2);
    let producer = {
        let (path, opts, progress) = (path.clone(), opts.clone(), progress.clone());
        conn.handle().spawn_blocking(move || produce(path, opts, method == Method::Copy, progress, tx))
    };

    let mut loaded = 0u64;
    let mut outcome = Outcome::Done;
    'chunks: loop {
        let msg = tokio::select! {
            m = rx.recv() => m,
            _ = cancelled(&progress) => { outcome = Outcome::Cancelled; break 'chunks; }
        };
        let chunk = match msg {
            None => break,
            Some(Msg::Chunk(c)) => c,
            Some(Msg::Bad(line, why)) => {
                outcome = Outcome::Failed(ImportFailure::plain(why).at(line));
                break;
            }
            Some(Msg::Failed(why)) => {
                outcome = Outcome::Failed(ImportFailure::plain(why));
                break;
            }
        };
        let mut todo: VecDeque<Range<usize>> = VecDeque::from([0..chunk.rows()]);
        while let Some(r) = todo.pop_front() {
            let res = tokio::select! {
                r = loader.atomic(&chunk, r.clone()) => r,
                _ = cancelled(&progress) => {
                    outcome = Outcome::Cancelled;
                    break 'chunks;
                }
            };
            match res {
                Ok(()) => {
                    loaded += r.len() as u64;
                    progress.rows.store(loaded, Ordering::Relaxed);
                }
                Err(f) if !row_error(&f.error) => {
                    outcome = Outcome::Failed(ImportFailure { error: f.error, line: None, lines: None });
                    break 'chunks;
                }
                Err(f) if !skip => {
                    let exact = f.bad.as_ref().filter(|b| b.len() == 1).map(|b| r.start + b.start);
                    let failure = match exact {
                        Some(i) => ImportFailure { error: f.error, line: Some(chunk.lines[i]), lines: None },
                        None if loader.savepoints || !one_tx => match loader.locate(&chunk, r.clone()).await {
                            Some((i, e)) => ImportFailure { error: e, line: Some(chunk.lines[i]), lines: None },
                            None => ImportFailure { error: f.error, line: None, lines: Some((chunk.lines[r.start], chunk.lines[r.end - 1])) },
                        },
                        None => {
                            let b = f.bad.clone().map(|b| (r.start + b.start)..(r.start + b.end)).unwrap_or(r.clone());
                            ImportFailure { error: f.error, line: None, lines: Some((chunk.lines[b.start], chunk.lines[b.end - 1])) }
                        }
                    };
                    outcome = Outcome::Failed(failure);
                    break 'chunks;
                }
                Err(f) if r.len() == 1 => progress.skip(chunk.lines[r.start], f.error.message.clone()),
                Err(f) => {
                    let parts: Vec<Range<usize>> = match f.bad.filter(|b| b.end <= r.len() && !b.is_empty() && b.len() < r.len()) {
                        Some(b) if b.len() == 1 => {
                            let bad = r.start + b.start;
                            progress.skip(chunk.lines[bad], f.error.message.clone());
                            vec![r.start..bad, bad + 1..r.end]
                        }
                        Some(b) => vec![r.start..r.start + b.start, r.start + b.start..r.start + b.end, r.start + b.end..r.end],
                        None => {
                            let mid = r.start + r.len() / 2;
                            vec![r.start..mid, mid..r.end]
                        }
                    };
                    for p in parts.into_iter().rev().filter(|p| !p.is_empty()) {
                        todo.push_front(p);
                    }
                }
            }
        }
    }
    drop(rx);
    if !matches!(outcome, Outcome::Done) {
        progress.cancel();
    }
    let _ = producer.await;

    match &outcome {
        Outcome::Done => {
            if one_tx && let Err(f) = loader.exec("COMMIT").await {
                outcome = Outcome::Failed(ImportFailure { error: f.error, line: None, lines: None });
                report.rolled_back = true;
                loaded = 0;
            }
        }
        _ => {
            let _ = loader.exec("ROLLBACK").await;
            if one_tx {
                report.rolled_back = true;
                loaded = 0;
                // LAR's DDL is not transactional: the table created for this import stays
                // after the ROLLBACK; drop it (TRUNCATE cannot be undone there either).
                if opts.create && conn.info.is_lar {
                    let _ = loader.exec(&format!("DROP TABLE {}", opts.qualified())).await;
                }
            }
        }
    }
    progress.rows.store(loaded, Ordering::Relaxed);
    report.rows = loaded;
    report.outcome = outcome;
    finish(report, &progress)
}

impl ImportFailure {
    fn at(mut self, line: u64) -> ImportFailure {
        self.line = Some(line);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{ColumnMap, CsvDialect, ImportFormat};

    fn opts(types: &[&str]) -> ImportOptions {
        ImportOptions {
            format: ImportFormat::Csv,
            dialect: CsvDialect::default(),
            source_names: types.iter().enumerate().map(|(i, _)| format!("c{i}")).collect(),
            schema: "public".into(),
            table: "t".into(),
            create: false,
            columns: types.iter().enumerate().map(|(i, t)| ColumnMap { source: i, target: format!("c{i}"), type_name: t.to_string() }).collect(),
            truncate: false,
            on_error: OnError::Stop,
            one_transaction: true,
            method: Method::Auto,
            insert_rows: 0,
        }
    }

    fn row(vals: &[Option<&str>]) -> Row {
        Row::from_values(vals)
    }

    #[test]
    fn copy_text_escapes_nulls_and_unicode() {
        let cols = Columns::of(&opts(&["text", "int8", "text", "jsonb", "text"]));
        let mut out = BytesMut::new();
        copy_line(&mut out, &cols, &row(&[Some("a\tb\\c\nd\re"), Some(""), Some(""), None, Some("café 😀 \"q\" 'x'")]));
        assert_eq!(std::str::from_utf8(&out).unwrap(), "a\\tb\\\\c\\nd\\re\t\\N\t\t\\N\tcafé 😀 \"q\" 'x'\n");
        let mut out = BytesMut::new();
        copy_line(&mut out, &Columns::of(&opts(&["text", "text"])), &row(&[Some("\\N"), Some("\\.")]));
        assert_eq!(std::str::from_utf8(&out).unwrap(), "\\\\N\t\\\\.\n");
    }

    #[test]
    fn insert_tuples_follow_the_types() {
        let cols = Columns::of(&opts(&["integer", "numeric(10,2)", "boolean", "text", "timestamptz", "text", "jsonb"]));
        let mut s = String::new();
        insert_tuple(&mut s, &cols, &row(&[Some("42"), Some("1.50"), Some("yes"), Some("it's"), Some("2024-01-01 10:00:00+00"), None, Some("")]));
        assert_eq!(s, "(42, 1.50, TRUE, 'it''s', '2024-01-01 10:00:00+00', NULL, NULL)");
        let mut s = String::new();
        insert_tuple(&mut s, &cols, &row(&[Some("4x"), Some("NaN"), Some("maybe"), Some("a\\b\nc"), Some("x"), Some(""), Some("{\"a\": 1}")]));
        assert_eq!(s, "('4x', 'NaN', 'maybe', 'a\\b\nc', 'x', '', '{\"a\": 1}')");
    }

    #[test]
    fn copy_error_lines() {
        assert_eq!(copy_line_of("COPY t, line 12, column id: \"x\""), Some(12));
        assert_eq!(copy_line_of("COPY t, line 3"), Some(3));
        assert_eq!(copy_line_of("PL/pgSQL function f()"), None);
    }

    #[test]
    fn chunks_slice_rows() {
        let cols = Columns::of(&opts(&["text"]));
        let mut enc = Encoder { cols, copy: true, data: BytesMut::new(), text: String::new(), ends: Vec::new(), lines: Vec::new() };
        for (i, v) in ["a", "bb", "ccc"].iter().enumerate() {
            enc.push(&row(&[Some(v)]), 10 + i as u64);
        }
        let c = enc.take().unwrap();
        assert_eq!(c.rows(), 3);
        assert_eq!(&c.span(&(1..3))[..], b"bb\nccc\n");
        assert_eq!(&c.span(&(0..1))[..], b"a\n");
        assert_eq!(c.lines, vec![10, 11, 12]);
        assert!(enc.take().is_none());
    }
}
