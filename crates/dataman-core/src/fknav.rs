use std::collections::BTreeSet;
use std::sync::Arc;

use crate::Task;
use crate::conn::{Connection, first_ok};
use crate::edit::{analyze_select_in, map_columns};
use crate::relations::Link;
use crate::result::{Cell, ResultSet};
use crate::sql::Dialect;
use crate::sqlgen::literal_in;
use crate::value::Kind;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ColumnSource {
    pub schema: String,
    pub table: String,
    pub column: String,
}

impl ColumnSource {
    pub fn new(schema: &str, table: &str, column: &str) -> ColumnSource {
        ColumnSource { schema: schema.into(), table: table.into(), column: column.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FkTarget {
    pub from_schema: String,
    pub to_schema: String,
    pub to_table: String,
    pub to_columns: Vec<String>,
    pub from_cols: Vec<usize>,
    pub inferred: bool,
}

impl FkTarget {
    pub fn label(&self) -> String {
        if self.to_schema == self.from_schema {
            format!("Go to \"{}\"", self.to_table)
        } else {
            format!("Go to \"{}.{}\"", self.to_schema, self.to_table)
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForeignKeys {
    cols: Vec<Vec<FkTarget>>,
    dialect: Dialect,
}

impl ForeignKeys {
    pub fn jump(&self, t: &FkTarget, values: &[(Kind, Cell<'_>)]) -> Option<FkJump> {
        jump_in(t, values, self.dialect)
    }

    pub fn targets(&self, col: usize) -> &[FkTarget] {
        self.cols.get(col).map_or(&[], Vec::as_slice)
    }

    pub fn is_fk(&self, col: usize) -> bool {
        !self.targets(col).is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.cols.iter().all(Vec::is_empty)
    }

    pub fn flags(&self) -> Vec<bool> {
        self.cols.iter().map(|t| !t.is_empty()).collect()
    }
}

pub fn resolve(sources: &[Option<ColumnSource>], links: &[Link]) -> ForeignKeys {
    let mut cols: Vec<Vec<FkTarget>> = vec![Vec::new(); sources.len()];
    let positions = |schema: &str, table: &str, column: &str| -> Vec<usize> {
        sources
            .iter()
            .enumerate()
            .filter(|(_, s)| s.as_ref().is_some_and(|s| s.schema == schema && s.table == table && s.column == column))
            .map(|(i, _)| i)
            .collect()
    };
    for l in links {
        let to_columns = match (l.to_columns.is_empty(), l.inferred) {
            (false, _) => l.to_columns.clone(),
            (true, true) if l.from_columns.len() == 1 => vec!["id".to_string()],
            _ => continue,
        };
        if to_columns.len() != l.from_columns.len() || l.from_columns.is_empty() {
            continue;
        }
        let target = |from_cols: Vec<usize>| FkTarget {
            from_schema: l.from_schema.clone(),
            to_schema: l.to_schema.clone(),
            to_table: l.to_table.clone(),
            to_columns: to_columns.clone(),
            from_cols,
            inferred: l.inferred,
        };
        if let [c] = l.from_columns.as_slice() {
            for i in positions(&l.from_schema, &l.from_table, c) {
                cols[i].push(target(vec![i]));
            }
            continue;
        }
        let found: Vec<Vec<usize>> = l.from_columns.iter().map(|c| positions(&l.from_schema, &l.from_table, c)).collect();
        if found.iter().any(|p| p.len() != 1) {
            continue;
        }
        let from_cols: Vec<usize> = found.into_iter().map(|p| p[0]).collect();
        for &i in &from_cols {
            cols[i].push(target(from_cols.clone()));
        }
    }
    ForeignKeys { cols, dialect: Dialect::Postgres }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FkJump {
    pub sql: String,
    pub title: String,
    pub label: String,
    pub detail: String,
    pub key: String,
}

const SHOWN: usize = 32;

fn shorten(s: &str) -> String {
    if s.chars().count() <= SHOWN {
        s.to_string()
    } else {
        s.chars().take(SHOWN - 1).collect::<String>() + "…"
    }
}

pub fn jump(t: &FkTarget, values: &[(Kind, Cell<'_>)]) -> Option<FkJump> {
    jump_in(t, values, Dialect::Postgres)
}

pub fn jump_in(t: &FkTarget, values: &[(Kind, Cell<'_>)], d: Dialect) -> Option<FkJump> {
    if values.len() != t.to_columns.len() || values.iter().any(|(_, c)| matches!(c, Cell::Null)) {
        return None;
    }
    let lits: Vec<String> = values.iter().map(|(k, c)| literal_in(d, *k, *c)).collect();
    let cond = t.to_columns.iter().zip(&lits).map(|(c, v)| format!("{} = {v}", d.quote_ident(c))).collect::<Vec<_>>().join(" AND ");
    let sql = format!("SELECT * FROM {} WHERE {cond};", d.qualified(Some(&t.to_schema), &t.to_table));
    let shown = t.to_columns.iter().zip(&lits).map(|(c, v)| format!("{c} = {}", shorten(v))).collect::<Vec<_>>().join(", ");
    let detail = if t.inferred { format!("{shown} · inferred") } else { shown.clone() };
    let key = t.to_columns.first().cloned().unwrap_or_default();
    Some(FkJump { sql, title: format!("{} · {shown}", t.to_table), label: t.label(), detail, key })
}

impl Connection {
    pub fn foreign_keys(self: &Arc<Self>, database: &str, rs: &Arc<ResultSet>) -> Task<Arc<ForeignKeys>> {
        let (this, db, rs) = (self.clone(), database.to_string(), rs.clone());
        self.spawn(async move {
            let mut rx = rs.subscribe();
            let cols = loop {
                if let Some(c) = rs.columns() {
                    break c;
                }
                if rs.phase().is_finished() || rx.changed().await.is_err() {
                    return Ok(Arc::new(ForeignKeys::default()));
                }
            };
            if cols.is_empty() {
                return Ok(Arc::new(ForeignKeys::default()));
            }
            let sources = this.column_sources(&db, &rs, cols.len()).await;
            let schemas: BTreeSet<&str> = sources.iter().flatten().map(|s| s.schema.as_str()).collect();
            let mut links = Vec::new();
            for s in schemas {
                if let Ok(g) = this.relationships(&db, s).await {
                    links.extend(g.links.iter().cloned());
                }
            }
            let mut fk = resolve(&sources, &links);
            fk.dialect = this.dialect();
            Ok(Arc::new(fk))
        })
    }

    async fn column_sources(self: &Arc<Self>, db: &str, rs: &ResultSet, n: usize) -> Vec<Option<ColumnSource>> {
        if let Some(named) = rs.named_origins()
            && named.len() == n
        {
            return named.iter().map(|o| o.as_ref().map(|o| ColumnSource::new(&o.schema, &o.table, &o.column))).collect();
        }
        let origins = rs.origins().unwrap_or_default();
        let oids: BTreeSet<u32> = origins.iter().filter_map(|o| o.table_oid).collect();
        if !self.info.is_lar && !oids.is_empty() {
            if let Some(s) = self.sources_from_catalog(db, &origins, &oids).await {
                return s;
            }
        }
        self.sources_from_sql(db, rs, n).await.unwrap_or_else(|| vec![None; n])
    }

    async fn sources_from_catalog(&self, db: &str, origins: &[crate::result::ColumnOrigin], oids: &BTreeSet<u32>) -> Option<Vec<Option<ColumnSource>>> {
        let client = self.meta(db).await.ok()?;
        let list = oids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT c.oid, n.nspname, c.relname, a.attnum, a.attname FROM pg_catalog.pg_attribute a \
             JOIN pg_catalog.pg_class c ON c.oid = a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE a.attrelid IN ({list}) AND a.attnum > 0 AND NOT a.attisdropped"
        );
        let rows = first_ok(&client, &[&sql]).await.ok()?;
        let find = |oid: u32, att: i16| {
            rows.iter().find_map(|r| {
                let get = |i: usize| r.get(i).cloned().flatten();
                if get(0)? != oid.to_string() || get(3)? != att.to_string() {
                    return None;
                }
                Some(ColumnSource { schema: get(1)?, table: get(2)?, column: get(4)? })
            })
        };
        Some(origins.iter().map(|o| find(o.table_oid?, o.column_id?)).collect())
    }

    async fn sources_from_sql(self: &Arc<Self>, db: &str, rs: &ResultSet, n: usize) -> Option<Vec<Option<ColumnSource>>> {
        let shape = analyze_select_in(&rs.sql, self.dialect()).ok()?;
        let schema = match &shape.table.schema {
            Some(s) => s.clone(),
            None => {
                let mut found = None;
                for s in self.schema.search_path(db) {
                    if let Ok(rels) = self.relations(db, &s).await
                        && rels.iter().any(|r| r.name == shape.table.name)
                    {
                        found = Some(s);
                        break;
                    }
                }
                found?
            }
        };
        let table_cols: Vec<String> = self.columns(db, &schema, &shape.table.name).await.ok()?.into_iter().map(|c| c.name).collect();
        let result_cols: Vec<String> = rs.columns()?.iter().map(|c| c.name.clone()).collect();
        let mapped = map_columns(&shape.items, &table_cols, &result_cols).ok()?;
        if mapped.len() != n {
            return None;
        }
        Some(mapped.into_iter().map(|c| c.map(|c| ColumnSource::new(&schema, &shape.table.name, &c))).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(from: &str, cols: &[&str], to: &str, to_cols: &[&str], inferred: bool) -> Link {
        Link {
            from_schema: "public".into(),
            from_table: from.into(),
            from_columns: cols.iter().map(|c| c.to_string()).collect(),
            to_schema: "public".into(),
            to_table: to.into(),
            to_columns: to_cols.iter().map(|c| c.to_string()).collect(),
            inferred,
        }
    }

    fn src(t: &str, c: &str) -> Option<ColumnSource> {
        Some(ColumnSource::new("public", t, c))
    }

    #[test]
    fn single_column_keys() {
        let sources = [src("orders", "id"), src("orders", "customer"), src("orders", "total"), src("customer", "name")];
        let links = [link("orders", &["customer"], "customer", &["id"], false), link("item", &["order_id"], "orders", &["id"], false)];
        let fk = resolve(&sources, &links);
        assert_eq!(fk.flags(), vec![false, true, false, false]);
        let t = &fk.targets(1)[0];
        assert_eq!((t.to_table.as_str(), t.to_columns.clone(), t.from_cols.clone()), ("customer", vec!["id".to_string()], vec![1]));
        assert!(fk.targets(7).is_empty(), "out of range is no target");
        let twice = [src("orders", "customer"), src("orders", "customer")];
        let fk = resolve(&twice, &links);
        assert_eq!(fk.targets(0)[0].from_cols, vec![0]);
        assert_eq!(fk.targets(1)[0].from_cols, vec![1]);
        let two = [link("orders", &["customer"], "customer", &["id"], false), link("orders", &["customer"], "account", &["id"], false)];
        assert_eq!(resolve(&[src("orders", "customer")], &two).targets(0).len(), 2);
    }

    #[test]
    fn composite_keys_need_every_column_once() {
        let l = [link("item", &["order_id", "line"], "order_line", &["order_id", "n"], false)];
        let both = [src("item", "line"), src("item", "qty"), src("item", "order_id")];
        let fk = resolve(&both, &l);
        assert_eq!(fk.flags(), vec![true, false, true]);
        assert_eq!(fk.targets(0)[0].from_cols, vec![2, 0]);
        assert_eq!(fk.targets(2)[0], fk.targets(0)[0]);
        assert!(resolve(&[src("item", "order_id"), src("item", "qty")], &l).is_empty());
        assert!(resolve(&[src("item", "order_id"), src("item", "line"), src("item", "order_id")], &l).is_empty());
    }

    #[test]
    fn unknown_sources_and_other_tables_are_not_references() {
        let l = [link("orders", &["customer"], "customer", &["id"], false)];
        assert!(resolve(&[None, None], &l).is_empty());
        assert!(resolve(&[src("invoice", "customer")], &l).is_empty());
        assert!(resolve(&[Some(ColumnSource::new("archive", "orders", "customer"))], &l).is_empty());
        assert!(resolve(&[], &l).is_empty());
    }

    #[test]
    fn lar_links_lead_to_the_id() {
        let l = [link("orders", &["customer"], "customer", &[], true)];
        let fk = resolve(&[src("orders", "customer")], &l);
        let t = &fk.targets(0)[0];
        assert!(t.inferred);
        assert_eq!(t.to_columns, vec!["id".to_string()]);
        let j = jump(t, &[(Kind::Int, Cell::Int(42))]).unwrap();
        assert_eq!(j.detail, "id = 42 · inferred");
        assert!(resolve(&[src("orders", "customer")], &[link("orders", &["customer"], "customer", &[], false)]).is_empty());
    }

    #[test]
    fn statements_quote_by_type() {
        let t = FkTarget {
            from_schema: "public".into(),
            to_schema: "public".into(),
            to_table: "customer".into(),
            to_columns: vec!["id".into()],
            from_cols: vec![1],
            inferred: false,
        };
        let j = jump(&t, &[(Kind::Int, Cell::Int(42))]).unwrap();
        assert_eq!(j.sql, "SELECT * FROM public.customer WHERE id = 42;");
        assert_eq!(j.title, "customer · id = 42");
        assert_eq!(j.label, "Go to \"customer\"");
        assert_eq!(j.detail, "id = 42");
        assert_eq!(j.key, "id", "the column the new tab's first row pops on");
        assert_eq!(jump(&t, &[(Kind::Int, Cell::Null)]), None);
        assert_eq!(jump(&t, &[]), None, "values must match the key");
        let odd = FkTarget {
            to_schema: "Sales".into(),
            to_table: "Région".into(),
            to_columns: vec!["code".into(), "user".into()],
            from_cols: vec![0, 1],
            ..t.clone()
        };
        let j = jump(&odd, &[(Kind::Text, Cell::Text("Zoë d'Arcy")), (Kind::Int, Cell::Int(-7))]).unwrap();
        assert_eq!(j.sql, "SELECT * FROM \"Sales\".\"Région\" WHERE code = 'Zoë d''Arcy' AND \"user\" = -7;");
        assert_eq!(j.label, "Go to \"Sales.Région\"");
        assert_eq!(j.title, "Région · code = 'Zoë d''Arcy', user = -7");
        let ts = jump(&t, &[(Kind::TimestampTz, Cell::Int(0))]).unwrap();
        assert_eq!(ts.sql, "SELECT * FROM public.customer WHERE id = '2000-01-01 00:00:00+00';");
        assert!(jump(&t, &[(Kind::Numeric, Cell::Text("12.50"))]).unwrap().sql.ends_with("id = 12.50;"));
        assert!(jump(&t, &[(Kind::Other, Cell::Text("8c4e…"))]).unwrap().sql.ends_with("id = '8c4e…';"));
        let long = "x".repeat(80);
        let j = jump(&t, &[(Kind::Text, Cell::Text(&long))]).unwrap();
        assert!(j.sql.contains(&long));
        assert!(j.title.chars().count() < 60 && j.title.ends_with('…'));
    }
}
