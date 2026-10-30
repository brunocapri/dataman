use std::sync::Arc;

use anyhow::Result;

use crate::Task;
use crate::conn::{Connection, first_ok};
use crate::sqlgen::quote_literal;

#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub from_schema: String,
    pub from_table: String,
    pub from_columns: Vec<String>,
    pub to_schema: String,
    pub to_table: String,
    pub to_columns: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RelationGraph {
    pub links: Vec<Link>,
}

const CACHE: &str = "relations";

impl Connection {
    pub fn relationships(self: &Arc<Self>, database: &str, schema: &str) -> Task<Arc<RelationGraph>> {
        let (this, db, schema) = (self.clone(), database.to_string(), schema.to_string());
        self.spawn(async move {
            let key = (CACHE, db.clone(), schema.clone());
            if let Some(g) = this.caches.lock().get(&key).cloned().and_then(|a| a.downcast::<RelationGraph>().ok()) {
                return Ok(g);
            }
            let g = if this.engine() == crate::config::Engine::MySql {
                this.my_graph(&db, &schema).await?
            } else if this.engine() == crate::config::Engine::SqlServer {
                this.ms_graph(&db, &schema).await?
            } else if this.engine() == crate::config::Engine::Sqlite {
                this.lite_graph(&db, &schema).await?
            } else if this.engine() == crate::config::Engine::DuckDb {
                this.duck_graph(&db, &schema).await?
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

    /// A key that names no parent column references the parent's primary key, in its order.
    async fn lite_graph(&self, db: &str, schema: &str) -> Result<RelationGraph> {
        use crate::schema::lite::{USER_OBJECTS, ident, lit};
        let sql = format!(
            "SELECT m.name, f.id, f.\"from\", {s}, f.\"table\",                     COALESCE(f.\"to\", (SELECT p.name FROM pragma_table_info(f.\"table\", {s}) p WHERE p.pk = f.seq + 1))                FROM {si}.sqlite_master m JOIN pragma_foreign_key_list(m.name, {s}) f               WHERE m.type = 'table' AND m.{USER_OBJECTS}               ORDER BY m.name, f.id, f.seq",
            s = lit(schema),
            si = ident(schema),
        );
        self.key_column_graph(db, schema, &sql).await
    }

    async fn duck_graph(&self, db: &str, schema: &str) -> Result<RelationGraph> {
        self.key_column_graph(db, schema, &crate::schema::duck::foreign_keys_sql("", schema)).await
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
                }),
            }
            last = Some(key);
        }
        Ok(g)
    }
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
    fn quoted_idents() {
        assert_eq!(split_quoted_idents("id,\"Name \"\"x\"\"\",b"), vec!["id", "Name \"x\"", "b"]);
    }
}
