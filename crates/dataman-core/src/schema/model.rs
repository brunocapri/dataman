//! What the schema tree and autocomplete know about a database.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::catalog::Catalog;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RelKind {
    Table,
    View,
    MaterializedView,
    Foreign,
    Partitioned,
}

impl RelKind {
    pub fn from_relkind(c: &str) -> Option<RelKind> {
        Some(match c {
            "r" => RelKind::Table,
            "v" => RelKind::View,
            "m" => RelKind::MaterializedView,
            "f" => RelKind::Foreign,
            "p" => RelKind::Partitioned,
            _ => return None,
        })
    }

    pub fn is_view(self) -> bool {
        matches!(self, RelKind::View | RelKind::MaterializedView)
    }

    pub fn label(self) -> &'static str {
        match self {
            RelKind::Table => "table",
            RelKind::View => "view",
            RelKind::MaterializedView => "materialized view",
            RelKind::Foreign => "foreign table",
            RelKind::Partitioned => "partitioned table",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    pub type_name: String,
    /// `None` when the catalog does not say.
    pub nullable: Option<bool>,
    pub primary_key: bool,
    pub default: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Index {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    /// `pg_get_indexdef`, when the server has it.
    pub definition: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Relation {
    pub schema: String,
    pub name: String,
    pub kind: RelKind,
    pub oid: Option<u32>,
}

/// A flat, immutable view of everything cached for one database, used by autocomplete.
#[derive(Clone, Debug, Default)]
pub struct SchemaSnapshot {
    /// Schemas searched for unqualified names, in order.
    pub search_path: Vec<String>,
    pub schemas: Vec<String>,
    pub relations: Vec<Relation>,
    /// Columns per relation index, when loaded.
    pub columns: HashMap<usize, Vec<Column>>,
    /// Schemas whose list of relations is loaded.
    pub loaded: HashSet<String>,
    /// Every relation of the search path's schemas has its columns loaded (a relation of
    /// theirs without an entry in `columns` has none).
    pub complete: bool,
    /// Functions and types (`pg_proc`, `pg_type`), once loaded for the connection.
    pub catalog: Option<Arc<Catalog>>,
    /// The connection's SQL dialect: what completion, lint and parameters read the editor's
    /// text as.
    pub dialect: crate::sql::Dialect,
    by_name: HashMap<String, Vec<usize>>,
}

impl SchemaSnapshot {
    pub fn new(
        search_path: Vec<String>,
        schemas: Vec<String>,
        relations: Vec<Relation>,
        columns: HashMap<usize, Vec<Column>>,
    ) -> Self {
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, r) in relations.iter().enumerate() {
            by_name.entry(r.name.to_lowercase()).or_default().push(i);
        }
        let loaded = relations.iter().map(|r| r.schema.clone()).collect();
        SchemaSnapshot { search_path, schemas, relations, columns, loaded, complete: false, catalog: None, dialect: Default::default(), by_name }
    }

    /// The columns of relation `rel` when they are known (an empty list is a known empty table).
    pub fn known_columns(&self, rel: usize) -> Option<&[Column]> {
        if let Some(c) = self.columns.get(&rel) {
            return Some(c);
        }
        let r = self.relations.get(rel)?;
        (self.complete && self.search_path.contains(&r.schema)).then_some(&[])
    }

    /// Every schema an unqualified name may come from has its relations loaded.
    pub fn search_path_loaded(&self) -> bool {
        !self.search_path.is_empty()
            && self.search_path.iter().filter(|s| self.schemas.is_empty() || self.schemas.contains(s)).all(|s| self.loaded.contains(s))
    }

    /// Resolves `[schema.]name` (already folded/unquoted) to a relation index, preferring the
    /// search path for unqualified names.
    pub fn resolve(&self, schema: Option<&str>, name: &str) -> Option<usize> {
        let cands = self.by_name.get(&name.to_lowercase())?;
        let exact = |i: &&usize| self.relations[**i].name == name;
        let pick = |pred: &dyn Fn(&usize) -> bool| {
            cands.iter().filter(|i| pred(i)).find(exact).or_else(|| cands.iter().find(|i| pred(i))).copied()
        };
        match schema {
            Some(s) => pick(&|i| self.relations[*i].schema.eq_ignore_ascii_case(s)),
            None => {
                for s in &self.search_path {
                    if let Some(i) = pick(&|i| &self.relations[*i].schema == s) {
                        return Some(i);
                    }
                }
                pick(&|_| true)
            }
        }
    }

    pub fn columns_of(&self, rel: usize) -> &[Column] {
        self.columns.get(&rel).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn is_schema(&self, name: &str) -> Option<&str> {
        self.schemas.iter().find(|s| s.eq_ignore_ascii_case(name)).map(String::as_str)
    }
}
