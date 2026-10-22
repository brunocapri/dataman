pub mod catalog;
pub(crate) mod duck;
pub mod lite;
mod model;
pub(crate) mod ms;
pub(crate) mod my;
pub mod objects;
mod pg;

pub use catalog::{Catalog, FuncSig, TypeEntry};
pub use model::{Column, Index, RelKind, Relation, SchemaSnapshot};
pub use objects::{
    Constraint, ConstraintKind, Extension, FuncKind, Function, ObjectCounts, Partition, Privileges, RelStats, Role, Sequence, TableExtras,
    Trigger, TypeKind, UserType, ViewSource, ViewSourceKind,
};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use parking_lot::{Mutex, RwLock};

use crate::Task;
use crate::backend::Db;
use crate::conn::{Connection, first_ok};
use crate::sql::Dialect;

type RelKey = (String, String);

async fn read_databases(c: &Db) -> Result<Vec<String>> {
    match c {
        Db::Pg(_) => pg::databases(c).await,
        Db::My(_) => my::databases(c).await,
        Db::Ms(_) => ms::databases(c).await,
        Db::Lite(_) => lite::databases(c).await,
    }
}

async fn read_schemas(c: &Db, db: &str) -> Result<Vec<String>> {
    match c {
        Db::Pg(_) => pg::schemas(c).await,
        Db::My(_) | Db::Lite(_) => Ok(vec![db.to_string()]),
        Db::Ms(_) => ms::schemas(c).await,
    }
}

async fn read_relations(c: &Db, schema: &str) -> Result<Vec<Relation>> {
    match c {
        Db::Pg(_) => pg::relations(c, schema).await,
        Db::My(_) => my::relations(c, schema).await,
        Db::Ms(_) => ms::relations(c, schema).await,
        Db::Lite(_) => lite::relations(c, schema).await,
    }
}

async fn read_columns(c: &Db, rel: &Relation) -> Option<Vec<Column>> {
    match c {
        Db::Pg(_) => pg::columns(c, rel.oid?).await,
        Db::My(_) => my::columns(c, &rel.schema, &rel.name).await,
        Db::Ms(_) => ms::columns(c, &rel.schema, &rel.name).await,
        Db::Lite(_) => lite::columns(c, &rel.schema, &rel.name).await,
    }
}

async fn read_indexes(c: &Db, rel: &Relation, cols: &[Column]) -> Option<Result<Vec<Index>>> {
    match c {
        Db::Pg(_) => Some(pg::indexes(c, rel.oid?, cols).await),
        Db::My(_) => Some(my::indexes(c, &rel.schema, &rel.name).await),
        Db::Ms(_) => Some(ms::indexes(c, &rel.schema, &rel.name).await),
        Db::Lite(_) => Some(lite::indexes(c, &rel.schema, &rel.name).await),
    }
}

async fn read_columns_of_schemas(c: &Db, list: &str) -> Result<Vec<Vec<Option<String>>>> {
    match c {
        Db::Pg(_) => pg::columns_of_schemas(c, list).await,
        Db::My(_) => my::columns_of_schemas(c, list).await,
        Db::Ms(_) => ms::columns_of_schemas(c, list).await,
        Db::Lite(_) => lite::columns_of_schemas(c, list).await,
    }
}

#[derive(Default)]
struct DbCache {
    search_path: Option<Vec<String>>,
    schemas: Option<Vec<String>>,
    relations: HashMap<String, Vec<Relation>>,
    columns: HashMap<RelKey, Vec<Column>>,
    brief: HashSet<RelKey>,
    indexes: HashMap<RelKey, Vec<Index>>,
    counts: HashMap<String, ObjectCounts>,
    stats: HashMap<String, Arc<HashMap<String, RelStats>>>,
    functions: HashMap<String, Arc<Vec<Function>>>,
    sequences: HashMap<String, Arc<Vec<Sequence>>>,
    types: HashMap<String, Arc<Vec<UserType>>>,
    triggers: HashMap<String, Arc<Vec<Trigger>>>,
    extras: HashMap<RelKey, TableExtras>,
    extensions: Option<Vec<Extension>>,
    catalog: Option<Arc<Catalog>>,
    preloaded: bool,
}

impl DbCache {
    fn forget_schema(&mut self, schema: &str) {
        self.preloaded = false;
        self.relations.remove(schema);
        self.columns.retain(|(s, _), _| s != schema);
        self.brief.retain(|(s, _)| s != schema);
        self.indexes.retain(|(s, _), _| s != schema);
        self.extras.retain(|(s, _), _| s != schema);
        self.counts.remove(schema);
        self.stats.remove(schema);
        self.functions.remove(schema);
        self.sequences.remove(schema);
        self.types.remove(schema);
        self.triggers.remove(schema);
    }
}

