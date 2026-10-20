pub(crate) mod catalog;
pub(crate) mod objects;

use anyhow::Result;

use super::{Column, Index, RelKind, Relation, lit, parse_bool};
use crate::backend::Db;
use crate::conn::first_ok;

fn parse_ints(s: &str) -> Vec<i64> {
    s.split(|c: char| !(c.is_ascii_digit() || c == '-')).filter_map(|p| p.parse().ok()).collect()
}

fn sort_schemas(mut v: Vec<String>) -> Vec<String> {
    v.retain(|s| !s.starts_with("pg_toast") && !s.starts_with("pg_temp_"));
    let rank = |s: &str| match s {
        "pg_catalog" | "information_schema" => 1,
        _ => 0,
    };
    v.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.cmp(b)));
    v.dedup();
    v
}

pub(super) async fn databases(client: &Db) -> Result<Vec<String>> {
    let rows = first_ok(
        client,
        &["SELECT datname FROM pg_catalog.pg_database WHERE datallowconn AND NOT datistemplate", "SELECT datname FROM pg_catalog.pg_database"],
    )
    .await?;
    let mut v: Vec<String> = rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
    v.sort();
    Ok(v)
}

pub(super) async fn schemas(client: &Db) -> Result<Vec<String>> {
    let rows = first_ok(client, &["SELECT nspname FROM pg_catalog.pg_namespace", "SELECT schema_name FROM information_schema.schemata"]).await?;
    Ok(sort_schemas(rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect()))
}

pub(super) async fn relations(client: &Db, schema: &str) -> Result<Vec<Relation>> {
    let s = lit(schema);
    let pg = format!(
        "SELECT c.oid, c.relname, c.relkind FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = {s}"
    );
    Ok(first_ok(client, &[&pg])
        .await?
        .into_iter()
        .filter_map(|r| {
            let kind = RelKind::from_relkind(r.get(2)?.as_deref()?)?;
            Some(Relation { schema: schema.to_string(), name: r.get(1)?.clone()?, kind, oid: r.first()?.as_deref().and_then(|o| o.parse().ok()) })
        })
        .collect())
}

pub(super) async fn columns(client: &Db, oid: u32) -> Option<Vec<Column>> {
    let base = format!(
        "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull, a.attnum \
         FROM pg_catalog.pg_attribute a WHERE a.attrelid = {oid} AND a.attnum > 0"
    );
    let q1 = format!("{base} AND NOT a.attisdropped ORDER BY a.attnum");
    let rows = first_ok(client, &[&q1, &base]).await.ok()?;
    let mut v: Vec<(i64, Column)> = rows
        .into_iter()
        .filter_map(|r| {
            Some((
                r.get(3).cloned().flatten().and_then(|n| n.parse().ok()).unwrap_or(0),
                Column {
                    name: r.first()?.clone()?,
                    type_name: r.get(1).cloned().flatten().unwrap_or_default(),
                    nullable: parse_bool(r.get(2)?).map(|nn| !nn),
                    primary_key: false,
                    default: None,
                },
            ))
        })
        .collect();
    v.sort_by_key(|(n, _)| *n);
    let nums: Vec<i64> = v.iter().map(|(n, _)| *n).collect();
    let mut v: Vec<Column> = v.into_iter().map(|(_, c)| c).collect();
    let pk = format!("SELECT indkey FROM pg_catalog.pg_index WHERE indrelid = {oid} AND indisprimary");
    if let Ok(rows) = first_ok(client, &[&pk]).await {
        for k in rows.iter().filter_map(|r| r.first().cloned().flatten()).flat_map(|s| parse_ints(&s)) {
            if let Some(i) = nums.iter().position(|n| *n == k) {
                v[i].primary_key = true;
            }
        }
    }
    let defs = format!("SELECT adnum, pg_catalog.pg_get_expr(adbin, adrelid) FROM pg_catalog.pg_attrdef WHERE adrelid = {oid}");
    if let Ok(rows) = first_ok(client, &[&defs]).await {
        for r in rows {
            let (Some(Some(n)), Some(Some(d))) = (r.first(), r.get(1)) else { continue };
            if let Some(i) = n.parse::<i64>().ok().and_then(|n| nums.iter().position(|x| *x == n)) {
                v[i].default = Some(d.clone());
            }
        }
    }
    Some(v)
}

pub(super) async fn indexes(client: &Db, oid: u32, cols: &[Column]) -> Result<Vec<Index>> {
    // Column numbers → names, from the catalog (attnum is not the list position when columns
    // were dropped).
    let nums = first_ok(client, &[&format!("SELECT attnum, attname FROM pg_catalog.pg_attribute WHERE attrelid = {oid} AND attnum > 0")])
        .await
        .unwrap_or_default();
    let name_of = |n: i64| -> String {
        nums.iter()
            .find(|r| r.first().cloned().flatten().and_then(|x| x.parse::<i64>().ok()) == Some(n))
            .and_then(|r| r.get(1).cloned().flatten())
            .or_else(|| (n > 0).then(|| cols.get(n as usize - 1).map(|c| c.name.clone())).flatten())
            .unwrap_or_else(|| "(expression)".into())
    };
    let base = "SELECT c.relname, i.indisunique, i.indisprimary, i.indkey";
    let from = format!("FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = {oid}");
    let q1 = format!("{base}, pg_catalog.pg_get_indexdef(i.indexrelid) {from}");
    let q2 = format!("{base} {from}");
    let mut v = Vec::new();
    for r in first_ok(client, &[&q1, &q2]).await? {
        let Some(Some(name)) = r.first() else { continue };
        v.push(Index {
            name: name.clone(),
            unique: parse_bool(&r.get(1).cloned().flatten()).unwrap_or(false),
            primary: parse_bool(&r.get(2).cloned().flatten()).unwrap_or(false),
            columns: r.get(3).cloned().flatten().map(|s| parse_ints(&s).into_iter().map(name_of).collect()).unwrap_or_default(),
            definition: r.get(4).cloned().flatten().filter(|d| !d.is_empty()),
        });
    }
    Ok(v)
}

pub(super) async fn columns_of_schemas(client: &Db, list: &str) -> Result<Vec<Vec<Option<String>>>> {
    let base = format!(
        "SELECT n.nspname, c.relname, a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull, a.attnum \
         FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname IN ({list}) AND a.attnum > 0"
    );
    let q1 = format!("{base} AND NOT a.attisdropped");
    Ok(first_ok(client, &[&q1, &base])
        .await?
        .into_iter()
        .map(|mut r| {
            r[4] = parse_bool(&r[4]).map(|nn| if nn { "NO".to_string() } else { "YES".to_string() });
            r
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(parse_ints("1 2"), vec![1, 2]);
        assert_eq!(parse_ints("[1, 3]"), vec![1, 3]);
        assert_eq!(parse_ints("{4}"), vec![4]);
        assert_eq!(
            sort_schemas(vec!["pg_catalog".into(), "z".into(), "public".into(), "pg_toast".into(), "information_schema".into()]),
            vec!["public", "z", "information_schema", "pg_catalog"]
        );
    }
}
