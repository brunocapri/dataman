use std::sync::Arc;

use anyhow::Result;

use crate::Task;
use crate::conn::{Connection, first_ok};
use crate::sqlgen::{qualified, quote_ident, quote_literal};

#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub from_schema: String,
    pub from_table: String,
    pub from_columns: Vec<String>,
    pub to_schema: String,
    pub to_table: String,
    pub to_columns: Vec<String>,
    pub inferred: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RelationGraph {
    pub links: Vec<Link>,
}

#[derive(Clone, Debug, PartialEq)]
struct TableNode {
    name: String,
    columns: Vec<(String, String)>,
    primary_key: Option<String>,
}

impl TableNode {
    fn has(&self, column: &str) -> bool {
        self.columns.iter().any(|c| c.0 == column)
    }
}

const CACHE: &str = "relations";

fn is_integer_type(t: &str) -> bool {
    matches!(
        t.to_ascii_lowercase().as_str(),
        "bigint" | "integer" | "smallint" | "int" | "int2" | "int4" | "int8" | "int64" | "bigserial" | "serial"
    )
}

fn singulars(t: &str) -> Vec<String> {
    let mut v = vec![t.to_string()];
    let mut add = |s: String| {
        if !s.is_empty() && !v.contains(&s) {
            v.push(s);
        }
    };
    if let Some(b) = t.strip_suffix("ies") {
        add(format!("{b}y"));
    }
    if let Some(b) = t.strip_suffix("es") {
        add(b.to_string());
    }
    if let Some(b) = t.strip_suffix('s') {
        add(b.to_string());
    }
    v
}

fn bases(col: &str) -> Vec<String> {
    let c = col.to_lowercase();
    let mut v = vec![c.clone()];
    for suf in ["_id", "_fk", "_ref", "_key", "id"] {
        if let Some(b) = c.strip_suffix(suf)
            && b.len() >= 2
        {
            v.push(b.trim_end_matches('_').to_string());
        }
    }
    for pre in ["id_", "fk_", "ref_"] {
        if let Some(b) = c.strip_prefix(pre)
            && b.len() >= 2
        {
            v.push(b.to_string());
        }
    }
    v.dedup();
    v
}

fn link_candidates(col: &str, table: &str, tables: &[&str]) -> Vec<String> {
    let bs = bases(col);
    let mut ranked: Vec<(usize, usize, String)> = Vec::new();
    for t in tables {
        if t.eq_ignore_ascii_case(table) {
            continue;
        }
        let lt = t.to_lowercase();
        let sing = singulars(&lt);
        let mut best = None;
        for (bi, b) in bs.iter().enumerate() {
            let score = if &lt == b {
                Some(0)
            } else if sing.iter().skip(1).any(|s| s == b) {
                Some(1)
            } else {
                None
            };
            if let Some(s) = score {
                let key = (s, bi);
                if best.is_none_or(|x| key < x) {
                    best = Some(key);
                }
            }
        }
        if let Some((s, bi)) = best {
            ranked.push((s, bi, t.to_string()));
        }
    }
    ranked.sort();
    ranked.into_iter().map(|r| r.2).collect()
}

fn probe_column(cand: &TableNode, source: &TableNode, others: &[&TableNode]) -> String {
    let fresh: Vec<&str> = cand.columns.iter().map(|c| c.0.as_str()).filter(|c| !source.has(c)).collect();
    fresh
        .iter()
        .find(|c| !others.iter().any(|o| o.name != cand.name && o.has(c)))
        .or(fresh.first())
        .map(|c| c.to_string())
        .unwrap_or_else(|| cand.primary_key.clone().unwrap_or_else(|| "id".into()))
}

