use std::collections::HashMap;
use std::sync::Arc;

use super::objects::split_args;
use super::pg::catalog as pg;
use crate::Task;
use crate::backend::Db;
use crate::conn::{Connection, first_ok};

#[derive(Clone, Debug, PartialEq)]
pub struct FuncSig {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub args: String,
    pub result: String,
    pub kind: char,
    pub returns_set: bool,
}

impl FuncSig {
    pub fn params(&self) -> Vec<String> {
        split_args(&self.args).into_iter().filter(|p| !p.starts_with("OUT ")).collect()
    }

    pub fn signature(&self) -> String {
        if self.kind == 'p' { format!("{}({})", self.name, self.args) } else { format!("{}({}) → {}", self.name, self.args, self.result) }
    }

    pub fn variadic(&self) -> bool {
        self.params().last().is_some_and(|p| p.starts_with("VARIADIC"))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypeEntry {
    pub schema: String,
    pub name: String,
    pub kind: char,
    pub labels: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Catalog {
    pub functions: Vec<FuncSig>,
    pub types: Vec<TypeEntry>,
    by_name: HashMap<String, Vec<usize>>,
}

pub const BUILTIN_TYPES: &[&str] = &[
    "bigint", "bigserial", "bit", "boolean", "box", "bytea", "char", "cidr", "circle", "date", "daterange",
    "decimal", "double precision", "float4", "float8", "inet", "int", "int2", "int4", "int4range", "int8",
    "int8range", "integer", "interval", "json", "jsonb", "jsonpath", "line", "lseg", "macaddr", "money", "name",
    "numeric", "numrange", "oid", "path", "point", "polygon", "real", "regclass", "regproc", "regtype", "serial",
    "smallint", "text", "time", "timestamp", "timestamptz", "timetz", "tsquery", "tsrange", "tstzrange",
    "tsvector", "uuid", "varchar", "xml",
];

impl Catalog {
    pub fn new(mut functions: Vec<FuncSig>, types: Vec<TypeEntry>) -> Catalog {
        functions.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.schema.cmp(&b.schema)).then_with(|| a.args.len().cmp(&b.args.len())));
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, f) in functions.iter().enumerate() {
            by_name.entry(f.name.to_lowercase()).or_default().push(i);
        }
        Catalog { functions, types, by_name }
    }

    pub fn functions_named<'a>(&'a self, schema: Option<&'a str>, name: &str) -> impl Iterator<Item = &'a FuncSig> + 'a {
        self.by_name
            .get(&name.to_lowercase())
            .into_iter()
            .flatten()
            .map(|&i| &self.functions[i])
            .filter(move |f| schema.is_none_or(|s| f.schema.eq_ignore_ascii_case(s)))
    }

    pub fn has_function(&self, name: &str) -> bool {
        self.by_name.contains_key(&name.to_lowercase())
    }

    pub fn type_named(&self, type_name: &str, search_path: &[String]) -> Option<&TypeEntry> {
        let t = type_name.trim();
        if t.ends_with("[]") {
            return None;
        }
        let unq = |s: &str| s.trim_matches('"').replace("\"\"", "\"");
        let (schema, name) = match t.rsplit_once('.') {
            Some((s, n)) if !n.contains('"') || n.starts_with('"') => (Some(unq(s)), unq(n)),
            _ => (None, unq(t)),
        };
        let mut cands = self.types.iter().filter(|e| e.name == name);
        match schema {
            Some(s) => cands.find(|e| e.schema == s),
            None => {
                let all: Vec<&TypeEntry> = cands.collect();
                search_path.iter().find_map(|s| all.iter().find(|e| &e.schema == s).copied()).or_else(|| all.first().copied())
            }
        }
    }

    pub fn enum_labels(&self, type_name: &str, search_path: &[String]) -> Option<&[String]> {
        self.type_named(type_name, search_path).filter(|t| t.kind == 'e').map(|t| t.labels.as_slice())
    }
}