pub struct SchemaCache {
    dialect: Dialect,
    default_db: String,
    databases: RwLock<Option<Vec<String>>>,
    roles: RwLock<Option<Vec<Role>>>,
    dbs: RwLock<HashMap<String, DbCache>>,
    notes: Mutex<Vec<String>>,
    version: AtomicU64,
    snapshots: Mutex<HashMap<String, (u64, Arc<SchemaSnapshot>)>>,
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn parse_bool(s: &Option<String>) -> Option<bool> {
    match s.as_deref() {
        Some("t" | "true" | "TRUE" | "1" | "YES" | "yes") => Some(true),
        Some("f" | "false" | "FALSE" | "0" | "NO" | "no") => Some(false),
        _ => None,
    }
}

impl SchemaCache {
    pub fn new(default_db: String, search_path: Vec<String>) -> SchemaCache {
        Self::with_dialect(default_db, search_path, Dialect::Postgres)
    }

    pub fn with_dialect(default_db: String, search_path: Vec<String>, dialect: Dialect) -> SchemaCache {
        let mut dbs = HashMap::new();
        dbs.insert(default_db.clone(), DbCache { search_path: Some(search_path), ..Default::default() });
        SchemaCache {
            dialect,
            default_db,
            databases: RwLock::new(None),
            roles: RwLock::new(None),
            dbs: RwLock::new(dbs),
            notes: Mutex::new(Vec::new()),
            version: AtomicU64::new(1),
            snapshots: Mutex::new(HashMap::new()),
        }
    }

    pub fn default_database(&self) -> &str {
        &self.default_db
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    fn bump(&self) {
        self.version.fetch_add(1, Ordering::AcqRel);
    }

    pub fn notes(&self) -> Vec<String> {
        self.notes.lock().clone()
    }

    fn note(&self, what: String) {
        let mut n = self.notes.lock();
        if !n.contains(&what) {
            n.push(what);
        }
    }

    pub(crate) fn has_search_path(&self, db: &str) -> bool {
        self.dbs.read().get(db).is_some_and(|d| d.search_path.is_some())
    }

    pub(crate) fn set_search_path(&self, db: &str, sp: Vec<String>) {
        self.dbs.write().entry(db.to_string()).or_default().search_path = Some(sp);
        self.bump();
    }

    pub fn search_path(&self, db: &str) -> Vec<String> {
        self.dbs.read().get(db).and_then(|d| d.search_path.clone()).unwrap_or_else(|| vec!["public".into()])
    }

    pub fn cached_databases(&self) -> Option<Vec<String>> {
        self.databases.read().clone()
    }
    pub fn cached_schemas(&self, db: &str) -> Option<Vec<String>> {
        self.dbs.read().get(db).and_then(|d| d.schemas.clone())
    }
    pub fn cached_relations(&self, db: &str, schema: &str) -> Option<Vec<Relation>> {
        self.dbs.read().get(db).and_then(|d| d.relations.get(schema).cloned())
    }
    pub fn cached_columns(&self, db: &str, schema: &str, rel: &str) -> Option<Vec<Column>> {
        self.dbs.read().get(db).and_then(|d| d.columns.get(&(schema.into(), rel.into())).cloned())
    }
    pub fn cached_full_columns(&self, db: &str, schema: &str, rel: &str) -> Option<Vec<Column>> {
        let key = (schema.to_string(), rel.to_string());
        self.dbs.read().get(db).filter(|d| !d.brief.contains(&key)).and_then(|d| d.columns.get(&key).cloned())
    }
    pub fn cached_indexes(&self, db: &str, schema: &str, rel: &str) -> Option<Vec<Index>> {
        self.dbs.read().get(db).and_then(|d| d.indexes.get(&(schema.into(), rel.into())).cloned())
    }

    pub fn invalidate(&self, db: Option<&str>) {
        match db {
            Some(db) => {
                if let Some(d) = self.dbs.write().get_mut(db) {
                    let sp = d.search_path.take();
                    *d = DbCache { search_path: sp, ..Default::default() };
                }
            }
            None => {
                *self.databases.write() = None;
                *self.roles.write() = None;
                for d in self.dbs.write().values_mut() {
                    let sp = d.search_path.take();
                    *d = DbCache { search_path: sp, ..Default::default() };
                }
            }
        }
        self.bump();
    }

    pub fn invalidate_schema(&self, db: &str, schema: &str) {
        if let Some(d) = self.dbs.write().get_mut(db) {
            d.forget_schema(schema);
        }
        self.bump();
    }

    pub fn invalidate_relation(&self, db: &str, schema: &str, rel: &str) {
        if let Some(d) = self.dbs.write().get_mut(db) {
            let key = (schema.to_string(), rel.to_string());
            // A relation without columns would read as one that has none.
            d.preloaded = false;
            d.columns.remove(&key);
            d.brief.remove(&key);
            d.indexes.remove(&key);
            d.extras.remove(&key);
        }
        self.bump();
    }

    pub fn snapshot(&self, db: &str) -> Arc<SchemaSnapshot> {
        let v = self.version();
        if let Some((sv, s)) = self.snapshots.lock().get(db)
            && *sv == v
        {
            return s.clone();
        }
        let g = self.dbs.read();
        let mut snap = match g.get(db) {
            None => SchemaSnapshot::default(),
            Some(d) => {
                let mut relations = Vec::new();
                let mut columns = HashMap::new();
                let mut schemas: Vec<&String> = d.relations.keys().collect();
                schemas.sort();
                for s in schemas {
                    for r in &d.relations[s] {
                        if let Some(c) = d.columns.get(&(r.schema.clone(), r.name.clone())) {
                            columns.insert(relations.len(), c.clone());
                        }
                        relations.push(r.clone());
                    }
                }
                let mut s = SchemaSnapshot::new(
                    d.search_path.clone().unwrap_or_else(|| vec!["public".into()]),
                    d.schemas.clone().unwrap_or_default(),
                    relations,
                    columns,
                );
                s.loaded = d.relations.keys().cloned().collect();
                s.complete = d.preloaded;
                s.catalog = d.catalog.clone();
                s
            }
        };
        snap.dialect = self.dialect;
        let snap = Arc::new(snap);
        self.snapshots.lock().insert(db.to_string(), (v, snap.clone()));
        snap
    }
}

impl Connection {
    pub fn databases(self: &Arc<Self>) -> Task<Vec<String>> {
        let this = self.clone();
        self.spawn(async move {
            if let Some(v) = this.schema.cached_databases() {
                return Ok(v);
            }
            let client = this.meta(this.database()).await?;
            let v = match read_databases(&client).await {
                Ok(v) => v,
                Err(e) => {
                    this.schema.note(format!("list of databases unavailable: {e}"));
                    vec![]
                }
            };
            let mut v = v;
            if !this.database().is_empty() && !v.iter().any(|d| d == this.database()) {
                v.insert(0, this.database().to_string());
            }
            *this.schema.databases.write() = Some(v.clone());
            this.schema.bump();
            Ok(v)
        })
    }