impl Connection {
    pub fn relationships(self: &Arc<Self>, database: &str, schema: &str) -> Task<Arc<RelationGraph>> {
        let (this, db, schema) = (self.clone(), database.to_string(), schema.to_string());
        self.spawn(async move {
            let key = (CACHE, db.clone(), schema.clone());
            if let Some(g) = this.caches.lock().get(&key).cloned().and_then(|a| a.downcast::<RelationGraph>().ok()) {
                return Ok(g);
            }
            let g = if this.info.is_lar {
                this.lar_graph(&db, &schema).await?
            } else if this.engine() == crate::config::Engine::MySql {
                this.my_graph(&db, &schema).await?
            } else if this.engine() == crate::config::Engine::SqlServer {
                this.ms_graph(&db, &schema).await?
            } else {
                this.pg_graph(&db, &schema).await?
            };
            let g = Arc::new(g);
            this.caches.lock().insert(key, g.clone());
            Ok(g)
        })
    }

    async fn pg_graph(&self, db: &str, schema: &str) -> Result<RelationGraph> {
        let client = self.meta(db).await?;
        let s = quote_literal(schema);
        let names = |rel: &str, keys: &str| {
            format!(
                "pg_catalog.array_to_string(ARRAY(SELECT pg_catalog.quote_ident(a.attname) FROM pg_catalog.unnest(con.{keys}) WITH ORDINALITY k(n, o) \
                 JOIN pg_catalog.pg_attribute a ON a.attrelid = con.{rel} AND a.attnum = k.n ORDER BY k.o), ',')"
            )
        };
        let sql = format!(
            "SELECT cl.relname, {}, fn.nspname, fc.relname, {} \
             FROM pg_catalog.pg_constraint con JOIN pg_catalog.pg_class cl ON cl.oid = con.conrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = cl.relnamespace \
             JOIN pg_catalog.pg_class fc ON fc.oid = con.confrelid \
             JOIN pg_catalog.pg_namespace fn ON fn.oid = fc.relnamespace \
             WHERE n.nspname = {s} AND con.contype = 'f' ORDER BY con.conname",
            names("conrelid", "conkey"),
            names("confrelid", "confkey"),
        );
        let split = |v: Option<String>| -> Vec<String> {
            v.filter(|s| !s.is_empty()).map(|s| split_quoted_idents(&s)).unwrap_or_default()
        };
        let mut g = RelationGraph::default();
        for r in first_ok(&client, &[&sql]).await? {
            let get = |i: usize| r.get(i).cloned().flatten();
            let (Some(table), Some(to_schema), Some(to_table)) = (get(0), get(2), get(3)) else { continue };
            g.links.push(Link {
                from_schema: schema.into(),
                from_table: table,
                from_columns: split(get(1)),
                to_schema,
                to_table,
                to_columns: split(get(4)),
                inferred: false,
            });
        }
        Ok(g)
    }

    async fn my_graph(&self, db: &str, schema: &str) -> Result<RelationGraph> {
        let sql = format!(
            "SELECT TABLE_NAME, CONSTRAINT_NAME, COLUMN_NAME, REFERENCED_TABLE_SCHEMA, REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
             FROM information_schema.KEY_COLUMN_USAGE \
             WHERE TABLE_SCHEMA = {} AND REFERENCED_TABLE_NAME IS NOT NULL \
             ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION",
            crate::schema::my::lit(schema)
        );
        self.key_column_graph(db, schema, &sql).await
    }

    async fn ms_graph(&self, db: &str, schema: &str) -> Result<RelationGraph> {
        let sql = format!(
            "SELECT OBJECT_NAME(fk.parent_object_id), fk.name, pc.name, SCHEMA_NAME(rt.schema_id), rt.name, rc.name \
               FROM sys.foreign_keys fk \
               JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
               JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
               JOIN sys.objects rt ON rt.object_id = fkc.referenced_object_id \
               JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
              WHERE fk.schema_id = SCHEMA_ID({}) \
              ORDER BY 1, 2, fkc.constraint_column_id",
            crate::schema::ms::lit(schema)
        );
        self.key_column_graph(db, schema, &sql).await
    }

