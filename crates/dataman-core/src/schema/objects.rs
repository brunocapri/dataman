//! The explorer's other catalog objects: per-schema counts, functions and procedures,
//! sequences, types (enums with their labels, composites, domains), triggers; per table its
//! constraints, triggers and partitions (or parent); per database its extensions; per server
//! its roles; table sizes and row estimates; privileges; and the definitions read when a menu
//! item asks for one.
//!
//! Every list is one catalog query, run when its node expands and cached like the rest of the
//! schema. A server that does not have a catalog table (LAR) gets `None` ("not reported") or an
//! empty list, never an error: the explorer hides those groups. An engine without a kind of
//! object ([`Caps`](crate::Caps)) gets the same. The SQL builders and the row parsers are pure
//! functions, per engine (`pg::objects`).

use std::collections::HashMap;
use std::sync::Arc;

use super::pg::objects as pg;
use super::{RelKey, lit, ms, my, parse_bool};
use crate::Task;
use crate::config::Engine;
use crate::conn::{Connection, first_ok};
use crate::sqlgen::{qualified, quote_ident, quote_literal};

pub(super) type Rows = Vec<Vec<Option<String>>>;

pub(super) fn get(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i).cloned().flatten().filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------------------------
// Model

/// How many of each object kind a schema has (`None`: the server did not say).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ObjectCounts {
    pub functions: Option<u64>,
    pub sequences: Option<u64>,
    pub types: Option<u64>,
    pub triggers: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuncKind {
    Function,
    Procedure,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Function {
    pub oid: u32,
    pub name: String,
    pub kind: FuncKind,
    /// `pg_get_function_identity_arguments`: `a integer, b text`.
    pub identity: String,
    /// The input argument types (`oidvectortypes`): `integer, text`.
    pub arg_types: Vec<String>,
    /// `pg_get_function_result` (`None` for procedures).
    pub result: Option<String>,
    pub returns_set: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sequence {
    pub name: String,
    pub type_name: Option<String>,
    /// The column that owns it (serial and identity columns): (table, column).
    pub owned_by: Option<(String, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeKind {
    Enum,
    Composite,
    Domain,
    Range,
}

impl TypeKind {
    pub fn label(self) -> &'static str {
        match self {
            TypeKind::Enum => "enum",
            TypeKind::Composite => "composite",
            TypeKind::Domain => "domain",
            TypeKind::Range => "range",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UserType {
    pub name: String,
    pub kind: TypeKind,
    /// A domain's base type.
    pub base: Option<String>,
    /// An enum's labels, in order.
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Trigger {
    pub name: String,
    pub table: String,
    pub enabled: bool,
    /// `before insert/update`, from `tgtype`.
    pub events: String,
    /// `pg_get_triggerdef`.
    pub definition: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConstraintKind {
    Primary,
    Unique,
    Foreign,
    Check,
    Exclusion,
}

impl ConstraintKind {
    pub fn from_contype(c: &str) -> Option<ConstraintKind> {
        Some(match c {
            "p" => ConstraintKind::Primary,
            "u" => ConstraintKind::Unique,
            "f" => ConstraintKind::Foreign,
            "c" => ConstraintKind::Check,
            "x" => ConstraintKind::Exclusion,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Constraint {
    pub name: String,
    pub kind: ConstraintKind,
    /// `pg_get_constraintdef`.
    pub definition: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Partition {
    pub schema: String,
    pub name: String,
    /// `FOR VALUES …` (`None` for inheritance children).
    pub bound: Option<String>,
}

/// What the explorer shows under a table besides its columns and indexes (`None`: not
/// reported).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableExtras {
    pub constraints: Option<Vec<Constraint>>,
    pub triggers: Option<Vec<Trigger>>,
    pub partitions: Option<Vec<Partition>>,
    /// The parent of a partition or inheritance child: (schema, name).
    pub parent: Option<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Extension {
    pub name: String,
    pub version: String,
    pub schema: Option<String>,
    /// The version `ALTER EXTENSION … UPDATE` would install.
    pub default_version: Option<String>,
}

impl Extension {
    /// The newer version available, if any.
    pub fn update(&self) -> Option<&str> {
        self.default_version.as_deref().filter(|d| *d != self.version)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Role {
    pub name: String,
    pub superuser: bool,
    pub login: bool,
    pub createdb: bool,
    pub createrole: bool,
    pub replication: bool,
    pub inherit: bool,
    pub bypassrls: bool,
    /// `-1`: no limit.
    pub conn_limit: i64,
    pub valid_until: Option<String>,
}

/// A table's estimated row count (`reltuples`; `None` when never analyzed) and total size.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RelStats {
    pub rows: Option<f64>,
    pub bytes: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Grant {
    pub grantee: String,
    pub privilege: String,
    pub grantable: bool,
}

/// The grants on a table or a schema.
#[derive(Clone, Debug, PartialEq)]
pub struct Privileges {
    pub schema_level: bool,
    pub owner: Option<String>,
    pub grants: Vec<Grant>,
}

pub const TABLE_PRIVILEGES: [&str; 7] = ["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"];
pub const SCHEMA_PRIVILEGES: [&str; 2] = ["USAGE", "CREATE"];

/// One cell of the grant matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cell {
    No,
    Yes,
    /// Granted WITH GRANT OPTION.
    Grantable,
}

impl Privileges {
    /// The matrix's columns: the standard privileges of the object, plus any other the server
    /// reported (PostgreSQL 17's MAINTAIN).
    pub fn columns(&self) -> Vec<String> {
        let base: &[&str] = if self.schema_level { &SCHEMA_PRIVILEGES } else { &TABLE_PRIVILEGES };
        let mut v: Vec<String> = base.iter().map(|s| s.to_string()).collect();
        for g in &self.grants {
            if !v.contains(&g.privilege) {
                v.push(g.privilege.clone());
            }
        }
        v
    }

    /// The matrix's rows: the owner first, then the other roles by name, PUBLIC last.
    pub fn roles(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        for g in &self.grants {
            if !v.contains(&g.grantee) {
                v.push(g.grantee.clone());
            }
        }
        let rank = |r: &str| {
            if Some(r) == self.owner.as_deref() {
                0
            } else if r == "PUBLIC" {
                2
            } else {
                1
            }
        };
        v.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
        v
    }

    pub fn cell(&self, role: &str, privilege: &str) -> Cell {
        match self.grants.iter().find(|g| g.grantee == role && g.privilege == privilege) {
            Some(g) if g.grantable => Cell::Grantable,
            Some(_) => Cell::Yes,
            None => Cell::No,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Formatting

/// `8 kB`, `1.2 MB`, `48 MB`: bytes in powers of 1024, PostgreSQL's unit names.
pub fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{} B", bytes.max(0));
    }
    let mut v = bytes as f64 / 1024.;
    let mut u = 0;
    while v >= 1024. && u + 1 < UNITS.len() {
        v /= 1024.;
        u += 1;
    }
    if v < 10. && v.fract() >= 0.05 { format!("{v:.1} {}", UNITS[u]) } else { format!("{:.0} {}", v.round(), UNITS[u]) }
}

/// An estimate in a few characters: `~0`, `~940`, `~1.2k`, `~48k`, `~3.1M`.
pub fn approx_count(n: f64) -> String {
    let n = n.max(0.).round();
    let (v, unit) = if n >= 1e9 {
        (n / 1e9, "B")
    } else if n >= 1e6 {
        (n / 1e6, "M")
    } else if n >= 1e3 {
        (n / 1e3, "k")
    } else {
        return format!("~{n:.0}");
    };
    if v < 10. && (v * 10.).round() % 10. != 0. { format!("~{v:.1}{unit}") } else { format!("~{:.0}{unit}", v.round()) }
}

/// `tgtype` → `before insert/update`, `after delete`, `instead of insert`.
pub fn trigger_events(tgtype: i64) -> String {
    let timing = if tgtype & 64 != 0 {
        "instead of"
    } else if tgtype & 2 != 0 {
        "before"
    } else {
        "after"
    };
    let ev: Vec<&str> = [(4, "insert"), (16, "update"), (8, "delete"), (32, "truncate")]
        .into_iter()
        .filter(|(b, _)| tgtype & b != 0)
        .map(|(_, n)| n)
        .collect();
    format!("{timing} {}", ev.join("/"))
}

/// A constraint in a few words for the explorer's meta column: `PK (id)`,
/// `FK (customer) → shop.customers(id)`, `unique (a, b)`, `check (total >= 0)`.
pub fn constraint_summary(kind: ConstraintKind, definition: Option<&str>) -> String {
    let word = match kind {
        ConstraintKind::Primary => "PK",
        ConstraintKind::Unique => "unique",
        ConstraintKind::Foreign => "FK",
        ConstraintKind::Check => "check",
        ConstraintKind::Exclusion => "exclude",
    };
    let Some(def) = definition.map(str::trim).filter(|d| !d.is_empty()) else { return word.to_string() };
    let strip = |prefix: &str| def.strip_prefix(prefix).map(str::trim_start);
    let rest = match kind {
        ConstraintKind::Primary => strip("PRIMARY KEY"),
        ConstraintKind::Unique => strip("UNIQUE"),
        ConstraintKind::Exclusion => strip("EXCLUDE"),
        ConstraintKind::Check => strip("CHECK").map(|r| {
            // `((expr))` → `(expr)`.
            match r.strip_prefix("((").and_then(|x| x.strip_suffix("))")) {
                Some(inner) if balanced(inner) => &r[1..r.len() - 1],
                _ => r,
            }
        }),
        ConstraintKind::Foreign => strip("FOREIGN KEY").map(|r| {
            // Up to the referenced columns: the ON DELETE / MATCH clauses are left out.
            let cut = r.find(" ON ").or_else(|| r.find(" MATCH ")).or_else(|| r.find(" DEFERRABLE")).or_else(|| r.find(" NOT VALID")).unwrap_or(r.len());
            &r[..cut]
        }),
    };
    match rest {
        Some(r) if kind == ConstraintKind::Foreign => format!("{word} {}", r.replacen(" REFERENCES ", " → ", 1)),
        Some(r) => format!("{word} {r}"),
        None => word.to_string(),
    }
}

/// Whether the parentheses of `s` balance (so stripping an outer pair keeps it valid).
pub(crate) fn balanced(s: &str) -> bool {
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// Splits `a integer, b numeric(10,2)` at the top-level commas.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut depth, mut cur) = (0i32, String::new());
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Definitions (pure)

/// `CREATE TYPE … AS ENUM (…)`.
pub fn enum_ddl(schema: &str, name: &str, labels: &[String]) -> String {
    let items: Vec<String> = labels.iter().map(|l| format!("    {}", quote_literal(l))).collect();
    format!("CREATE TYPE {} AS ENUM (\n{}\n);", qualified(Some(schema), name), items.join(",\n"))
}

/// `CREATE TYPE … AS (a integer, …)`.
pub fn composite_ddl(schema: &str, name: &str, fields: &[(String, String)]) -> String {
    let items: Vec<String> = fields.iter().map(|(n, t)| format!("    {} {t}", quote_ident(n))).collect();
    format!("CREATE TYPE {} AS (\n{}\n);", qualified(Some(schema), name), items.join(",\n"))
}

/// `CREATE DOMAIN … AS base [DEFAULT …] [NOT NULL] [CONSTRAINT … CHECK (…)]`.
pub fn domain_ddl(schema: &str, name: &str, base: &str, not_null: bool, default: Option<&str>, checks: &[(String, String)]) -> String {
    let mut s = format!("CREATE DOMAIN {} AS {base}", qualified(Some(schema), name));
    if let Some(d) = default {
        s.push_str(&format!("\n    DEFAULT {d}"));
    }
    if not_null {
        s.push_str("\n    NOT NULL");
    }
    for (n, def) in checks {
        s.push_str(&format!("\n    CONSTRAINT {} {def}", quote_ident(n)));
    }
    s.push(';');
    s
}

/// `CREATE ROLE … WITH …;` and its memberships (`GRANT group TO role`).
pub fn role_ddl(r: &Role, member_of: &[(String, bool)]) -> String {
    let flag = |on: bool, yes: &str, no: &str| if on { yes.to_string() } else { no.to_string() };
    let mut opts = vec![
        flag(r.login, "LOGIN", "NOLOGIN"),
        flag(r.superuser, "SUPERUSER", "NOSUPERUSER"),
        flag(r.createdb, "CREATEDB", "NOCREATEDB"),
        flag(r.createrole, "CREATEROLE", "NOCREATEROLE"),
        flag(r.inherit, "INHERIT", "NOINHERIT"),
        flag(r.replication, "REPLICATION", "NOREPLICATION"),
        flag(r.bypassrls, "BYPASSRLS", "NOBYPASSRLS"),
    ];
    if r.conn_limit >= 0 {
        opts.push(format!("CONNECTION LIMIT {}", r.conn_limit));
    }
    if let Some(v) = &r.valid_until {
        opts.push(format!("VALID UNTIL {}", quote_literal(v)));
    }
    let mut s = format!("CREATE ROLE {} WITH\n    {};", quote_ident(&r.name), opts.join("\n    "));
    for (g, admin) in member_of {
        s.push_str(&format!("\nGRANT {} TO {}{};", quote_ident(g), quote_ident(&r.name), if *admin { " WITH ADMIN OPTION" } else { "" }));
    }
    s
}

/// `FOR VALUES FROM ('2026-01-01') TO ('2026-02-01')` → `FROM ('2026-01-01') TO ('2026-02-01')`.
pub fn partition_bound_summary(bound: &str) -> String {
    bound.trim().strip_prefix("FOR VALUES").map(str::trim_start).unwrap_or(bound.trim()).to_string()
}

// ---------------------------------------------------------------------------------------------
// Cache

impl super::SchemaCache {
    pub fn cached_counts(&self, db: &str, schema: &str) -> Option<ObjectCounts> {
        self.dbs.read().get(db).and_then(|d| d.counts.get(schema).copied())
    }
    pub fn cached_stats(&self, db: &str, schema: &str) -> Option<Arc<HashMap<String, RelStats>>> {
        self.dbs.read().get(db).and_then(|d| d.stats.get(schema).cloned())
    }
    pub fn cached_functions(&self, db: &str, schema: &str) -> Option<Arc<Vec<Function>>> {
        self.dbs.read().get(db).and_then(|d| d.functions.get(schema).cloned())
    }
    pub fn cached_sequences(&self, db: &str, schema: &str) -> Option<Arc<Vec<Sequence>>> {
        self.dbs.read().get(db).and_then(|d| d.sequences.get(schema).cloned())
    }
    pub fn cached_types(&self, db: &str, schema: &str) -> Option<Arc<Vec<UserType>>> {
        self.dbs.read().get(db).and_then(|d| d.types.get(schema).cloned())
    }
    pub fn cached_triggers(&self, db: &str, schema: &str) -> Option<Arc<Vec<Trigger>>> {
        self.dbs.read().get(db).and_then(|d| d.triggers.get(schema).cloned())
    }
    pub fn cached_extras(&self, db: &str, schema: &str, rel: &str) -> Option<TableExtras> {
        self.dbs.read().get(db).and_then(|d| d.extras.get(&(schema.to_string(), rel.to_string())).cloned())
    }
    pub fn cached_extensions(&self, db: &str) -> Option<Vec<Extension>> {
        self.dbs.read().get(db).and_then(|d| d.extensions.clone())
    }
    pub fn cached_roles(&self) -> Option<Vec<Role>> {
        self.roles.read().clone()
    }

    /// Forgets the roles (read again on the next load).
    pub fn invalidate_roles(&self) {
        *self.roles.write() = None;
        self.bump();
    }

    /// Forgets a database's extensions.
    pub fn invalidate_extensions(&self, db: &str) {
        if let Some(d) = self.dbs.write().get_mut(db) {
            d.extensions = None;
        }
        self.bump();
    }

    fn with_db(&self, db: &str, f: impl FnOnce(&mut super::DbCache)) {
        f(self.dbs.write().entry(db.to_string()).or_default());
        self.bump();
    }
}

// ---------------------------------------------------------------------------------------------
// Loading

/// Runs each query alone; a failure leaves its kind out (`None`).
async fn each_count(client: &crate::backend::Db, schema: &str) -> ObjectCounts {
    let mut v: [Option<u64>; 4] = [None; 4];
    for (i, (_, q)) in pg::count_queries(schema).iter().enumerate() {
        if let Ok(rows) = first_ok(client, &[q]).await {
            v[i] = rows.first().and_then(|r| get(r, 0)).and_then(|s| s.parse().ok());
        }
    }
    ObjectCounts { functions: v[0], sequences: v[1], types: v[2], triggers: v[3] }
}

impl Connection {
    /// The counts of a schema's functions, sequences, types and triggers (one query).
    pub fn object_counts(self: &Arc<Self>, db: &str, schema: &str) -> Task<ObjectCounts> {
        let (this, db, schema) = (self.clone(), db.to_string(), schema.to_string());
        self.spawn(async move {
            if let Some(c) = this.schema.cached_counts(&db, &schema) {
                return Ok(c);
            }
            let caps = this.caps();
            if !(caps.functions || caps.sequences || caps.types || caps.triggers) {
                return Ok(ObjectCounts::default());
            }
            let client = this.meta(&db).await?;
            let c = match this.engine() {
                Engine::MySql => {
                    first_ok(&client, &[&my::counts_sql(&schema)]).await.ok().and_then(|rows| rows.first().map(|r| my::parse_counts(r))).unwrap_or_default()
                }
                Engine::SqlServer => {
                    first_ok(&client, &[&ms::counts_sql(&schema)]).await.ok().and_then(|rows| rows.first().map(|r| ms::parse_counts(r))).unwrap_or_default()
                }
                _ => match first_ok(&client, &[&pg::counts_sql(&schema)]).await {
                    Ok(rows) => rows.first().map(|r| pg::parse_counts(r)).unwrap_or_default(),
                    // A server that cannot run the four together (LAR's planner): one by one.
                    Err(_) => each_count(&client, &schema).await,
                },
            };
            this.schema.with_db(&db, |d| {
                d.counts.insert(schema, c);
            });
            Ok(c)
        })
    }

    /// Row estimates and total sizes of a schema's tables (one query, when the tables group
    /// opens). Empty when the server does not report them.
    pub fn rel_stats(self: &Arc<Self>, db: &str, schema: &str) -> Task<Arc<HashMap<String, RelStats>>> {
        let (this, db, schema) = (self.clone(), db.to_string(), schema.to_string());
        self.spawn(async move {
            if let Some(s) = this.schema.cached_stats(&db, &schema) {
                return Ok(s);
            }
            let client = this.meta(&db).await?;
            let r = match this.engine() {
                Engine::MySql => first_ok(&client, &[&my::stats_sql(&schema)]).await.map(|rows| my::parse_stats(&rows)),
                Engine::SqlServer => {
                    let qs = ms::stats_sql(&schema);
                    first_ok(&client, &[&qs[0], &qs[1]]).await.map(|rows| ms::parse_stats(&rows))
                }
                _ => {
                    let qs = pg::stats_sql(&schema);
                    first_ok(&client, &[&qs[0], &qs[1], &qs[2]]).await.map(|rows| pg::parse_stats(&rows))
                }
            };
            let m = match r {
                Ok(m) => m,
                Err(e) => {
                    this.schema.note(format!("{db}.{schema}: table sizes unavailable: {e}"));
                    HashMap::new()
                }
            };
            let m = Arc::new(m);
            this.schema.with_db(&db, |d| {
                d.stats.insert(schema, m.clone());
            });
            Ok(m)
        })
    }

    pub fn functions(self: &Arc<Self>, db: &str, schema: &str) -> Task<Arc<Vec<Function>>> {
        let (this, db, schema) = (self.clone(), db.to_string(), schema.to_string());
        self.spawn(async move {
            if let Some(v) = this.schema.cached_functions(&db, &schema) {
                return Ok(v);
            }
            if !this.caps().functions {
                return Ok(Default::default());
            }
            let client = this.meta(&db).await?;
            let v: Vec<Function> = match this.engine() {
                Engine::MySql => {
                    let rows = first_ok(&client, &[&my::functions_sql(&schema)]).await.unwrap_or_default();
                    rows.iter().enumerate().filter_map(|(n, r)| my::parse_function(n, r)).collect()
                }
                Engine::SqlServer => {
                    let rows = first_ok(&client, &[&ms::routines_sql(Some(&schema))]).await.unwrap_or_default();
                    ms::parse_routines(&rows).into_iter().enumerate().map(|(n, f)| ms::function_of(n, f)).collect()
                }
                _ => {
                    let qs = pg::functions_sql(&schema);
                    first_ok(&client, &[&qs[0], &qs[1]]).await.map(|rows| rows.iter().filter_map(|r| pg::parse_function(r)).collect()).unwrap_or_default()
                }
            };
            let v = Arc::new(v);
            this.schema.with_db(&db, |d| {
                d.functions.insert(schema, v.clone());
            });
            Ok(v)
        })
    }

    pub fn sequences(self: &Arc<Self>, db: &str, schema: &str) -> Task<Arc<Vec<Sequence>>> {
        let (this, db, schema) = (self.clone(), db.to_string(), schema.to_string());
        self.spawn(async move {
            if let Some(v) = this.schema.cached_sequences(&db, &schema) {
                return Ok(v);
            }
            if !this.caps().sequences {
                return Ok(Default::default());
            }
            let client = this.meta(&db).await?;
            let v: Vec<Sequence> = if this.engine() == Engine::SqlServer {
                first_ok(&client, &[&ms::sequences_sql(&schema)]).await.map(|rows| rows.iter().filter_map(|r| ms::parse_sequence(r)).collect()).unwrap_or_default()
            } else {
                let qs = pg::sequences_sql(&schema);
                first_ok(&client, &[&qs[0], &qs[1], &qs[2]]).await.map(|rows| rows.iter().filter_map(|r| pg::parse_sequence(r)).collect()).unwrap_or_default()
            };
            let v = Arc::new(v);
            this.schema.with_db(&db, |d| {
                d.sequences.insert(schema, v.clone());
            });
            Ok(v)
        })
    }

    pub fn types(self: &Arc<Self>, db: &str, schema: &str) -> Task<Arc<Vec<UserType>>> {
        let (this, db, schema) = (self.clone(), db.to_string(), schema.to_string());
        self.spawn(async move {
            if let Some(v) = this.schema.cached_types(&db, &schema) {
                return Ok(v);
            }
            if !this.caps().types {
                return Ok(Default::default());
            }
            let client = this.meta(&db).await?;
            let (q, parse): (String, fn(&[Option<String>]) -> Option<UserType>) =
                if this.engine() == Engine::SqlServer { (ms::types_sql(&schema), ms::parse_type) } else { (pg::types_sql(&schema), pg::parse_type) };
            let v: Vec<UserType> = first_ok(&client, &[&q]).await.map(|rows| rows.iter().filter_map(|r| parse(r)).collect()).unwrap_or_default();
            let v = Arc::new(v);
            this.schema.with_db(&db, |d| {
                d.types.insert(schema, v.clone());
            });
            Ok(v)
        })
    }

    pub fn triggers(self: &Arc<Self>, db: &str, schema: &str) -> Task<Arc<Vec<Trigger>>> {
        let (this, db, schema) = (self.clone(), db.to_string(), schema.to_string());
        self.spawn(async move {
            if let Some(v) = this.schema.cached_triggers(&db, &schema) {
                return Ok(v);
            }
            if !this.caps().triggers {
                return Ok(Default::default());
            }
            let client = this.meta(&db).await?;
            let (q, parse): (String, fn(&[Option<String>]) -> Option<Trigger>) = match this.engine() {
                Engine::MySql => (my::triggers_sql(&schema), my::parse_trigger),
                Engine::SqlServer => (ms::triggers_sql(&schema), ms::parse_trigger),
                _ => (pg::triggers_sql(&schema), pg::parse_trigger),
            };
            let v: Vec<Trigger> = first_ok(&client, &[&q]).await.map(|rows| rows.iter().filter_map(|r| parse(r)).collect()).unwrap_or_default();
            let v = Arc::new(v);
            this.schema.with_db(&db, |d| {
                d.triggers.insert(schema, v.clone());
            });
            Ok(v)
        })
    }

    /// A table's constraints, triggers and partitions or parent (one query).
    pub fn table_extras(self: &Arc<Self>, db: &str, schema: &str, rel: &str) -> Task<TableExtras> {
        let (this, db, schema, rel) = (self.clone(), db.to_string(), schema.to_string(), rel.to_string());
        self.spawn(async move {
            if let Some(v) = this.schema.cached_extras(&db, &schema, &rel) {
                return Ok(v);
            }
            if this.relation(&db, &schema, &rel).is_none() {
                this.load_relations(&db, &schema).await?;
            }
            let oid = this.relation(&db, &schema, &rel).and_then(|r| r.oid);
            let mut t = TableExtras::default();
            if this.engine() == Engine::MySql {
                let client = this.meta(&db).await?;
                let qs = my::constraints_sql(&schema, &rel);
                let mut cons: Vec<Constraint> = Vec::new();
                for q in &qs {
                    if let Ok(rows) = first_ok(&client, &[q]).await {
                        cons.extend(rows.iter().filter_map(|r| my::parse_constraint(&schema, r)));
                    }
                }
                cons.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
                t.constraints = Some(cons);
                t.triggers = first_ok(&client, &[&my::table_triggers_sql(&schema, &rel)]).await.ok().map(|rows| rows.iter().filter_map(|r| my::parse_trigger(r)).collect());
                t.partitions = Some(Vec::new());
            } else if this.engine() == Engine::SqlServer {
                let client = this.meta(&db).await?;
                t.constraints = first_ok(&client, &[&ms::constraints_sql(&schema, &rel)]).await.ok().map(|rows| ms::parse_constraints(&schema, &rows));
                t.triggers = first_ok(&client, &[&ms::table_triggers_sql(&schema, &rel)]).await.ok().map(|rows| rows.iter().filter_map(|r| ms::parse_trigger(r)).collect());
                t.partitions = Some(Vec::new());
            } else if let Some(oid) = oid {
                let client = this.meta(&db).await?;
                match first_ok(&client, &[&pg::extras_sql(oid)]).await {
                    Ok(rows) => {
                        // Every piece answered: what is absent is empty, not unreported.
                        t.constraints = Some(Vec::new());
                        t.triggers = Some(Vec::new());
                        t.partitions = Some(Vec::new());
                        pg::parse_extras(&rows, &rel, &mut t);
                    }
                    Err(_) => {
                        for q in pg::extras_queries(oid) {
                            if let Ok(rows) = first_ok(&client, &[&q]).await {
                                pg::parse_extras(&rows, &rel, &mut t);
                            }
                        }
                    }
                }
            }
            let key: RelKey = (schema, rel);
            this.schema.with_db(&db, |d| {
                d.extras.insert(key, t.clone());
            });
            Ok(t)
        })
    }

    pub fn extensions(self: &Arc<Self>, db: &str) -> Task<Vec<Extension>> {
        let (this, db) = (self.clone(), db.to_string());
        self.spawn(async move {
            if let Some(v) = this.schema.cached_extensions(&db) {
                return Ok(v);
            }
            if !this.caps().extensions {
                return Ok(Vec::new());
            }
            let client = this.meta(&db).await?;
            let qs = pg::extensions_sql();
            let v: Vec<Extension> = first_ok(&client, &qs).await.map(|rows| rows.iter().filter_map(|r| pg::parse_extension(r)).collect()).unwrap_or_default();
            this.schema.with_db(&db, |d| d.extensions = Some(v.clone()));
            Ok(v)
        })
    }

    /// The server's roles (the built-in `pg_*` ones left out).
    pub fn roles(self: &Arc<Self>) -> Task<Vec<Role>> {
        let this = self.clone();
        self.spawn(async move {
            if let Some(v) = this.schema.cached_roles() {
                return Ok(v);
            }
            if !this.caps().roles {
                return Ok(Vec::new());
            }
            let client = this.meta(this.database()).await?;
            let qs = pg::roles_sql();
            let v: Vec<Role> = first_ok(&client, &qs).await.map(|rows| rows.iter().filter_map(|r| pg::parse_role(r)).collect()).unwrap_or_default();
            *this.schema.roles.write() = Some(v.clone());
            this.schema.bump();
            Ok(v)
        })
    }

    /// `pg_get_functiondef` (`None` when the server does not report it).
    pub fn function_definition(self: &Arc<Self>, db: &str, oid: u32) -> Task<Option<String>> {
        let (this, db) = (self.clone(), db.to_string());
        self.spawn(async move {
            if !this.caps().functions {
                return Ok(None);
            }
            let client = this.meta(&db).await?;
            let rows = first_ok(&client, &[&format!("SELECT pg_catalog.pg_get_functiondef({oid})")]).await?;
            Ok(rows.first().and_then(|r| get(r, 0)).map(|d| {
                let d = d.trim_end().to_string();
                if d.ends_with(';') { d } else { format!("{d};") }
            }))
        })
    }

    /// A function's or procedure's definition: `pg_get_functiondef`, MySQL's `SHOW CREATE
    /// PROCEDURE | FUNCTION`.
    pub fn routine_definition(self: &Arc<Self>, db: &str, schema: &str, f: &Function) -> Task<Option<String>> {
        if self.engine() == Engine::SqlServer {
            return self.module_definition(db, schema, &f.name);
        }
        if self.engine() != Engine::MySql {
            return self.function_definition(db, f.oid);
        }
        let (this, db, schema, name, kind) = (self.clone(), db.to_string(), schema.to_string(), f.name.clone(), f.kind);
        self.spawn(async move {
            let what = if kind == FuncKind::Procedure { "PROCEDURE" } else { "FUNCTION" };
            let client = this.meta(&db).await?;
            let rows = first_ok(&client, &[&my::show_create(what, &schema, &name)]).await?;
            Ok(my::parse_show_create(what, &rows))
        })
    }

    /// A SQL Server module's text (procedure, function, view, trigger) as it was created:
    /// `OBJECT_DEFINITION`.
    pub fn module_definition(self: &Arc<Self>, db: &str, schema: &str, name: &str) -> Task<Option<String>> {
        let (this, db, schema, name) = (self.clone(), db.to_string(), schema.to_string(), name.to_string());
        self.spawn(async move {
            let client = this.meta(&db).await?;
            let rows = first_ok(&client, &[&ms::definition_sql(&schema, &name)]).await?;
            Ok(rows.first().and_then(|r| get(r, 0)).map(|d| d.trim().to_string()))
        })
    }

    /// A type's `CREATE TYPE` / `CREATE DOMAIN`, from the catalog.
    pub fn type_definition(self: &Arc<Self>, db: &str, schema: &str, name: &str) -> Task<Option<String>> {
        let (this, db, schema, name) = (self.clone(), db.to_string(), schema.to_string(), name.to_string());
        self.spawn(async move {
            if !this.caps().types {
                return Ok(None);
            }
            let client = this.meta(&db).await?;
            if this.engine() == Engine::SqlServer {
                let rows = first_ok(&client, &[&ms::type_definition_sql(&schema, &name)]).await?;
                return Ok(ms::parse_type_definition(&schema, &name, &rows));
            }
            let q = format!(
                "SELECT t.oid, t.typtype, pg_catalog.format_type(t.typbasetype, t.typtypmod), t.typnotnull, t.typdefault, t.typrelid \
                 FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace WHERE {} AND t.typname = {}",
                pg::in_schema("n", &schema),
                lit(&name)
            );
            let Some(r) = first_ok(&client, &[&q]).await?.into_iter().next() else { return Ok(None) };
            let oid = get(&r, 0).unwrap_or_default();
            Ok(match get(&r, 1).as_deref() {
                Some("e") => {
                    let rows = first_ok(&client, &[&format!("SELECT enumlabel FROM pg_catalog.pg_enum WHERE enumtypid = {oid} ORDER BY enumsortorder")]).await?;
                    let labels: Vec<String> = rows.iter().filter_map(|r| r.first().cloned().flatten()).collect();
                    Some(enum_ddl(&schema, &name, &labels))
                }
                Some("c") => {
                    let relid = get(&r, 5).unwrap_or_default();
                    let rows = first_ok(
                        &client,
                        &[&format!(
                            "SELECT attname, pg_catalog.format_type(atttypid, atttypmod) FROM pg_catalog.pg_attribute \
                             WHERE attrelid = {relid} AND attnum > 0 AND NOT attisdropped ORDER BY attnum"
                        )],
                    )
                    .await?;
                    let fields: Vec<(String, String)> = rows.iter().filter_map(|r| Some((get(r, 0)?, get(r, 1).unwrap_or_default()))).collect();
                    Some(composite_ddl(&schema, &name, &fields))
                }
                Some("d") => {
                    let rows = first_ok(
                        &client,
                        &[&format!(
                            "SELECT conname, pg_catalog.pg_get_constraintdef(oid) FROM pg_catalog.pg_constraint \
                             WHERE contypid = {oid} AND contype = 'c' ORDER BY conname"
                        )],
                    )
                    .await
                    .unwrap_or_default();
                    let checks: Vec<(String, String)> = rows.iter().filter_map(|r| Some((get(r, 0)?, get(r, 1)?))).collect();
                    let base = get(&r, 2).unwrap_or_else(|| "text".into());
                    Some(domain_ddl(&schema, &name, &base, parse_bool(&get(&r, 3)) == Some(true), get(&r, 4).as_deref(), &checks))
                }
                Some("r") => {
                    let rows = first_ok(
                        &client,
                        &[&format!(
                            "SELECT pg_catalog.format_type(r.rngsubtype, NULL), \
                             CASE WHEN r.rngcollation <> 0 AND r.rngcollation <> (SELECT typcollation FROM pg_catalog.pg_type WHERE oid = r.rngsubtype) \
                                  THEN (SELECT quote_ident(n.nspname) || '.' || quote_ident(c.collname) FROM pg_catalog.pg_collation c JOIN pg_catalog.pg_namespace n ON n.oid = c.collnamespace WHERE c.oid = r.rngcollation) END, \
                             CASE WHEN NOT o.opcdefault THEN quote_ident(n2.nspname) || '.' || quote_ident(o.opcname) END, \
                             NULLIF(r.rngcanonical::oid, 0)::regproc::text, NULLIF(r.rngsubdiff::oid, 0)::regproc::text \
                             FROM pg_catalog.pg_range r JOIN pg_catalog.pg_opclass o ON o.oid = r.rngsubopc \
                             JOIN pg_catalog.pg_namespace n2 ON n2.oid = o.opcnamespace WHERE r.rngtypid = {oid}"
                        )],
                    )
                    .await?;
                    let Some(r) = rows.first() else { return Ok(None) };
                    let mut opts = vec![format!("subtype = {}", get(r, 0).unwrap_or_default())];
                    if let Some(o) = get(r, 2) {
                        opts.push(format!("subtype_opclass = {o}"));
                    }
                    if let Some(c) = get(r, 1) {
                        opts.push(format!("collation = {c}"));
                    }
                    if let Some(f) = get(r, 3) {
                        opts.push(format!("canonical = {f}"));
                    }
                    if let Some(f) = get(r, 4) {
                        opts.push(format!("subtype_diff = {f}"));
                    }
                    Some(format!("CREATE TYPE {}.{} AS RANGE (\n    {}\n);", quote_ident(&schema), quote_ident(&name), opts.join(",\n    ")))
                }
                _ => None,
            })
        })
    }

    /// A role's `CREATE ROLE` with its memberships.
    pub fn role_definition(self: &Arc<Self>, name: &str) -> Task<Option<String>> {
        let (this, name) = (self.clone(), name.to_string());
        self.spawn(async move {
            let roles = match this.schema.cached_roles() {
                Some(r) => r,
                None => this.roles().await?,
            };
            let Some(role) = roles.into_iter().find(|r| r.name == name) else { return Ok(None) };
            let client = this.meta(this.database()).await?;
            let q = format!(
                "SELECT g.rolname, m.admin_option FROM pg_catalog.pg_auth_members m \
                 JOIN pg_catalog.pg_roles g ON g.oid = m.roleid JOIN pg_catalog.pg_roles u ON u.oid = m.member \
                 WHERE u.rolname = {} ORDER BY 1",
                lit(&name)
            );
            let member_of: Vec<(String, bool)> = first_ok(&client, &[&q])
                .await
                .unwrap_or_default()
                .iter()
                .filter_map(|r| Some((get(r, 0)?, parse_bool(&get(r, 1)).unwrap_or(false))))
                .collect();
            Ok(Some(role_ddl(&role, &member_of)))
        })
    }

    /// A sequence's `last_value` and `is_called`.
    pub fn sequence_value(self: &Arc<Self>, db: &str, schema: &str, name: &str) -> Task<Option<(String, bool)>> {
        let q = match self.engine() {
            Engine::SqlServer => ms::sequence_value_sql(schema, name),
            _ => format!("SELECT last_value, is_called FROM {}", qualified(Some(schema), name)),
        };
        let (this, db) = (self.clone(), db.to_string());
        self.spawn(async move {
            if !this.caps().sequences {
                return Ok(None);
            }
            let client = this.meta(&db).await?;
            let rows = first_ok(&client, &[&q]).await?;
            Ok(rows.first().and_then(|r| Some((get(r, 0)?, parse_bool(&get(r, 1)).unwrap_or(true)))))
        })
    }

    /// The grants on a table (`table` given) or a schema; `None` when the server does not
    /// report them.
    pub fn privileges(self: &Arc<Self>, db: &str, schema: &str, table: Option<&str>) -> Task<Option<Privileges>> {
        let (this, db, schema, table) = (self.clone(), db.to_string(), schema.to_string(), table.map(str::to_string));
        self.spawn(async move {
            if !this.caps().privileges {
                return Ok(None);
            }
            let client = this.meta(&db).await?;
            let qs = pg::privileges_sql(&schema, table.as_deref());
            let refs: Vec<&str> = qs.iter().map(String::as_str).collect();
            Ok(match first_ok(&client, &refs).await {
                Ok(rows) => pg::parse_privileges(&rows, table.is_none()),
                Err(_) => None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_estimates() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(8192), "8 kB");
        assert_eq!(human_size(1536), "1.5 kB");
        assert_eq!(human_size(48 * 1024 * 1024), "48 MB");
        assert_eq!(human_size((1.2 * 1024. * 1024. * 1024.) as i64), "1.2 GB");
        assert_eq!(human_size(-5), "0 B");
        assert_eq!(approx_count(0.), "~0");
        assert_eq!(approx_count(940.), "~940");
        assert_eq!(approx_count(1234.), "~1.2k");
        assert_eq!(approx_count(1000.), "~1k");
        assert_eq!(approx_count(48_310.), "~48k");
        assert_eq!(approx_count(1_000_000.), "~1M");
        assert_eq!(approx_count(3_140_000.), "~3.1M");
        assert_eq!(approx_count(2.5e9), "~2.5B");
    }

    #[test]
    fn trigger_event_bits() {
        // ROW | BEFORE | INSERT | UPDATE
        assert_eq!(trigger_events(1 | 2 | 4 | 16), "before insert/update");
        assert_eq!(trigger_events(8), "after delete");
        assert_eq!(trigger_events(64 | 1 | 4), "instead of insert");
        assert_eq!(trigger_events(32), "after truncate");
    }

    #[test]
    fn constraint_summaries() {
        use ConstraintKind::*;
        assert_eq!(constraint_summary(Primary, Some("PRIMARY KEY (id)")), "PK (id)");
        assert_eq!(
            constraint_summary(Foreign, Some("FOREIGN KEY (customer) REFERENCES shop.customers(id) ON DELETE CASCADE")),
            "FK (customer) → shop.customers(id)"
        );
        assert_eq!(constraint_summary(Unique, Some("UNIQUE (a, b)")), "unique (a, b)");
        assert_eq!(constraint_summary(Check, Some("CHECK ((total >= (0)::numeric))")), "check (total >= (0)::numeric)");
        // Outer parentheses that do not wrap the whole expression stay.
        assert_eq!(constraint_summary(Check, Some("CHECK ((a > 0) AND (b > 0))")), "check ((a > 0) AND (b > 0))");
        assert_eq!(constraint_summary(Exclusion, Some("EXCLUDE USING gist (r WITH &&)")), "exclude USING gist (r WITH &&)");
        assert_eq!(constraint_summary(Check, None), "check");
        assert_eq!(constraint_summary(Foreign, Some("FOREIGN KEY (a) REFERENCES t(b) NOT VALID")), "FK (a) → t(b)");
    }

    #[test]
    fn argument_lists() {
        assert_eq!(split_args("a integer, b numeric(10,2), c text[]"), vec!["a integer", "b numeric(10,2)", "c text[]"]);
        assert!(split_args("").is_empty());
        assert_eq!(split_args("integer"), vec!["integer"]);
    }

    #[test]
    fn definitions() {
        assert_eq!(enum_ddl("s", "mood", &["sad".into(), "it's ok".into()]), "CREATE TYPE s.mood AS ENUM (\n    'sad',\n    'it''s ok'\n);");
        assert_eq!(composite_ddl("s", "pt", &[("x".into(), "integer".into()), ("Y".into(), "text".into())]), "CREATE TYPE s.pt AS (\n    x integer,\n    \"Y\" text\n);");
        assert_eq!(
            domain_ddl("s", "price", "numeric(12,2)", true, Some("0"), &[("price_check".into(), "CHECK ((VALUE >= (0)::numeric))".into())]),
            "CREATE DOMAIN s.price AS numeric(12,2)\n    DEFAULT 0\n    NOT NULL\n    CONSTRAINT price_check CHECK ((VALUE >= (0)::numeric));"
        );
        let r = Role { name: "app".into(), login: true, inherit: true, conn_limit: 5, ..Default::default() };
        let d = role_ddl(&r, &[("readers".into(), false), ("ops".into(), true)]);
        assert!(d.starts_with("CREATE ROLE app WITH\n    LOGIN\n    NOSUPERUSER"), "{d}");
        assert!(d.contains("CONNECTION LIMIT 5;") && d.contains("GRANT readers TO app;") && d.contains("GRANT ops TO app WITH ADMIN OPTION;"), "{d}");
    }
}