    pub fn schemas(self: &Arc<Self>, db: &str) -> Task<Vec<String>> {
        let (this, db) = (self.clone(), db.to_string());
        self.spawn(async move {
            if let Some(v) = this.schema.cached_schemas(&db) {
                return Ok(v);
            }
            let client = this.meta(&db).await?;
            let v = match read_schemas(&client, &db).await {
                Ok(v) => v,
                Err(e) => {
                    this.schema.note(format!("{db}: list of schemas unavailable: {e}"));
                    this.schema.search_path(&db)
                }
            };
            this.schema.dbs.write().entry(db).or_default().schemas = Some(v.clone());
            this.schema.bump();
            Ok(v)
        })
    }

    pub fn relations(self: &Arc<Self>, db: &str, schema: &str) -> Task<Vec<Relation>> {
        let (this, db, schema) = (self.clone(), db.to_string(), schema.to_string());
        self.spawn(async move { this.load_relations(&db, &schema).await })
    }

    async fn load_relations(&self, db: &str, schema: &str) -> Result<Vec<Relation>> {
        if let Some(v) = self.schema.cached_relations(db, schema) {
            return Ok(v);
        }
        let client = self.meta(db).await?;
        let mut v: Vec<Relation> = match read_relations(&client, schema).await {
            Ok(v) => v,
            Err(_) => {
                let is = format!("SELECT table_name, table_type FROM information_schema.tables WHERE table_schema = {}", lit(schema));
                match first_ok(&client, &[&is]).await {
                    Ok(rows) => rows
                        .into_iter()
                        .filter_map(|r| {
                            let kind = match r.get(1)?.as_deref()? {
                                "VIEW" => RelKind::View,
                                "FOREIGN" | "FOREIGN TABLE" => RelKind::Foreign,
                                _ => RelKind::Table,
                            };
                            Some(Relation { schema: schema.to_string(), name: r.first()?.clone()?, kind, oid: None })
                        })
                        .collect(),
                    Err(e) => {
                        self.schema.note(format!("{db}.{schema}: list of tables unavailable: {e}"));
                        vec![]
                    }
                }
            }
        };
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v.dedup_by(|a, b| a.name == b.name);
        self.schema.dbs.write().entry(db.to_string()).or_default().relations.insert(schema.to_string(), v.clone());
        self.schema.bump();
        Ok(v)
    }