    async fn key_column_graph(&self, db: &str, schema: &str, sql: &str) -> Result<RelationGraph> {
        let client = self.meta(db).await?;
        let mut g = RelationGraph::default();
        let mut last: Option<(String, String)> = None;
        for r in first_ok(&client, &[sql]).await? {
            let get = |i: usize| r.get(i).cloned().flatten();
            let (Some(table), Some(name), Some(col), Some(to_schema), Some(to_table), Some(to_col)) = (get(0), get(1), get(2), get(3), get(4), get(5)) else {
                continue;
            };
            let key = (table.clone(), name);
            match g.links.last_mut() {
                Some(l) if last.as_ref() == Some(&key) => {
                    l.from_columns.push(col);
                    l.to_columns.push(to_col);
                }
                _ => g.links.push(Link {
                    from_schema: schema.into(),
                    from_table: table,
                    from_columns: vec![col],
                    to_schema,
                    to_table,
                    to_columns: vec![to_col],
                    inferred: false,
                }),
            }
            last = Some(key);
        }
        Ok(g)
    }

    async fn lar_graph(self: &Arc<Self>, db: &str, schema: &str) -> Result<RelationGraph> {
        let mut tables = Vec::new();
        for r in self.relations(db, schema).await?.iter().filter(|r| !r.kind.is_view()) {
            let cols = self.columns(db, schema, &r.name).await.unwrap_or_default();
            let mut t = TableNode {
                name: r.name.clone(),
                columns: cols.iter().map(|c| (c.name.clone(), c.type_name.clone())).collect(),
                primary_key: None,
            };
            t.primary_key = lar_key_guess(&t);
            tables.push(t);
        }
        let client = self.meta(db).await?;
        let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        let mut g = RelationGraph::default();
        for t in &tables {
            for (c, _) in t.columns.iter().filter(|(c, ty)| is_integer_type(ty) && t.primary_key.as_ref() != Some(c)) {
                let cands = link_candidates(c, &t.name, &names);
                let cand_nodes: Vec<&TableNode> = cands.iter().filter_map(|n| tables.iter().find(|t| &t.name == n)).collect();
                for cand in &cand_nodes {
                    let col = probe_column(cand, t, &cand_nodes);
                    let sql = format!("SELECT {}.{} FROM {} LIMIT 0", quote_ident(c), quote_ident(&col), qualified(Some(schema), &t.name));
                    if crate::conn::text_rows(&client, &sql).await.is_ok() {
                        g.links.push(Link {
                            from_schema: schema.into(),
                            from_table: t.name.clone(),
                            from_columns: vec![c.clone()],
                            to_schema: schema.into(),
                            to_table: cand.name.clone(),
                            to_columns: cand.primary_key.iter().cloned().collect(),
                            inferred: true,
                        });
                        break;
                    }
                }
            }
        }
        Ok(g)
    }
}

fn lar_key_guess(t: &TableNode) -> Option<String> {
    let int = |n: &str| t.columns.iter().find(|c| c.0 == n && is_integer_type(&c.1)).map(|c| c.0.clone());
    int("id").or_else(|| {
        let lt = t.name.to_lowercase();
        singulars(&lt).iter().find_map(|s| int(&format!("{s}_id")))
    })
}

fn split_quoted_idents(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                cur.push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_by_name() {
        let tables = ["clients", "customers", "orders", "items", "products", "status", "category", "companies", "addresses"];
        assert_eq!(link_candidates("customer", "orders", &tables), vec!["customers"]);
        assert_eq!(link_candidates("client_id", "orders", &tables), vec!["clients"]);
        assert_eq!(link_candidates("product_id", "items", &tables), vec!["products"]);
        assert_eq!(link_candidates("category", "products", &tables), vec!["category"]);
        assert_eq!(link_candidates("company", "x", &tables), vec!["companies"]);
        assert_eq!(link_candidates("region", "x", &tables), Vec::<String>::new());
        assert_eq!(link_candidates("address_id", "x", &tables), vec!["addresses"]);
        assert_eq!(link_candidates("qty", "items", &tables), Vec::<String>::new());
        assert!(link_candidates("order", "orders", &tables).is_empty());
    }

    #[test]
    fn helpers() {
        assert_eq!(split_quoted_idents("id,\"Name \"\"x\"\"\",b"), vec!["id", "Name \"x\"", "b"]);
        let t = TableNode { name: "categories".into(), columns: vec![("category_id".into(), "bigint".into())], primary_key: None };
        assert_eq!(lar_key_guess(&t).as_deref(), Some("category_id"));
    }
}
