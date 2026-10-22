//! LAR does not accept `IS NOT DISTINCT FROM` in writes (verified: `0A000 not supported`), so
//! there the row is first re-read by its key (allowed inside a transaction even after writing
//! the table) and compared.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::Result;

use crate::Task;
use crate::backend::Db;
use crate::conn::{Connection, first_ok};
use crate::result::{Cell, ErrorInfo, ResultSet, Snapshot};
use crate::sql::lexer::{Token, TokenKind, tokenize_in};
use crate::sql::{Dialect, ident_value_in};
use crate::sqlgen::{cell_text_utc, literal_in, qualified, value_literal_in};
use crate::value::Kind;

pub use crate::sqlgen::CellValue;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

impl TableRef {
    pub fn sql(&self) -> String {
        qualified(self.schema.as_deref(), &self.name)
    }

    pub fn sql_in(&self, dialect: Dialect) -> String {
        dialect.qualified(self.schema.as_deref(), &self.name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    PrimaryKey,
    UniqueIndex,
    Guessed,
    Explicit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictCheck {
    Predicate,
    Reread,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyColumn {
    pub name: String,
    pub result_col: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EditTarget {
    pub table: TableRef,
    pub key: Vec<KeyColumn>,
    pub key_source: KeySource,
    pub columns: Vec<Option<String>>,
    pub kinds: Vec<Kind>,
    pub type_names: Vec<String>,
    pub check: ConflictCheck,
    pub not_null: Vec<bool>,
    pub dialect: Dialect,
    pub generated: Vec<bool>,
}

impl EditTarget {
    pub fn is_editable(&self, col: usize) -> bool {
        self.columns.get(col).is_some_and(|c| c.is_some()) && !self.generated.get(col).copied().unwrap_or(false)
    }

    pub fn nullable(&self, col: usize) -> bool {
        !self.not_null.get(col).copied().unwrap_or(false)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Editability {
    Editable(EditTarget),
    ReadOnly(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellEdit {
    pub old: CellValue,
    pub new: CellValue,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChangeSet {
    edits: BTreeMap<usize, BTreeMap<usize, CellEdit>>,
    inserts: Vec<Vec<Option<CellValue>>>,
    deletes: BTreeSet<usize>,
}

impl ChangeSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn edit(&mut self, row: usize, col: usize, old: CellValue, new: CellValue) {
        let r = self.edits.entry(row).or_default();
        let orig = r.get(&col).map(|e| e.old.clone()).unwrap_or(old);
        if orig == new {
            r.remove(&col);
        } else {
            r.insert(col, CellEdit { old: orig, new });
        }
        if r.is_empty() {
            self.edits.remove(&row);
        }
    }

    pub fn revert_cell(&mut self, row: usize, col: usize) {
        if let Some(r) = self.edits.get_mut(&row) {
            r.remove(&col);
            if r.is_empty() {
                self.edits.remove(&row);
            }
        }
    }

    pub fn edited(&self, row: usize, col: usize) -> Option<&CellEdit> {
        self.edits.get(&row)?.get(&col)
    }

    pub fn delete_row(&mut self, row: usize) {
        self.edits.remove(&row);
        self.deletes.insert(row);
    }

    pub fn undelete_row(&mut self, row: usize) {
        self.deletes.remove(&row);
    }

    pub fn is_deleted(&self, row: usize) -> bool {
        self.deletes.contains(&row)
    }

    pub fn insert_row(&mut self, ncols: usize) -> usize {
        self.inserts.push(vec![None; ncols]);
        self.inserts.len() - 1
    }

    pub fn set_inserted(&mut self, index: usize, col: usize, v: Option<CellValue>) {
        if let Some(c) = self.inserts.get_mut(index).and_then(|r| r.get_mut(col)) {
            *c = v;
        }
    }

    pub fn remove_inserted(&mut self, index: usize) {
        if index < self.inserts.len() {
            self.inserts.remove(index);
        }
    }

    pub fn inserted(&self) -> &[Vec<Option<CellValue>>] {
        &self.inserts
    }

    pub fn edited_rows(&self) -> impl Iterator<Item = (usize, &BTreeMap<usize, CellEdit>)> {
        self.edits.iter().map(|(r, e)| (*r, e))
    }

    pub fn deleted_rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.deletes.iter().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.edits.is_empty() && self.inserts.is_empty() && self.deletes.is_empty()
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        (self.edits.len(), self.inserts.len(), self.deletes.len())
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn plan(&self, target: &EditTarget, snap: &Snapshot) -> Result<Vec<PlannedChange>, String> {
        let mut out = Vec::new();
        let d = target.dialect;
        let quote_ident = |name: &str| d.quote_ident(name);
        let table = target.table.sql_in(d);
        let key_where = |row: usize| -> Result<(String, Vec<(String, Value)>), String> {
            if row >= snap.source_rows() {
                return Err(format!("row {} is not in the result", row + 1));
            }
            let mut parts = Vec::new();
            let mut vals = Vec::new();
            for k in &target.key {
                let cell = snap.source_cell(row, k.result_col);
                if cell == Cell::Null {
                    return Err(format!("row {} has no value in key column {}", row + 1, k.name));
                }
                parts.push(format!("{} = {}", quote_ident(&k.name), key_literal(d, target.kinds[k.result_col], cell)));
                vals.push((k.name.clone(), Value::from(cell)));
            }
            Ok((parts.join(" AND "), vals))
        };
        for (&row, edits) in &self.edits {
            if self.deletes.contains(&row) {
                continue;
            }
            let (kw, _) = key_where(row)?;
            let mut sets = Vec::new();
            let mut checks = Vec::new();
            let mut expect = Vec::new();
            for (&col, e) in edits {
                let Some(name) = target.columns.get(col).cloned().flatten() else {
                    return Err(format!("column {} is computed and cannot be edited", col + 1));
                };
                if target.generated.get(col).copied().unwrap_or(false) {
                    return Err(format!("{name} is written by the server and cannot be edited"));
                }
                if target.key.iter().any(|k| k.result_col == col) && target.check == ConflictCheck::Reread {
                    return Err(format!("LAR cannot change a primary key ({name}): delete the row and insert a new one"));
                }
                let kind = target.kinds[col];
                sets.push(format!("{} = {}", quote_ident(&name), value_literal_in(d, kind, &e.new)));
                let cell = snap.source_cell(row, col);
                if comparable(d, &target.type_names[col]) {
                    checks.push(match cell_text_utc(kind, cell) {
                        None => format!("{} IS NULL", quote_ident(&name)),
                        Some(t) => d.not_distinct(&quote_ident(&name), &compare_literal(d, kind, &t)),
                    });
                    expect.push((name.clone(), col, Value::from(cell)));
                }
            }
            let mut sql = format!("UPDATE {table} SET {} WHERE {kw}", sets.join(", "));
            let mut reread = None;
            match target.check {
                ConflictCheck::Predicate => {
                    for c in &checks {
                        sql.push_str(" AND ");
                        sql.push_str(c);
                    }
                }
                ConflictCheck::Reread if !expect.is_empty() => {
                    let cols = expect.iter().map(|(n, _, _)| quote_ident(n)).collect::<Vec<_>>().join(", ");
                    reread = Some(Reread {
                        sql: format!("SELECT {cols} FROM {table} WHERE {kw}"),
                        expect: expect.iter().map(|(n, _, v)| (n.clone(), v.clone())).collect(),
                    });
                }
                ConflictCheck::Reread => {}
            }
            out.push(PlannedChange {
                kind: ChangeKind::Update,
                row: Some(row),
                insert: None,
                sql,
                exists_sql: Some(format!("SELECT 1 FROM {table} WHERE {kw}")),
                reread,
                key: kw,
            });
        }
        for (i, vals) in self.inserts.iter().enumerate() {
            let mut cols = Vec::new();
            let mut lits = Vec::new();
            for (col, v) in vals.iter().enumerate() {
                let Some(v) = v else { continue };
                if target.generated.get(col).copied().unwrap_or(false) {
                    continue;
                }
                let Some(name) = target.columns.get(col).cloned().flatten() else { continue };
                cols.push(quote_ident(&name));
                lits.push(value_literal_in(d, target.kinds[col], v));
            }
            let sql = if cols.is_empty() {
                if d == Dialect::MySql { format!("INSERT INTO {table} () VALUES ()") } else { format!("INSERT INTO {table} DEFAULT VALUES") }
            } else {
                format!("INSERT INTO {table} ({}) VALUES ({})", cols.join(", "), lits.join(", "))
            };
            out.push(PlannedChange { kind: ChangeKind::Insert, row: None, insert: Some(i), sql, exists_sql: None, reread: None, key: String::new() });
        }
        for &row in &self.deletes {
            let (kw, _) = key_where(row)?;
            // Like an UPDATE, a DELETE must not remove a row someone changed since it was read:
            // every column the grid shows (and can compare) must still hold what it showed.
            let mut checks = Vec::new();
            let mut expect = Vec::new();
            for (col, name) in target.columns.iter().enumerate() {
                let Some(name) = name else { continue };
                if target.key.iter().any(|k| k.result_col == col) || !delete_checkable(d, target.kinds[col], &target.type_names[col]) {
                    continue;
                }
                let cell = snap.source_cell(row, col);
                checks.push(match cell_text_utc(target.kinds[col], cell) {
                    None => format!("{} IS NULL", quote_ident(name)),
                    Some(t) => d.not_distinct(&quote_ident(name), &compare_literal(d, target.kinds[col], &t)),
                });
                expect.push((name.clone(), Value::from(cell)));
            }
            let mut sql = format!("DELETE FROM {table} WHERE {kw}");
            let mut reread = None;
            match target.check {
                ConflictCheck::Predicate => {
                    for c in &checks {
                        sql.push_str(" AND ");
                        sql.push_str(c);
                    }
                }
                ConflictCheck::Reread if !expect.is_empty() => {
                    let cols = expect.iter().map(|(n, _)| quote_ident(n)).collect::<Vec<_>>().join(", ");
                    reread = Some(Reread { sql: format!("SELECT {cols} FROM {table} WHERE {kw}"), expect });
                }
                ConflictCheck::Reread => {}
            }
            out.push(PlannedChange {
                kind: ChangeKind::Delete,
                row: Some(row),
                insert: None,
                sql,
                exists_sql: Some(format!("SELECT 1 FROM {table} WHERE {kw}")),
                reread,
                key: kw,
            });
        }
        Ok(out)
    }
}

/// MySQL's FLOAT never equals the decimal it prints (`0.1` is 0.100000001…), and its JSON only
/// equals JSON.
fn comparable(d: Dialect, type_name: &str) -> bool {
    let base = type_name.trim_start_matches('_');
    // SQLite's REAL is a double, which reads back the decimal it prints exactly.
    if d == Dialect::Sqlite {
        return true;
    }
    if d == Dialect::DuckDb {
        // FLOAT is single precision: it never equals the decimal it prints.
        let t = base.to_ascii_uppercase();
        return !matches!(t.as_str(), "FLOAT" | "FLOAT4" | "REAL") && !t.contains("[]") && !t.starts_with("STRUCT") && !t.starts_with("MAP") && !t.starts_with("UNION");
    }
    if d == Dialect::TSql {
        // SQL Server cannot compare its old large types, XML and CLR types with `=`; floats
        // never equal the decimal they print, and the grid keeps time to the µs (datetime2's
        // seventh digit is not shown).
        return !matches!(
            base,
            "text" | "ntext" | "image" | "xml" | "udt" | "sql_variant" | "float" | "real" | "datetime2" | "time" | "datetimeoffset"
        );
    }
    !matches!(base, "json" | "xml" | "point" | "line" | "lseg" | "box" | "path" | "polygon" | "circle" | "jsonpath" | "lquery" | "ltxtquery" | "txid_snapshot" | "pg_snapshot" | "float" | "float unsigned" | "geometry")
}

/// MySQL compares a number with a string as doubles (an unsigned BIGINT would lose digits):
/// numbers go bare there, bytes as `X'…'`.
fn compare_literal(d: Dialect, kind: Kind, shown: &str) -> String {
    if d != Dialect::Postgres {
        return value_literal_in(d, kind, &CellValue::Text(shown.to_string()));
    }
    d.quote_literal(shown)
}

fn delete_checkable(d: Dialect, kind: Kind, type_name: &str) -> bool {
    comparable(d, type_name)
        && matches!(
            kind,
            Kind::Bool
                | Kind::Int
                | Kind::Float
                | Kind::Float4
                | Kind::Numeric
                | Kind::Text
                | Kind::Date
                | Kind::Time
                | Kind::Timestamp
                | Kind::TimestampTz
                | Kind::Bytes
                | Kind::Json
        )
}

/// A `real` written bare (`0.1`) is a numeric constant, compared after widening both sides to
/// double precision, so it never matches the stored float4: cast it.
fn key_literal(d: Dialect, kind: Kind, cell: Cell<'_>) -> String {
    let lit = literal_in(d, kind, cell);
    if d == Dialect::Postgres && kind == Kind::Float4 && matches!(cell, Cell::Float(_)) { format!("{lit}::real") } else { lit }
}

#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl From<Cell<'_>> for Value {
    fn from(c: Cell<'_>) -> Value {
        match c {
            Cell::Null => Value::Null,
            Cell::Bool(b) => Value::Bool(b),
            Cell::Int(v) => Value::Int(v),
            Cell::Float(f) => Value::Float(f),
            Cell::Text(s) => Value::Text(s.to_string()),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, o: &Value) -> bool {
        match (self, o) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b || (a.is_nan() && b.is_nan()),
            (Value::Text(a), Value::Text(b)) => a == b,
            _ => false,
        }
    }
}

impl Value {
    pub fn display(&self, kind: Kind) -> String {
        let c = match self {
            Value::Null => Cell::Null,
            Value::Bool(b) => Cell::Bool(*b),
            Value::Int(v) => Cell::Int(*v),
            Value::Float(f) => Cell::Float(*f),
            Value::Text(s) => Cell::Text(s),
        };
        cell_text_utc(kind, c).unwrap_or_else(|| "NULL".into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Update,
    Insert,
    Delete,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reread {
    pub sql: String,
    pub expect: Vec<(String, Value)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlannedChange {
    pub kind: ChangeKind,
    pub row: Option<usize>,
    pub insert: Option<usize>,
    pub sql: String,
    pub exists_sql: Option<String>,
    pub reread: Option<Reread>,
    pub key: String,
}

pub fn preview(changes: &[PlannedChange]) -> String {
    let mut s = String::new();
    for c in changes {
        if let Some(r) = &c.reread {
            s.push_str("-- check: ");
            s.push_str(&r.sql);
            s.push('\n');
        }
        s.push_str(&c.sql);
        s.push_str(";\n");
    }
    s
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConflictKind {
    RowGone,
    Changed { column: Option<String>, expected: Option<String>, found: Option<String> },
    NotUnique(u64),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Applied { rows: u64 },
    Conflict { kind: ConflictKind, message: String },
    Failed { error: ErrorInfo, message: String },
    NotRun,
    RolledBack { rows: u64 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct StatementOutcome {
    pub index: usize,
    pub sql: String,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApplyReport {
    pub committed: bool,
    pub outcomes: Vec<StatementOutcome>,
    pub error: Option<String>,
}

pub fn explain_error(e: &ErrorInfo) -> String {
    let base = match e.code.as_deref() {
        Some("23505") => "Duplicate value: another row already has this value in a unique column or key.",
        Some("23502") => "A required column (NOT NULL) has no value.",
        Some("23503") => "Foreign key: the row points to a row that does not exist, or other rows still point to it.",
        Some("23514") => "A CHECK constraint rejected the value.",
        Some("40001") => "Another transaction changed the same rows at the same time; nothing was saved. Reload and try again.",
        Some("40P01") => "Deadlock with another transaction; nothing was saved. Try again.",
        Some("25006") => "The server is read-only (a replica or standby): writes go to the primary.",
        Some("25001") => "This statement cannot run inside a transaction block.",
        Some("22P02") | Some("22007") | Some("22008") => "A value is not valid for its column's type.",
        Some("22001") => "A value is too long for its column.",
        Some("22003") => "A number is out of range for its column.",
        Some("42501") => "Permission denied.",
        Some("0A000") => "The server does not support this.",
        Some("57014") => "Cancelled.",
        Some("1062") => "Duplicate value: another row already has this value in a unique column or key.",
        Some("1048" | "1364") => "A required column (NOT NULL) has no value.",
        Some("1451" | "1452") => "Foreign key: the row points to a row that does not exist, or other rows still point to it.",
        Some("3819") => "A CHECK constraint rejected the value.",
        Some("1213") => "Deadlock with another transaction; nothing was saved. Try again.",
        Some("1205") => "Another transaction holds a lock on these rows; nothing was saved. Try again.",
        Some("1292" | "1366" | "1265") => "A value is not valid for its column's type.",
        Some("1406") => "A value is too long for its column.",
        Some("1264") => "A number is out of range for its column.",
        Some("1142" | "1143" | "1044") => "Permission denied.",
        Some("1792") => "The transaction is read-only.",
        Some("1290") => "The server is read-only: writes go to the primary.",
        Some("1317") => "Cancelled.",
        Some("SQLITE_CONSTRAINT_UNIQUE" | "SQLITE_CONSTRAINT_PRIMARYKEY") => "Duplicate value: another row already has this value in a unique column or key.",
        Some("SQLITE_CONSTRAINT_NOTNULL") => "A required column (NOT NULL) has no value.",
        Some("SQLITE_CONSTRAINT_FOREIGNKEY") => "Foreign key: the row points to a row that does not exist, or other rows still point to it.",
        Some("SQLITE_CONSTRAINT_CHECK") => "A CHECK constraint rejected the value.",
        Some("SQLITE_CONSTRAINT_DATATYPE" | "SQLITE_MISMATCH") => "A value is not valid for its column's type.",
        Some("SQLITE_BUSY" | "SQLITE_LOCKED") => "The database is locked: another tab or program holds a write transaction on this file; nothing was saved. Try again.",
        Some("SQLITE_READONLY") => "The file is read-only (or opened read-only): nothing was saved.",
        Some("SQLITE_INTERRUPT") => "Cancelled.",
        Some("DUCKDB_CONSTRAINT_UNIQUE") => "Duplicate value: another row already has this value in a unique column or key.",
        Some("DUCKDB_CONSTRAINT_NOTNULL") => "A required column (NOT NULL) has no value.",
        Some("DUCKDB_CONSTRAINT_FOREIGNKEY") => "Foreign key: the row points to a row that does not exist, or other rows still point to it.",
        Some("DUCKDB_CONSTRAINT_CHECK") => "A CHECK constraint rejected the value.",
        Some("TransactionContext Error") => "A conflict with another transaction (or a read-only one); nothing was saved. Try again.",
        Some("Conversion Error") => "A value is not valid for its column's type.",
        _ => "",
    };
    let mut s = String::new();
    if !base.is_empty() {
        s.push_str(base);
        s.push(' ');
    }
    s.push('(');
    s.push_str(&e.to_string().replace('\n', " "));
    s.push(')');
    s
}

fn cell_eq(a: &Value, b: Cell<'_>) -> bool {
    *a == Value::from(b)
}

impl Connection {
    pub fn apply_changes(self: &Arc<Self>, database: Option<&str>, changes: Vec<PlannedChange>) -> Task<ApplyReport> {
        let (this, db) = (self.clone(), database.map(str::to_string));
        self.spawn(async move {
            if this.read_only_for(db.as_deref().unwrap_or(this.database())) {
                let outcomes = changes.iter().enumerate().map(|(i, c)| StatementOutcome { index: i, sql: c.sql.clone(), outcome: Outcome::NotRun }).collect();
                return Ok(ApplyReport { committed: false, outcomes, error: Some("The database is read-only: nothing was saved.".into()) });
            }
            let d = this.dedicated(db.as_deref()).await?;
            let client = &d.db();
            let dialect = this.dialect();
            let mut outcomes: Vec<StatementOutcome> =
                changes.iter().enumerate().map(|(i, c)| StatementOutcome { index: i, sql: c.sql.clone(), outcome: Outcome::NotRun }).collect();
            if let Err(info) = client.execute(dialect.begin()).await {
                return Ok(ApplyReport { committed: false, outcomes, error: Some(explain_error(&info)) });
            }
            let mut failure: Option<String> = None;
            for (i, c) in changes.iter().enumerate() {
                if let Some(r) = &c.reread {
                    match crate::rows::typed_query(client, &r.sql, 2).await {
                        Err(info) => {
                            let message = explain_error(&info);
                            outcomes[i].outcome = Outcome::Failed { error: info, message: message.clone() };
                            failure = Some(message);
                            break;
                        }
                        Ok(rows) if rows.is_empty() => {
                            let message = format!("The row with {} is gone (deleted or changed by someone else).", c.key);
                            outcomes[i].outcome = Outcome::Conflict { kind: ConflictKind::RowGone, message: message.clone() };
                            failure = Some(message);
                            break;
                        }
                        Ok(rows) => {
                            let mismatch = r.expect.iter().enumerate().find(|(j, (_, v))| !cell_eq(v, rows.cell(0, *j)));
                            if let Some((j, (name, v))) = mismatch {
                                let kind = rows.columns[j].kind;
                                let found = rows.text(0, j);
                                let expected = match v {
                                    Value::Null => None,
                                    v => Some(v.display(kind)),
                                };
                                let message = format!(
                                    "Row with {}: {name} is now {} (the grid showed {}). Reload before saving.",
                                    c.key,
                                    found.clone().unwrap_or_else(|| "NULL".into()),
                                    expected.clone().unwrap_or_else(|| "NULL".into())
                                );
                                outcomes[i].outcome = Outcome::Conflict {
                                    kind: ConflictKind::Changed { column: Some(name.clone()), expected, found },
                                    message: message.clone(),
                                };
                                failure = Some(message);
                                break;
                            }
                        }
                    }
                }
                let n = match client.execute(&c.sql).await {
                    Ok(n) => n.unwrap_or(0),
                    Err(info) => {
                        let message = explain_error(&info);
                        outcomes[i].outcome = Outcome::Failed { error: info, message: message.clone() };
                        failure = Some(message);
                        break;
                    }
                };
                if c.kind != ChangeKind::Insert && n == 0 {
                    let exists = match &c.exists_sql {
                        Some(q) => first_ok(client, &[q]).await.map(|r| !r.is_empty()).unwrap_or(false),
                        None => false,
                    };
                    let (kind, message) = if exists {
                        (
                            ConflictKind::Changed { column: None, expected: None, found: None },
                            format!("The row with {} was changed by someone else since it was read. Reload before saving.", c.key),
                        )
                    } else {
                        (ConflictKind::RowGone, format!("The row with {} is gone (deleted or changed by someone else).", c.key))
                    };
                    outcomes[i].outcome = Outcome::Conflict { kind, message: message.clone() };
                    failure = Some(message);
                    break;
                }
                if c.kind != ChangeKind::Insert && n > 1 {
                    let message = format!("{} rows match {}: the key is not unique. Nothing was saved.", n, c.key);
                    outcomes[i].outcome = Outcome::Conflict { kind: ConflictKind::NotUnique(n), message: message.clone() };
                    failure = Some(message);
                    break;
                }
                outcomes[i].outcome = Outcome::Applied { rows: n };
            }
            let undo = |outcomes: &mut Vec<StatementOutcome>| {
                for o in outcomes.iter_mut() {
                    if let Outcome::Applied { rows } = o.outcome {
                        o.outcome = Outcome::RolledBack { rows };
                    }
                }
            };
            if let Some(f) = failure {
                let _ = client.execute(dialect.rollback()).await;
                undo(&mut outcomes);
                return Ok(ApplyReport { committed: false, outcomes, error: Some(f) });
            }
            match client.execute(dialect.commit()).await {
                Ok(_) => Ok(ApplyReport { committed: true, outcomes, error: None }),
                Err(info) => {
                    undo(&mut outcomes);
                    Ok(ApplyReport { committed: false, outcomes, error: Some(explain_error(&info)) })
                }
            }
        })
    }

    pub fn edit_target(self: &Arc<Self>, database: &str, rs: &Arc<ResultSet>, key: Option<Vec<String>>) -> Task<Editability> {
        let (this, db, rs) = (self.clone(), database.to_string(), rs.clone());
        self.spawn(async move { this.resolve_edit_target(&db, &rs, key).await })
    }

    async fn resolve_edit_target(self: &Arc<Self>, db: &str, rs: &ResultSet, key: Option<Vec<String>>) -> Result<Editability> {
        let ro = |s: &str| Ok(Editability::ReadOnly(s.to_string()));
        if self.read_only_for(db) {
            return ro("the database is read-only");
        }
        let Some(cols) = rs.columns() else { return ro("the statement has not returned columns yet") };
        if cols.is_empty() {
            return ro("the statement returned no rows to edit");
        }
        let kinds: Vec<Kind> = cols.iter().map(|c| c.kind).collect();
        let mut type_names: Vec<String> = cols.iter().map(|c| c.type_name.clone()).collect();
        // Only a plain SELECT from one table is edited, even when the server reports a single
        // origin table for every column: a subquery or a CTE can join a table with itself and
        // still show only its columns (a worker's id beside the manager's email), and an edit
        // keyed by the worker's id would write the manager's values into the worker's row. When
        // the statement cannot be analyzed, for whatever reason, the result is read-only.
        let dialect = self.dialect();
        let shape = match analyze_select_in(&rs.sql, dialect) {
            Ok(s) => s,
            Err(reason) => return Ok(Editability::ReadOnly(reason)),
        };
        let check = if self.info.is_lar { ConflictCheck::Reread } else { ConflictCheck::Predicate };
        let client = self.meta(db).await?;
        if !self.info.is_lar && dialect == Dialect::Postgres {
            resolve_base_types(&client, &cols, &mut type_names).await;
        }
        if let Some(named) = rs.named_origins() {
            let tables: BTreeSet<(&str, &str)> = named.iter().flatten().map(|o| (o.schema.as_str(), o.table.as_str())).collect();
            if tables.len() > 1 {
                return ro("the result mixes columns of several tables (a join): edit the tables one at a time");
            }
        }

        let oids: BTreeSet<u32> = rs.origins().map(|o| o.iter().filter_map(|x| x.table_oid).collect()).unwrap_or_default();
        if oids.len() > 1 {
            return ro("the result mixes columns of several tables (a join): edit the tables one at a time");
        }
        let mut generated = vec![false; cols.len()];
        let (table, columns, oid, not_null) = if let Some(&oid) = oids.iter().next() {
            let rel = first_ok(
                &client,
                &[&format!(
                    "SELECT n.nspname, c.relname, c.relkind FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = {oid}"
                )],
            )
            .await?;
            let Some(r) = rel.first() else { return ro("the table of this result is not in the catalog") };
            if !matches!(r.get(2).cloned().flatten().as_deref(), Some("r" | "p" | "f")) {
                return ro("the result comes from a view, which is not edited directly");
            }
            let (nsp, relname) = (r.first().cloned().flatten().unwrap_or_default(), r.get(1).cloned().flatten().unwrap_or_default());
            if relname != shape.table.name || shape.table.schema.as_ref().is_some_and(|s| *s != nsp) {
                return Ok(Editability::ReadOnly(format!(
                    "the result's columns come from {}, not from {}",
                    qualified(Some(&nsp), &relname),
                    shape.table.sql()
                )));
            }
            let atts = first_ok(
                &client,
                &[
                    &format!("SELECT attnum, attname, attnotnull FROM pg_catalog.pg_attribute WHERE attrelid = {oid} AND attnum > 0"),
                    &format!("SELECT attnum, attname FROM pg_catalog.pg_attribute WHERE attrelid = {oid} AND attnum > 0"),
                ],
            )
            .await?;
            let origins = rs.origins().unwrap_or_default();
            let att = |i: usize| {
                let n = origins.get(i)?.column_id?;
                atts.iter().find(|a| a.first().cloned().flatten() == Some(n.to_string()))
            };
            let columns: Vec<Option<String>> = (0..cols.len()).map(|i| att(i).and_then(|a| a.get(1).cloned().flatten())).collect();
            let not_null: Vec<bool> = (0..cols.len()).map(|i| att(i).and_then(|a| a.get(2).cloned().flatten()).as_deref() == Some("t")).collect();
            let table = TableRef { schema: r.first().cloned().flatten(), name: r.get(1).cloned().flatten().unwrap_or_default() };
            (table, columns, Some(oid), not_null)
        } else {
            // A table created since its schema was read (DDL in a tab, another client) is not in
            // the cache yet: one more read before it counts as missing.
            let mut fresh = false;
            let rel = loop {
                let schema = match &shape.table.schema {
                    Some(s) => Some(s.clone()),
                    None => {
                        let mut found = None;
                        for s in self.schema.search_path(db) {
                            if let Ok(rels) = self.relations(db, &s).await
                                && rels.iter().any(|r| same_name(dialect, &r.name, &shape.table.name))
                            {
                                found = Some(s);
                                break;
                            }
                        }
                        found
                    }
                };
                let rel = match &schema {
                    Some(s) => self.relations(db, s).await?.into_iter().find(|r| same_name(dialect, &r.name, &shape.table.name)).map(|r| (s.clone(), r)),
                    None => None,
                };
                if rel.is_some() || fresh {
                    break rel;
                }
                fresh = true;
                match &shape.table.schema {
                    Some(s) => self.schema.invalidate_schema(db, s),
                    None => {
                        for s in self.schema.search_path(db) {
                            self.schema.invalidate_schema(db, &s);
                        }
                    }
                }
            };
            let (schema, rel) = match rel {
                Some((s, r)) => (s, Some(r)),
                None => return Ok(Editability::ReadOnly(format!("table {} was not found", shape.table.name))),
            };
            let Some(rel) = rel else { return Ok(Editability::ReadOnly(format!("table {} was not found", shape.table.name))) };
            if rel.kind.is_view() {
                return ro("the result comes from a view, which is not edited directly");
            }
            let tcols_full = self.columns(db, &schema, &rel.name).await?;
            let tcols: Vec<String> = tcols_full.iter().map(|c| c.name.clone()).collect();
            let columns = match map_columns(&shape.items, &tcols, &cols.iter().map(|c| c.name.clone()).collect::<Vec<_>>()) {
                Ok(c) => c,
                Err(reason) => return Ok(Editability::ReadOnly(reason)),
            };
            if dialect == Dialect::TSql {
                let own = ms_generated(&client, &schema, &rel.name).await;
                generated = columns.iter().map(|c| c.as_ref().is_some_and(|n| own.contains(n))).collect();
            }
            // SQLite's and DuckDB's generated columns are read-only.
            if matches!(dialect, Dialect::Sqlite | Dialect::DuckDb) {
                generated = columns
                    .iter()
                    .map(|c| c.as_ref().and_then(|n| tcols_full.iter().find(|t| &t.name == n)).is_some_and(|t| t.default.as_deref().is_some_and(|d| d.starts_with("GENERATED"))))
                    .collect();
            }
            let not_null = columns
                .iter()
                .map(|c| c.as_ref().and_then(|n| tcols_full.iter().find(|t| &t.name == n)).is_some_and(|t| t.nullable == Some(false)))
                .collect();
            (TableRef { schema: Some(schema), name: rel.name.clone() }, columns, rel.oid, not_null)
        };
        let mut columns = columns;
        let mut seen = BTreeSet::new();
        for c in columns.iter_mut() {
            if let Some(n) = c
                && !seen.insert(n.clone())
            {
                *c = None;
            }
        }
        let position = |name: &str| columns.iter().position(|c| c.as_deref() == Some(name));

        let (names, source) = if let Some(k) = key {
            if k.is_empty() {
                return ro("no key column given");
            }
            (k, KeySource::Explicit)
        } else if self.info.is_lar {
            let int = |n: &str| {
                position(n).filter(|&i| kinds[i] == Kind::Int).map(|_| n.to_string())
            };
            let guess = int("id").or_else(|| int(&format!("{}_id", table.name))).or_else(|| {
                let t = table.name.trim_end_matches('s');
                int(&format!("{t}_id"))
            });
            match guess {
                Some(g) => (vec![g], KeySource::Guessed),
                None => {
                    return Ok(Editability::ReadOnly(format!(
                        "LAR does not report primary keys: no integer `id` column in the result to use as the key of {} (choose the key column)",
                        table.name
                    )));
                }
            }
        } else {
            let keys = if dialect == Dialect::MySql {
                my_key(&client, table.schema.as_deref().unwrap_or(db), &table.name).await
            } else if dialect == Dialect::TSql {
                ms_key(&client, table.schema.as_deref().unwrap_or("dbo"), &table.name).await
            } else if dialect == Dialect::Sqlite {
                lite_key(&client, table.schema.as_deref().unwrap_or("main"), &table.name).await
            } else if dialect == Dialect::DuckDb {
                duck_key(&client, table.schema.as_deref().unwrap_or("main"), &table.name).await
            } else {
                let Some(oid) = oid else { return ro("the table's catalog entry is unavailable, so its key is unknown") };
                pg_key(&client, oid).await
            };
            match keys {
                Ok(keys) => {
                    let mut chosen = None;
                    let mut missing = None;
                    for (primary, names) in keys {
                        if names.iter().all(|n| position(n).is_some()) {
                            chosen = Some((names, if primary { KeySource::PrimaryKey } else { KeySource::UniqueIndex }));
                            break;
                        }
                        missing.get_or_insert(names);
                    }
                    match (chosen, missing) {
                        (Some(c), _) => c,
                        (None, Some(m)) => {
                            return Ok(Editability::ReadOnly(format!(
                                "the key of {} ({}) is not in the result: add it to the SELECT to edit",
                                table.name,
                                m.join(", ")
                            )));
                        }
                        (None, None) => {
                            return Ok(Editability::ReadOnly(format!(
                                "{} has no primary key or unique NOT NULL index, so rows cannot be told apart",
                                table.name
                            )));
                        }
                    }
                }
                Err(e) => return Ok(Editability::ReadOnly(format!("the key of {} could not be read: {e}", table.name))),
            }
        };
        let mut key = Vec::new();
        for n in &names {
            match position(n) {
                Some(i) => key.push(KeyColumn { name: n.clone(), result_col: i }),
                None => return Ok(Editability::ReadOnly(format!("key column {n} is not in the result: add it to the SELECT to edit"))),
            }
        }
        Ok(Editability::Editable(EditTarget { table, key, key_source: source, columns, kinds, type_names, check, not_null, dialect: self.dialect(), generated }))
    }
}

async fn pg_key(client: &Db, oid: u32) -> Result<Vec<(bool, Vec<String>)>> {
    let idx = first_ok(
        client,
        &[&format!(
            "SELECT i.indisprimary, i.indkey FROM pg_catalog.pg_index i WHERE i.indrelid = {oid} AND i.indisunique \
             AND i.indpred IS NULL AND i.indexprs IS NULL ORDER BY i.indisprimary DESC, i.indnatts"
        )],
    )
    .await?;
    let atts = first_ok(
        client,
        &[&format!("SELECT attnum, attname, attnotnull FROM pg_catalog.pg_attribute WHERE attrelid = {oid} AND attnum > 0")],
    )
    .await?;
    let mut out = Vec::new();
    for r in idx {
        let primary = r.first().cloned().flatten().as_deref() == Some("t");
        let nums: Vec<String> = r.get(1).cloned().flatten().unwrap_or_default().split_whitespace().map(str::to_string).collect();
        let mut names = Vec::new();
        let mut ok = !nums.is_empty();
        for n in &nums {
            match atts.iter().find(|a| a.first().cloned().flatten().as_deref() == Some(n)) {
                Some(a) if primary || a.get(2).cloned().flatten().as_deref() == Some("t") => {
                    names.push(a.get(1).cloned().flatten().unwrap_or_default())
                }
                _ => ok = false,
            }
        }
        if ok {
            out.push((primary, names));
        }
    }
    Ok(out)
}

async fn my_key(client: &Db, schema: &str, table: &str) -> Result<Vec<(bool, Vec<String>)>> {
    use crate::schema::my::lit;
    let rows = first_ok(
        client,
        &[&format!(
            "SELECT s.INDEX_NAME, s.COLUMN_NAME, c.IS_NULLABLE FROM information_schema.STATISTICS s \
             LEFT JOIN information_schema.COLUMNS c ON c.TABLE_SCHEMA = s.TABLE_SCHEMA AND c.TABLE_NAME = s.TABLE_NAME AND c.COLUMN_NAME = s.COLUMN_NAME \
             WHERE s.TABLE_SCHEMA = {} AND s.TABLE_NAME = {} AND s.NON_UNIQUE = 0 \
             ORDER BY s.INDEX_NAME = 'PRIMARY' DESC, s.INDEX_NAME, s.SEQ_IN_INDEX",
            lit(schema),
            lit(table)
        )],
    )
    .await?;
    let mut out: Vec<(bool, Vec<String>, bool)> = Vec::new();
    let mut last: Option<String> = None;
    for r in rows {
        let get = |i: usize| r.get(i).cloned().flatten();
        let Some(index) = get(0) else { continue };
        let col = get(1);
        let usable = col.is_some() && (index == "PRIMARY" || get(2).as_deref() == Some("NO"));
        if last.as_deref() != Some(&index) {
            out.push((index == "PRIMARY", Vec::new(), true));
            last = Some(index);
        }
        let e = out.last_mut().expect("pushed");
        e.2 &= usable;
        if let Some(c) = col {
            e.1.push(c);
        }
    }
    let mut keys: Vec<(bool, Vec<String>)> = out.into_iter().filter(|(_, c, ok)| *ok && !c.is_empty()).map(|(p, c, _)| (p, c)).collect();
    keys.sort_by_key(|(p, c)| (!*p, c.len()));
    Ok(keys)
}

/// SQLite and DuckDB compare names whatever their case.
fn same_name(d: Dialect, a: &str, b: &str) -> bool {
    if d.names_ignore_case() { a.eq_ignore_ascii_case(b) } else { a == b }
}

/// The primary key, then unique indexes whose columns are all NOT NULL (no partial or
/// expression index). A rowid table without a declared key has none (its `rowid` is not used).
async fn lite_key(client: &Db, schema: &str, table: &str) -> Result<Vec<(bool, Vec<String>)>> {
    use crate::schema::lite::lit;
    let (t, s) = (lit(table), lit(schema));
    let pk: Vec<String> = first_ok(client, &[&format!("SELECT name FROM pragma_table_info({t}, {s}) WHERE pk > 0 ORDER BY pk")])
        .await?
        .into_iter()
        .filter_map(|r| r.into_iter().next().flatten())
        .collect();
    let rows = first_ok(
        client,
        &[&format!(
            "SELECT il.name, ii.name, ti.\"notnull\" FROM pragma_index_list({t}, {s}) il JOIN pragma_index_info(il.name, {s}) ii \
             LEFT JOIN pragma_table_info({t}, {s}) ti ON ti.name = ii.name \
             WHERE il.\"unique\" = 1 AND il.partial = 0 AND il.origin <> 'pk' ORDER BY il.name, ii.seqno"
        )],
    )
    .await?;
    let mut uniques: Vec<(String, Vec<String>, bool)> = Vec::new();
    for r in rows {
        let get = |i: usize| r.get(i).cloned().flatten();
        let Some(index) = get(0) else { continue };
        if uniques.last().is_none_or(|u| u.0 != index) {
            uniques.push((index, Vec::new(), true));
        }
        let e = uniques.last_mut().expect("pushed");
        match get(1) {
            Some(c) if get(2).as_deref() == Some("1") => e.1.push(c),
            _ => e.2 = false,
        }
    }
    let mut keys: Vec<(bool, Vec<String>)> = Vec::new();
    if !pk.is_empty() {
        keys.push((true, pk));
    }
    let mut u: Vec<Vec<String>> = uniques.into_iter().filter(|(_, c, ok)| *ok && !c.is_empty()).map(|(_, c, _)| c).collect();
    u.sort_by_key(Vec::len);
    keys.extend(u.into_iter().map(|c| (false, c)));
    Ok(keys)
}

/// The primary key, then UNIQUE constraints whose columns are all NOT NULL.
async fn duck_key(client: &Db, schema: &str, table: &str) -> Result<Vec<(bool, Vec<String>)>> {
    let rows = first_ok(client, &[&crate::schema::duck::keys_sql("", schema, table)]).await?;
    Ok(rows
        .into_iter()
        .filter(|r| r.get(2).cloned().flatten().as_deref() == Some("true"))
        .filter_map(|r| {
            let primary = r.first().cloned().flatten().as_deref() == Some("true");
            let cols: Vec<String> = r.get(1).cloned().flatten()?.split('\u{1f}').map(str::to_string).collect();
            Some((primary, cols))
        })
        .collect())
}

async fn ms_generated(client: &Db, schema: &str, table: &str) -> BTreeSet<String> {
    let q = format!(
        "SELECT name FROM sys.columns WHERE object_id = {} AND (is_identity = 1 OR is_computed = 1 OR system_type_id = 189)",
        crate::schema::ms::object_id(schema, table)
    );
    first_ok(client, &[&q]).await.map(|rows| rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect()).unwrap_or_default()
}

async fn ms_key(client: &Db, schema: &str, table: &str) -> Result<Vec<(bool, Vec<String>)>> {
    let rows = first_ok(
        client,
        &[&format!(
            "SELECT i.index_id, i.is_primary_key, c.name, c.is_nullable FROM sys.indexes i \
               JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
               JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
              WHERE i.object_id = {} AND i.is_unique = 1 AND i.has_filter = 0 AND ic.is_included_column = 0 \
              ORDER BY i.is_primary_key DESC, i.index_id, ic.key_ordinal",
            crate::schema::ms::object_id(schema, table)
        )],
    )
    .await?;
    let mut out: Vec<(String, bool, Vec<String>, bool)> = Vec::new();
    for r in rows {
        let get = |i: usize| r.get(i).cloned().flatten();
        let (Some(id), Some(col)) = (get(0), get(2)) else { continue };
        if out.last().is_none_or(|(last, ..)| *last != id) {
            out.push((id.clone(), get(1).as_deref() == Some("1"), Vec::new(), true));
        }
        let e = out.last_mut().expect("pushed");
        e.3 &= e.1 || get(3).as_deref() == Some("0");
        e.2.push(col);
    }
    let mut keys: Vec<(bool, Vec<String>)> = out.into_iter().filter(|(_, _, c, ok)| *ok && !c.is_empty()).map(|(_, p, c, _)| (p, c)).collect();
    keys.sort_by_key(|(p, c)| (!*p, c.len()));
    Ok(keys)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SelectItem {
    Star,
    Column(String),
    Expr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SelectShape {
    pub table: TableRef,
    pub alias: Option<String>,
    pub items: Vec<SelectItem>,
}

#[cfg(test)]
pub(crate) fn analyze_select(sql: &str) -> Result<SelectShape, String> {
    analyze_select_in(sql, Dialect::Postgres)
}

pub(crate) fn analyze_select_in(sql: &str, d: Dialect) -> Result<SelectShape, String> {
    let ident_value = |sql: &str, t: &Token| ident_value_in(sql, t, d);
    let toks: Vec<Token> = tokenize_in(sql, d).into_iter().filter(|t| !t.kind.is_trivia()).collect();
    let word = |t: &Token, w: &str| matches!(t.kind, TokenKind::Keyword | TokenKind::Ident) && t.text(sql).eq_ignore_ascii_case(w);
    let punct = |t: &Token, p: &str| t.kind == TokenKind::Punct && t.text(sql) == p;
    let is_name = |t: &Token| matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent);
    let mut toks = toks;
    if toks.last().is_some_and(|t| punct(t, ";")) {
        toks.pop();
    }
    if toks.windows(2).any(|w| word(&w[0], "SYSTEM_TIME") || (word(&w[0], "AS") && word(&w[1], "OF"))) {
        return Err("a result read AS OF a past instant is read-only".into());
    }
    const CLAUSES: &[&str] = &["WHERE", "ORDER", "LIMIT", "OFFSET", "FETCH", "FOR"];
    if toks.first().is_some_and(|t| word(t, "TABLE")) {
        let mut i = 1;
        if toks.get(i).is_some_and(|t| word(t, "ONLY")) {
            i += 1;
        }
        let Some(first) = toks.get(i).filter(|t| is_name(t)) else { return Err("the result does not come from a table".into()) };
        let mut table = TableRef { schema: None, name: ident_value(sql, first) };
        i += 1;
        if toks.get(i).is_some_and(|t| punct(t, ".")) && toks.get(i + 1).is_some_and(|t| is_name(t)) {
            table = TableRef { schema: Some(table.name), name: ident_value(sql, &toks[i + 1]) };
            i += 2;
        }
        if let Some(t) = toks.get(i)
            && !["ORDER", "LIMIT", "OFFSET", "FETCH"].iter().any(|c| word(t, c))
        {
            return Err("the statement is not a simple query of one table".into());
        }
        return Ok(SelectShape { table, alias: None, items: vec![SelectItem::Star] });
    }
    if toks.first().is_some_and(|t| word(t, "WITH")) {
        return Err("a result read through a WITH query (CTE) is read-only: edit the table with a plain SELECT".into());
    }
    if !toks.first().is_some_and(|t| word(t, "SELECT")) {
        return Err("only the results of a SELECT can be edited".into());
    }
    let mut depth = 0;
    let mut top: Vec<(usize, Token)> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if punct(t, "(") || punct(t, "[") {
            if depth == 0 {
                top.push((i, *t));
            }
            depth += 1;
            continue;
        }
        if punct(t, ")") || punct(t, "]") {
            depth -= 1;
            continue;
        }
        if depth == 0 {
            top.push((i, *t));
        }
    }
    for (_, t) in &top {
        for (w, why) in [
            ("JOIN", "the result joins several tables"),
            ("GROUP", "a grouped result (GROUP BY) does not map to table rows"),
            ("HAVING", "a grouped result (GROUP BY) does not map to table rows"),
            ("UNION", "a UNION does not map to table rows"),
            ("INTERSECT", "an INTERSECT does not map to table rows"),
            ("EXCEPT", "an EXCEPT does not map to table rows"),
            ("DISTINCT", "a DISTINCT result does not map to table rows"),
            ("WINDOW", "window results are read-only"),
        ] {
            if word(t, w) {
                return Err(why.into());
            }
        }
    }
    let from = top.iter().position(|(_, t)| word(t, "FROM")).ok_or("the SELECT has no FROM")?;
    let (sel_i, from_i) = (top[0].0, top[from].0);
    let mut first_item = sel_i + 1;
    if toks.get(first_item).is_some_and(|t| word(t, "TOP")) {
        first_item += 1;
        if toks.get(first_item).is_some_and(|t| punct(t, "(")) {
            while first_item < from_i && !punct(&toks[first_item], ")") {
                first_item += 1;
            }
        }
        first_item += 1;
        if toks.get(first_item).is_some_and(|t| word(t, "PERCENT")) {
            first_item += 1;
        }
        if toks.get(first_item).is_some_and(|t| word(t, "WITH")) && toks.get(first_item + 1).is_some_and(|t| word(t, "TIES")) {
            first_item += 2;
        }
    }
    let mut items = Vec::new();
    let mut cur: Vec<Token> = Vec::new();
    let mut d = 0;
    for t in &toks[first_item.min(from_i)..from_i] {
        if punct(t, "(") {
            d += 1;
        } else if punct(t, ")") {
            d -= 1;
        }
        if d == 0 && punct(t, ",") {
            items.push(std::mem::take(&mut cur));
        } else {
            cur.push(*t);
        }
    }
    items.push(cur);
    let items: Vec<SelectItem> = items
        .iter()
        .map(|it| {
            let it: &[Token] = it;
            let col_part = |it: &[Token]| -> Option<String> {
                match it {
                    [n] if is_name(n) => Some(ident_value(sql, n)),
                    [q, dot, n] if is_name(q) && punct(dot, ".") && is_name(n) => Some(ident_value(sql, n)),
                    _ => None,
                }
            };
            let star = |t: &Token| t.kind == TokenKind::Operator && t.text(sql) == "*";
            match it {
                [s] if star(s) => SelectItem::Star,
                [q, dot, s] if is_name(q) && punct(dot, ".") && star(s) => SelectItem::Star,
                _ => {
                    let n = it.len();
                    if let Some(c) = col_part(it) {
                        return SelectItem::Column(c);
                    }
                    if n >= 2 && is_name(&it[n - 1]) {
                        let base = if n >= 3 && word(&it[n - 2], "AS") { &it[..n - 2] } else { &it[..n - 1] };
                        if let Some(c) = col_part(base) {
                            return SelectItem::Column(c);
                        }
                    }
                    SelectItem::Expr
                }
            }
        })
        .collect();
    let rest: Vec<Token> = toks[from_i + 1..].to_vec();
    let Some(first) = rest.first() else { return Err("the SELECT has no table".into()) };
    if punct(first, "(") {
        return Err("the result comes from a subquery".into());
    }
    if !is_name(first) {
        return Err("the result does not come from a table".into());
    }
    let mut i = 1;
    let mut table = TableRef { schema: None, name: ident_value(sql, first) };
    if rest.len() >= 3 && punct(&rest[1], ".") && is_name(&rest[2]) {
        table = TableRef { schema: Some(table.name), name: ident_value(sql, &rest[2]) };
        i = 3;
    }
    if rest.get(i).is_some_and(|t| punct(t, "(")) {
        return Err("the result comes from a function, not a table".into());
    }
    let mut alias = None;
    if rest.get(i).is_some_and(|t| word(t, "AS")) {
        i += 1;
    }
    if let Some(t) = rest.get(i)
        && is_name(t)
        && !CLAUSES.iter().any(|c| word(t, c))
    {
        alias = Some(ident_value(sql, t));
        i += 1;
    }
    if let Some(t) = rest.get(i) {
        if punct(t, ",") {
            return Err("the result reads several tables".into());
        }
        if !CLAUSES.iter().any(|c| word(t, c)) {
            return Err("the statement is not a simple SELECT from one table".into());
        }
    }
    Ok(SelectShape { table, alias, items })
}

/// `*` expands to the schema cache's columns, which may be stale: every expanded name must
/// equal the result column at its position. Aliases make names unreliable, so only a lone `*`
/// falls back to names.
pub(crate) fn map_columns(items: &[SelectItem], table_cols: &[String], result_cols: &[String]) -> Result<Vec<Option<String>>, String> {
    let mut out = Vec::new();
    let mut aligned = true;
    for it in items {
        match it {
            SelectItem::Star => {
                for t in table_cols {
                    aligned &= result_cols.get(out.len()) == Some(t);
                    out.push(Some(t.clone()));
                }
            }
            SelectItem::Column(c) => out.push(table_cols.iter().find(|t| *t == c).cloned()),
            SelectItem::Expr => out.push(None),
        }
    }
    if aligned && out.len() == result_cols.len() {
        return Ok(out);
    }
    if items == [SelectItem::Star] {
        return Ok(result_cols.iter().map(|r| table_cols.iter().find(|t| *t == r).cloned()).collect());
    }
    Err("the result's columns do not match the table's as last read (refresh the schema to edit)".into())
}

async fn resolve_base_types(client: &Db, cols: &[crate::result::ColumnInfo], type_names: &mut [String]) {
    let oids: BTreeSet<u32> = cols.iter().map(|c| c.type_oid).filter(|o| *o != 0).collect();
    if oids.is_empty() {
        return;
    }
    let list = oids.iter().map(|o| o.to_string()).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "WITH RECURSIVE r(top, cur, depth) AS ( \
           SELECT t.oid, t.oid, 0 FROM pg_catalog.pg_type t WHERE t.oid IN ({list}) \
           UNION ALL \
           SELECT r.top, CASE WHEN t.typtype = 'd' THEN t.typbasetype ELSE t.typelem END, r.depth + 1 \
           FROM r JOIN pg_catalog.pg_type t ON t.oid = r.cur \
           WHERE (t.typtype = 'd' OR (t.typcategory = 'A' AND t.typelem <> 0)) AND r.depth < 16) \
         SELECT DISTINCT ON (r.top) r.top, t.typname FROM r JOIN pg_catalog.pg_type t ON t.oid = r.cur \
         ORDER BY r.top, r.depth DESC"
    );
    let Ok(rows) = first_ok(client, &[&sql]).await else { return };
    let base: BTreeMap<u32, String> = rows
        .into_iter()
        .filter_map(|r| Some((r.first().cloned().flatten()?.parse().ok()?, r.get(1).cloned().flatten()?)))
        .collect();
    for (c, name) in cols.iter().zip(type_names.iter_mut()) {
        if let Some(b) = base.get(&c.type_oid)
            && !comparable(Dialect::Postgres, b)
        {
            *name = b.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::{ChunkBuilder, ColumnInfo};

    #[test]
    fn simple_selects() {
        let s = analyze_select("SELECT id, name AS n, upper(x), p.*, \"Total\" FROM sales.orders p WHERE id > 3 ORDER BY id LIMIT 10;").unwrap();
        assert_eq!(s.table, TableRef { schema: Some("sales".into()), name: "orders".into() });
        assert_eq!(s.alias.as_deref(), Some("p"));
        assert_eq!(
            s.items,
            vec![
                SelectItem::Column("id".into()),
                SelectItem::Column("name".into()),
                SelectItem::Expr,
                SelectItem::Star,
                SelectItem::Column("Total".into())
            ]
        );
        assert_eq!(analyze_select("select * from t").unwrap().items, vec![SelectItem::Star]);
        for (sql, why) in [
            ("SELECT * FROM a JOIN b ON a.id = b.a", "joins"),
            ("SELECT * FROM a, b", "several"),
            ("SELECT count(*) FROM a GROUP BY x", "GROUP"),
            ("SELECT DISTINCT x FROM a", "DISTINCT"),
            ("SELECT * FROM (SELECT 1) s", "subquery"),
            ("SELECT * FROM generate_series(1, 3)", "function"),
            ("SELECT * FROM t FOR SYSTEM_TIME AS OF 5", "AS OF"),
            ("UPDATE t SET x = 1", "SELECT"),
            ("SELECT 1", "FROM"),
        ] {
            let e = analyze_select(sql).unwrap_err();
            assert!(e.contains(why), "{sql}: {e}");
        }
        assert!(analyze_select("SELECT * FROM t WHERE id IN (SELECT a FROM b JOIN c ON true)").is_ok());
    }

    #[test]
    fn subqueries_and_ctes_are_read_only() {
        for sql in [
            "SELECT * FROM (SELECT w.id, w.name, m.email FROM emp w JOIN emp m ON m.id = w.manager) x",
            "WITH x AS (SELECT w.id, w.name, m.email FROM emp w JOIN emp m ON m.id = w.manager) SELECT * FROM x",
            "WITH x AS (SELECT 1) SELECT * FROM emp",
            "SELECT * FROM emp TABLESAMPLE SYSTEM (10)",
            "(SELECT * FROM emp)",
        ] {
            assert!(analyze_select(sql).is_err(), "{sql}");
        }
        let s = analyze_select("TABLE s.emp ORDER BY 1").unwrap();
        assert_eq!(s.table, TableRef { schema: Some("s".into()), name: "emp".into() });
        assert_eq!(s.items, vec![SelectItem::Star]);
        assert!(analyze_select("TABLE emp UNION TABLE emp2").is_err());
    }

    #[test]
    fn column_mapping() {
        let t = vec!["id".to_string(), "name".into(), "total".into()];
        let items = vec![SelectItem::Column("name".into()), SelectItem::Expr, SelectItem::Star];
        let r: Vec<String> = ["n", "x", "id", "name", "total"].iter().map(|s| s.to_string()).collect();
        assert_eq!(map_columns(&items, &t, &r).unwrap(), vec![Some("name".into()), None, Some("id".into()), Some("name".into()), Some("total".into())]);
        let r: Vec<String> = ["id", "zzz"].iter().map(|s| s.to_string()).collect();
        assert!(map_columns(&items, &t, &r).is_err());
        let stale = vec!["id".to_string(), "total".into(), "name".into()];
        let r: Vec<String> = ["n", "x", "id", "name", "total"].iter().map(|s| s.to_string()).collect();
        assert!(map_columns(&items, &stale, &r).is_err());
        let r: Vec<String> = ["id", "name", "total"].iter().map(|s| s.to_string()).collect();
        assert_eq!(map_columns(&[SelectItem::Star], &stale, &r).unwrap(), vec![Some("id".into()), Some("name".into()), Some("total".into())]);
        let r: Vec<String> = ["id", "name", "total", "fresh"].iter().map(|s| s.to_string()).collect();
        assert_eq!(map_columns(&[SelectItem::Star], &t, &r).unwrap()[3], None);
    }

    #[test]
    fn comparable_types() {
        let pg = Dialect::Postgres;
        assert!(comparable(pg, "int4") && comparable(pg, "_int4") && comparable(pg, "jsonb") && comparable(pg, "_text"));
        for t in ["json", "_json", "xml", "_xml", "point", "_point", "_box"] {
            assert!(!comparable(pg, t), "{t}");
        }
        assert!(!delete_checkable(pg, Kind::Other, "money"));
        assert!(delete_checkable(pg, Kind::Text, "varchar"));
        assert!(!delete_checkable(pg, Kind::Json, "json"));
        assert!(delete_checkable(pg, Kind::Json, "jsonb"));
        let ms = Dialect::TSql;
        assert!(comparable(ms, "nvarchar") && comparable(ms, "money") && comparable(ms, "datetime"));
        for t in ["text", "ntext", "xml", "udt", "float", "datetime2"] {
            assert!(!comparable(ms, t), "{t}");
        }
    }

    #[test]
    fn float4_keys_are_cast() {
        let pg = Dialect::Postgres;
        assert_eq!(key_literal(pg, Kind::Float4, Cell::Float(0.1f32 as f64)), "0.1::real");
        assert_eq!(key_literal(pg, Kind::Float4, Cell::Float(f64::NAN)), "'NaN'::real");
        assert_eq!(key_literal(pg, Kind::Float, Cell::Float(0.1)), "0.1");
        assert_eq!(key_literal(pg, Kind::Int, Cell::Int(3)), "3");
        assert_eq!(key_literal(pg, Kind::Float4, Cell::Null), "NULL");
        assert_eq!(key_literal(Dialect::MySql, Kind::Text, Cell::Text("a\\b")), "'a\\\\b'");
    }

    fn target(check: ConflictCheck) -> (EditTarget, Snapshot) {
        let c = |n: &str, t: &str, k| ColumnInfo { name: n.into(), type_name: t.into(), type_oid: 0, kind: k };
        let cols = vec![c("id", "int8", Kind::Int), c("name", "text", Kind::Text), c("total", "float8", Kind::Float), c("x", "int8", Kind::Int)];
        let rs = ResultSet::new("select");
        rs.set_columns(cols.clone());
        let mut b = ChunkBuilder::new(&cols);
        for (id, name) in [(10, Some("a'b")), (11, None)] {
            b.push_i64(0, id);
            match name {
                Some(n) => b.push_text(1, n),
                None => b.push_null(1),
            }
            b.push_f64(2, 0.1);
            b.push_i64(3, 1);
            b.end_row();
        }
        rs.push_chunk(b.finish());
        let t = EditTarget {
            table: TableRef { schema: Some("public".into()), name: "Orders".into() },
            key: vec![KeyColumn { name: "id".into(), result_col: 0 }],
            key_source: KeySource::PrimaryKey,
            columns: vec![Some("id".into()), Some("name".into()), Some("total".into()), None],
            kinds: cols.iter().map(|c| c.kind).collect(),
            type_names: cols.iter().map(|c| c.type_name.clone()).collect(),
            check,
            not_null: vec![true, false, false, false],
            dialect: Dialect::Postgres,
            generated: vec![false; 4],
        };
        (t, rs.snapshot())
    }

    #[test]
    fn change_set_and_plan() {
        let (t, snap) = target(ConflictCheck::Predicate);
        let mut cs = ChangeSet::new();
        cs.edit(0, 1, CellValue::text("a'b"), CellValue::text("fresh"));
        cs.edit(0, 2, CellValue::text("0.1"), CellValue::text("2.5"));
        cs.edit(1, 1, CellValue::Null, CellValue::text("x"));
        cs.edit(1, 1, CellValue::text("x"), CellValue::Null);
        assert_eq!(cs.counts(), (1, 0, 0));
        let i = cs.insert_row(4);
        cs.set_inserted(i, 0, Some(CellValue::text("12")));
        cs.set_inserted(i, 1, Some(CellValue::Null));
        cs.delete_row(1);
        let plan = cs.plan(&t, &snap).unwrap();
        let sqls: Vec<&str> = plan.iter().map(|p| p.sql.as_str()).collect();
        assert_eq!(
            sqls,
            vec![
                "UPDATE public.\"Orders\" SET name = 'fresh', total = 2.5 WHERE id = 10 AND name IS NOT DISTINCT FROM 'a''b' AND total IS NOT DISTINCT FROM '0.1'",
                "INSERT INTO public.\"Orders\" (id, name) VALUES (12, NULL)",
                "DELETE FROM public.\"Orders\" WHERE id = 11 AND name IS NULL AND total IS NOT DISTINCT FROM '0.1'",
            ]
        );
        assert_eq!(plan[2].exists_sql.as_deref(), Some("SELECT 1 FROM public.\"Orders\" WHERE id = 11"));
        assert!(preview(&plan).ends_with("total IS NOT DISTINCT FROM '0.1';\n"));
        let mut t2 = t.clone();
        t2.type_names = vec!["json".into(); t2.type_names.len()];
        assert_eq!(cs.plan(&t2, &snap).unwrap()[2].sql, "DELETE FROM public.\"Orders\" WHERE id = 11");
        let mut bad = ChangeSet::new();
        bad.edit(0, 3, CellValue::text("1"), CellValue::text("2"));
        assert!(bad.plan(&t, &snap).unwrap_err().contains("computed"));
    }

    #[test]
    fn lar_plan_rereads() {
        let (t, snap) = target(ConflictCheck::Reread);
        let mut cs = ChangeSet::new();
        cs.edit(1, 1, CellValue::Null, CellValue::text("y"));
        let plan = cs.plan(&t, &snap).unwrap();
        assert_eq!(plan[0].sql, "UPDATE public.\"Orders\" SET name = 'y' WHERE id = 11");
        let r = plan[0].reread.as_ref().unwrap();
        assert_eq!(r.sql, "SELECT name FROM public.\"Orders\" WHERE id = 11");
        assert_eq!(r.expect, vec![("name".to_string(), Value::Null)]);
        assert!(preview(&plan).starts_with("-- check: SELECT name"));
        let mut pk = ChangeSet::new();
        pk.edit(0, 0, CellValue::text("10"), CellValue::text("99"));
        assert!(pk.plan(&t, &snap).unwrap_err().contains("primary key"));
        let mut del = ChangeSet::new();
        del.delete_row(0);
        let plan = del.plan(&t, &snap).unwrap();
        assert_eq!(plan[0].sql, "DELETE FROM public.\"Orders\" WHERE id = 10");
        let r = plan[0].reread.as_ref().unwrap();
        assert_eq!(r.sql, "SELECT name, total FROM public.\"Orders\" WHERE id = 10");
        assert_eq!(r.expect, vec![("name".to_string(), Value::Text("a'b".into())), ("total".to_string(), Value::Float(0.1))]);
    }

    #[test]
    fn readable_errors() {
        let e = ErrorInfo { code: Some("23505".into()), message: "duplicate key value violates unique constraint \"p_pkey\"".into(), ..Default::default() };
        let s = explain_error(&e);
        assert!(s.starts_with("Duplicate value"), "{s}");
        assert!(s.contains("p_pkey"));
        let e = ErrorInfo { code: Some("40001".into()), message: "conflict".into(), ..Default::default() };
        assert!(explain_error(&e).contains("nothing was saved"));
    }
}