    fn relation(&self, db: &str, schema: &str, rel: &str) -> Option<Relation> {
        self.schema.cached_relations(db, schema)?.into_iter().find(|r| r.name == rel)
    }

    pub fn columns(self: &Arc<Self>, db: &str, schema: &str, rel: &str) -> Task<Vec<Column>> {
        let (this, db, schema, rel) = (self.clone(), db.to_string(), schema.to_string(), rel.to_string());
        self.spawn(async move { this.load_columns(&db, &schema, &rel).await })
    }

    async fn load_columns(&self, db: &str, schema: &str, rel: &str) -> Result<Vec<Column>> {
        if let Some(v) = self.schema.cached_full_columns(db, schema, rel) {
            return Ok(v);
        }
        if self.relation(db, schema, rel).is_none() {
            self.load_relations(db, schema).await?;
        }
        let relation = self.relation(db, schema, rel);
        let client = self.meta(db).await?;
        let cols = match &relation {
            Some(r) => read_columns(&client, r).await,
            None => None,
        };
        let cols = match cols {
            Some(c) => c,
            None => {
                let q = format!(
                    "SELECT column_name, data_type, is_nullable, column_default FROM information_schema.columns \
                     WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
                    lit(schema),
                    lit(rel)
                );
                match first_ok(&client, &[&q]).await {
                    Ok(rows) => rows
                        .into_iter()
                        .filter_map(|r| {
                            Some(Column {
                                name: r.first()?.clone()?,
                                type_name: r.get(1).cloned().flatten().unwrap_or_default(),
                                nullable: parse_bool(r.get(2)?),
                                primary_key: false,
                                default: r.get(3).cloned().flatten(),
                            })
                        })
                        .collect(),
                    Err(e) => {
                        self.schema.note(format!("{schema}.{rel}: columns unavailable: {e}"));
                        vec![]
                    }
                }
            }
        };
        {
            let mut g = self.schema.dbs.write();
            let d = g.entry(db.to_string()).or_default();
            let key = (schema.to_string(), rel.to_string());
            d.brief.remove(&key);
            d.columns.insert(key, cols.clone());
        }
        self.schema.bump();
        Ok(cols)
    }

    pub fn indexes(self: &Arc<Self>, db: &str, schema: &str, rel: &str) -> Task<Vec<Index>> {
        let (this, db, schema, rel) = (self.clone(), db.to_string(), schema.to_string(), rel.to_string());
        self.spawn(async move {
            if let Some(v) = this.schema.cached_indexes(&db, &schema, &rel) {
                return Ok(v);
            }
            let cols = this.load_columns(&db, &schema, &rel).await?;
            let relation = this.relation(&db, &schema, &rel);
            let client = this.meta(&db).await?;
            let mut v = Vec::new();
            if let Some(r) = &relation {
                match read_indexes(&client, r, &cols).await {
                    Some(Ok(found)) => v = found,
                    Some(Err(e)) => this.schema.note(format!("{schema}.{rel}: indexes unavailable: {e}")),
                    None => {}
                }
            }
            v.sort_by(|a, b| b.primary.cmp(&a.primary).then_with(|| a.name.cmp(&b.name)));
            this.schema
                .dbs
                .write()
                .entry(db.clone())
                .or_default()
                .indexes
                .insert((schema.clone(), rel.clone()), v.clone());
            this.schema.bump();
            Ok(v)
        })
    }