impl super::SchemaCache {
    pub fn cached_catalog(&self, db: &str) -> Option<Arc<Catalog>> {
        self.dbs.read().get(db).and_then(|d| d.catalog.clone())
    }
}

impl Connection {
    pub fn catalog(self: &Arc<Self>, db: &str) -> Task<Arc<Catalog>> {
        let (this, db) = (self.clone(), db.to_string());
        self.spawn(async move {
            if let Some(c) = this.schema.cached_catalog(&db) {
                return Ok(c);
            }
            let client = this.meta(&db).await?;
            let (functions, types) = match &client {
                Db::Pg(_) => {
                    let fq = pg::functions_sql();
                    let functions = match first_ok(&client, &[&fq[0], &fq[1], &fq[2]]).await {
                        Ok(rows) => rows.iter().filter_map(|r| pg::parse_function(r)).collect(),
                        Err(e) => {
                            this.schema.note(format!("{db}: functions for autocomplete unavailable: {e}"));
                            Vec::new()
                        }
                    };
                    let types = first_ok(&client, &[&pg::types_sql()]).await.map(|rows| rows.iter().filter_map(|r| pg::parse_type(r)).collect()).unwrap_or_default();
                    (functions, types)
                }
                Db::My(_) => {
                    let functions = match first_ok(&client, &[&super::my::routines_sql(&db)]).await {
                        Ok(rows) => rows.iter().filter_map(|r| super::my::parse_routine(r)).collect(),
                        Err(e) => {
                            this.schema.note(format!("{db}: routines for autocomplete unavailable: {e}"));
                            Vec::new()
                        }
                    };
                    (functions, Vec::new())
                }
                Db::Lite(_) => (super::lite::functions(&client).await, Vec::new()),
                Db::Ms(_) => {
                    let functions = match first_ok(&client, &[&super::ms::routines_sql(None)]).await {
                        Ok(rows) => super::ms::parse_routines(&rows),
                        Err(e) => {
                            this.schema.note(format!("{db}: routines for autocomplete unavailable: {e}"));
                            Vec::new()
                        }
                    };
                    (functions, Vec::new())
                }
            };
            let cat = Arc::new(Catalog::new(functions, types));
            this.schema.dbs.write().entry(db).or_default().catalog = Some(cat.clone());
            this.schema.bump();
            Ok(cat)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(schema: &str, name: &str, args: &str, result: &str) -> FuncSig {
        FuncSig { oid: 1, schema: schema.into(), name: name.into(), args: args.into(), result: result.into(), kind: 'f', returns_set: false }
    }

    #[test]
    fn lookups() {
        let cat = Catalog::new(
            vec![f("pg_catalog", "date_trunc", "text, timestamp with time zone", "timestamp with time zone"), f("pg_catalog", "date_trunc", "text, interval", "interval"), f("public", "customer_total", "c bigint", "numeric")],
            vec![
                TypeEntry { schema: "public".into(), name: "mood".into(), kind: 'e', labels: vec!["ok".into(), "sad".into()] },
                TypeEntry { schema: "app".into(), name: "mood".into(), kind: 'e', labels: vec!["x".into()] },
            ],
        );
        assert_eq!(cat.functions_named(None, "DATE_TRUNC").count(), 2);
        assert_eq!(cat.functions_named(Some("public"), "date_trunc").count(), 0);
        let sp = vec!["public".to_string()];
        assert_eq!(cat.enum_labels("mood", &sp).unwrap(), &["ok".to_string(), "sad".to_string()]);
        assert_eq!(cat.enum_labels("app.mood", &sp).unwrap(), &["x".to_string()]);
        assert!(cat.enum_labels("mood[]", &sp).is_none());
        assert!(cat.enum_labels("text", &sp).is_none());
        let t = f("public", "x", "a integer, VARIADIC b text[]", "void");
        assert!(t.variadic());
        assert_eq!(t.params(), vec!["a integer", "VARIADIC b text[]"]);
        assert_eq!(f("public", "g", "a int, OUT b int", "integer").params(), vec!["a int"]);
    }
}