    pub fn preload(self: &Arc<Self>, db: &str) -> Task<()> {
        let (this, db) = (self.clone(), db.to_string());
        self.spawn(async move {
            if !this.engine().is_sql() {
                return Ok(());
            }
            let schemas = this.schemas(&db).await?;
            let sp: Vec<String> = this.schema.search_path(&db).into_iter().filter(|s| schemas.contains(s)).collect();
            for s in &sp {
                this.load_relations(&db, s).await?;
            }
            let client = this.meta(&db).await?;
            let list = sp.iter().map(|s| lit(s)).collect::<Vec<_>>().join(", ");
            if list.is_empty() {
                return Ok(());
            }
            let is = format!(
                "SELECT table_schema, table_name, column_name, data_type, is_nullable, ordinal_position \
                 FROM information_schema.columns WHERE table_schema IN ({list})"
            );
            let rows = match read_columns_of_schemas(&client, &list).await {
                Ok(r) => r,
                Err(_) => match first_ok(&client, &[&is]).await {
                    Ok(r) => r,
                    Err(e) => {
                        this.schema.note(format!("{db}: columns for autocomplete unavailable: {e}"));
                        return Ok(());
                    }
                },
            };
            let mut by_rel: HashMap<RelKey, Vec<(i64, Column)>> = HashMap::new();
            for r in rows {
                let get = |i: usize| r.get(i).cloned().flatten();
                let (Some(s), Some(t), Some(name)) = (get(0), get(1), get(2)) else { continue };
                by_rel.entry((s, t)).or_default().push((
                    get(5).and_then(|n| n.parse().ok()).unwrap_or(0),
                    Column {
                        name,
                        type_name: get(3).unwrap_or_default(),
                        nullable: parse_bool(&get(4)),
                        primary_key: false,
                        default: None,
                    },
                ));
            }
            {
                let mut g = this.schema.dbs.write();
                let d = g.entry(db.clone()).or_default();
                for (k, mut v) in by_rel {
                    v.sort_by_key(|(n, _)| *n);
                    // Columns loaded individually carry more detail (PK, defaults): keep them.
                    if let std::collections::hash_map::Entry::Vacant(e) = d.columns.entry(k) {
                        d.brief.insert(e.key().clone());
                        e.insert(v.into_iter().map(|(_, c)| c).collect());
                    }
                }
                d.preloaded = sp.iter().all(|s| d.relations.contains_key(s));
            }
            this.schema.bump();
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalidation_is_scoped() {
        let c = SchemaCache::new("db".into(), vec!["public".into()]);
        let rel = |s: &str, n: &str| Relation { schema: s.into(), name: n.into(), kind: RelKind::Table, oid: None };
        let col = Column { name: "id".into(), type_name: "int".into(), nullable: None, primary_key: false, default: None };
        {
            let mut g = c.dbs.write();
            let d = g.get_mut("db").unwrap();
            d.schemas = Some(vec!["public".into(), "s".into()]);
            d.relations.insert("public".into(), vec![rel("public", "a"), rel("public", "b")]);
            d.relations.insert("s".into(), vec![rel("s", "a")]);
            for (s, n) in [("public", "a"), ("public", "b"), ("s", "a")] {
                d.columns.insert((s.into(), n.into()), vec![col.clone()]);
                d.indexes.insert((s.into(), n.into()), vec![]);
            }
        }
        let v = c.version();
        c.invalidate_relation("db", "public", "a");
        assert!(c.version() > v);
        assert!(c.cached_columns("db", "public", "a").is_none() && c.cached_indexes("db", "public", "a").is_none());
        assert!(c.cached_columns("db", "public", "b").is_some() && c.cached_relations("db", "public").is_some());
        c.invalidate_schema("db", "public");
        assert!(c.cached_relations("db", "public").is_none() && c.cached_columns("db", "public", "b").is_none());
        assert!(c.cached_relations("db", "s").is_some() && c.cached_columns("db", "s", "a").is_some());
        assert_eq!(c.cached_schemas("db").unwrap().len(), 2);
        c.invalidate_schema("nope", "public");
    }

    #[test]
    fn brief_columns_are_not_full() {
        let c = SchemaCache::new("db".into(), vec!["public".into()]);
        let col = Column { name: "id".into(), type_name: "int".into(), nullable: Some(false), primary_key: false, default: None };
        {
            let mut g = c.dbs.write();
            let d = g.get_mut("db").unwrap();
            for n in ["a", "b"] {
                d.columns.insert(("public".into(), n.into()), vec![col.clone()]);
            }
            d.brief.insert(("public".into(), "a".into()));
        }
        assert!(c.cached_columns("db", "public", "a").is_some());
        assert!(c.cached_full_columns("db", "public", "a").is_none());
        assert!(c.cached_full_columns("db", "public", "b").is_some());
        c.invalidate_relation("db", "public", "a");
        c.dbs.write().get_mut("db").unwrap().columns.insert(("public".into(), "a".into()), vec![col]);
        assert!(c.cached_full_columns("db", "public", "a").is_some());
    }

    #[test]
    fn helpers() {
        assert_eq!(lit("o'k"), "'o''k'");
        let c = SchemaCache::with_dialect("db".into(), vec![], Dialect::MySql);
        assert_eq!(c.snapshot("db").dialect, Dialect::MySql);
        assert_eq!(SchemaCache::new("db".into(), vec![]).snapshot("nope").dialect, Dialect::Postgres);
    }
}
