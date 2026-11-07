//! ClickHouse's catalog: `system.databases`, `system.tables`, `system.columns`,
//! `system.data_skipping_indices`. A database is its only namespace, so schema and database are
//! the same name.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow, bail};

use super::objects::{ViewSource, ViewSourceKind};
use super::{Column, FuncSig, Index, RelKind, RelStats, Relation};
use crate::backend::Db;
use crate::conn::first_ok;
use crate::sql::{Dialect, Token, TokenKind, ident_value_in, string_value_in, tokenize_in};

pub(crate) fn lit(s: &str) -> String {
    Dialect::ClickHouse.quote_literal(s)
}

fn get(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i).cloned().flatten()
}

pub(super) async fn databases(c: &Db) -> Result<Vec<String>> {
    let rows = first_ok(c, &["SELECT name FROM system.databases ORDER BY name"]).await?;
    let mut v: Vec<String> = rows.into_iter().filter_map(|r| get(&r, 0)).collect();
    v.sort_by_key(|d| crate::Engine::ClickHouse.is_system_db(d));
    Ok(v)
}

fn rel_kind(engine: &str) -> RelKind {
    match engine {
        "View" | "LiveView" | "WindowView" => RelKind::View,
        "MaterializedView" => RelKind::MaterializedView,
        _ => RelKind::Table,
    }
}

pub(super) async fn relations(c: &Db, schema: &str) -> Result<Vec<Relation>> {
    let q = format!("SELECT name, engine FROM system.tables WHERE database = {} AND NOT is_temporary ORDER BY name", lit(schema));
    Ok(first_ok(c, &[&q])
        .await?
        .into_iter()
        .filter_map(|r| Some(Relation { schema: schema.to_string(), name: get(&r, 0)?, kind: rel_kind(&get(&r, 1)?), oid: None }))
        .collect())
}

fn default_of(kind: &str, expr: Option<String>) -> Option<String> {
    let e = expr.filter(|e| !e.is_empty())?;
    Some(if kind == "DEFAULT" || kind.is_empty() { e } else { format!("{kind} {e}") })
}

pub(super) async fn columns(c: &Db, schema: &str, table: &str) -> Option<Vec<Column>> {
    let q = format!(
        "SELECT name, type, default_kind, default_expression, is_in_primary_key FROM system.columns \
          WHERE database = {} AND table = {} ORDER BY position",
        lit(schema),
        lit(table)
    );
    let rows = first_ok(c, &[&q]).await.ok()?;
    Some(
        rows.into_iter()
            .filter_map(|r| {
                let type_name = get(&r, 1).unwrap_or_default();
                Some(Column {
                    name: get(&r, 0)?,
                    nullable: Some(type_name.starts_with("Nullable(") || type_name.starts_with("LowCardinality(Nullable(")),
                    type_name,
                    primary_key: get(&r, 4).as_deref() == Some("1"),
                    default: default_of(&get(&r, 2).unwrap_or_default(), get(&r, 3)),
                })
            })
            .collect(),
    )
}

/// A key as the server writes it, cut at its own commas: not those inside a call, a string or a
/// quoted name (`toYYYYMM(c), concat(b, 'x, y'), \`p, q\``). A quoted name alone is that name.
fn key_parts(key: &str) -> Vec<String> {
    let d = Dialect::ClickHouse;
    let toks: Vec<Token> = tokenize_in(key, d);
    let mut out = Vec::new();
    let (mut depth, mut from) = (0usize, 0);
    let mut part = |from: usize, to: usize| {
        let text = key[from..to].trim();
        let inner: Vec<&Token> = toks.iter().filter(|t| t.start >= from && t.end <= to && !t.kind.is_trivia()).collect();
        out.push(match inner.as_slice() {
            [one] if one.kind == TokenKind::QuotedIdent => crate::sql::ident_value_in(key, one, d),
            _ => text.to_string(),
        });
    };
    for t in toks.iter().filter(|t| t.kind == TokenKind::Punct) {
        match t.text(key) {
            "(" | "[" => depth += 1,
            ")" | "]" => depth = depth.saturating_sub(1),
            "," if depth == 0 => {
                part(from, t.start);
                from = t.end;
            }
            _ => {}
        }
    }
    part(from, key.len());
    out
}

/// The primary key (never unique here: it sorts the data) and the data skipping indexes.
pub(super) async fn indexes(c: &Db, schema: &str, table: &str) -> Result<Vec<Index>> {
    let (s, t) = (lit(schema), lit(table));
    let mut out = Vec::new();
    let q = format!("SELECT primary_key FROM system.tables WHERE database = {s} AND name = {t}");
    if let Some(key) = first_ok(c, &[&q]).await?.first().and_then(|r| get(r, 0)).filter(|k| !k.is_empty()) {
        out.push(Index { name: "PRIMARY KEY".into(), columns: key_parts(&key), unique: false, primary: true, definition: None });
    }
    let q = format!("SELECT name, expr, type_full, granularity FROM system.data_skipping_indices WHERE database = {s} AND table = {t} ORDER BY name");
    for r in first_ok(c, &[&q]).await? {
        let Some(name) = get(&r, 0) else { continue };
        let expr = get(&r, 1).unwrap_or_default();
        let definition = format!("INDEX {} {expr} TYPE {} GRANULARITY {}", Dialect::ClickHouse.quote_ident(&name), get(&r, 2).unwrap_or_default(), get(&r, 3).unwrap_or_default());
        out.push(Index { name, columns: vec![expr], unique: false, primary: false, definition: Some(definition) });
    }
    Ok(out)
}

pub(super) async fn columns_of_schemas(c: &Db, list: &str) -> Result<Vec<Vec<Option<String>>>> {
    let q = format!(
        "SELECT database, table, name, type, if(startsWith(type, 'Nullable(') OR startsWith(type, 'LowCardinality(Nullable('), 'YES', 'NO'), position \
           FROM system.columns WHERE database IN ({list})"
    );
    first_ok(c, &[&q]).await
}

pub(super) fn stats_sql(schema: &str) -> String {
    format!("SELECT name, total_rows, total_bytes FROM system.tables WHERE database = {}", lit(schema))
}

pub(super) fn parse_stats(rows: &[Vec<Option<String>>]) -> std::collections::HashMap<String, RelStats> {
    rows.iter().filter_map(|r| Some((get(r, 0)?, RelStats { rows: get(r, 1).and_then(|s| s.parse().ok()), bytes: get(r, 2).and_then(|s| s.parse().ok()) }))).collect()
}

pub(crate) fn primary_key_sql(schema: &str, table: &str) -> String {
    format!("SELECT name FROM system.columns WHERE database = {} AND table = {} AND is_in_primary_key ORDER BY position", lit(schema), lit(table))
}

/// The statement as the server rebuilds it, for tables and views alike.
pub(crate) fn show_create_sql(schema: &str, name: &str) -> String {
    format!("SHOW CREATE TABLE {}", Dialect::ClickHouse.qualified(Some(schema), name))
}

pub(crate) async fn functions(c: &Db) -> Vec<FuncSig> {
    let q = "SELECT name, is_aggregate FROM system.functions WHERE alias_to = '' ORDER BY name LIMIT 5000";
    match first_ok(c, &[q]).await {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|r| {
                let name = get(&r, 0)?;
                Some(FuncSig { oid: 0, schema: String::new(), name, args: String::new(), result: String::new(), kind: if get(&r, 1).as_deref() == Some("1") { 'a' } else { 'f' }, returns_set: false })
            })
            .collect(),
        Err(e) => {
            log::debug!("functions for autocomplete unavailable: {e:#}");
            Vec::new()
        }
    }
}

/// Where a table's data comes from when it is not only what it stores. A table of an engine
/// that is neither here nor `functions::clickhouse_plain_engine` cannot be followed.
#[derive(Clone, Copy)]
enum Reads {
    /// A statement of its own, in `as_select`.
    Query,
    /// `Merge(db or REGEXP(db), table_regexp)`: whatever the server's regular expressions match now.
    Merge,
    /// Arguments that name one table, as written. Distributed is followed to the local table of
    /// that name: the shards' copies are not in reach, and a server has the same rules by name.
    Named { db: usize, table: usize },
    /// `CREATE DICTIONARY … SOURCE(…)`, or `ENGINE = Dictionary(d)`.
    Dictionary,
}

const ENGINE_READS: &[(&str, Reads)] = &[
    ("View", Reads::Query),
    ("MaterializedView", Reads::Query),
    ("LiveView", Reads::Query),
    ("WindowView", Reads::Query),
    ("Merge", Reads::Merge),
    ("Buffer", Reads::Named { db: 0, table: 1 }),
    ("Distributed", Reads::Named { db: 1, table: 2 }),
    ("Dictionary", Reads::Dictionary),
];

const MOST_OBJECTS: usize = 100_000;
const MOST_MERGED: usize = 4096;

struct Object {
    engine: String,
    full: String,
    create: String,
    select: String,
}

type Key = (String, String);

/// What each relation of a server reads, from one read of its catalog: the agents' rules ask for
/// many relations in one call (a listing, a statement's tables, what those read), and the server's
/// `dependencies_table` holds only what materialized views are fed from. Read per call, never
/// kept: anything here can be replaced at any time.
pub struct ReadThrough {
    objects: HashMap<Key, Object>,
    /// The materialized views that write into a table.
    fed_by: HashMap<Key, Vec<Key>>,
    by_uuid: HashMap<String, Key>,
    routines: HashSet<String>,
}

fn unknown(what: String) -> ViewSource {
    ViewSource { schema: String::new(), name: what, kind: ViewSourceKind::Unknown }
}

fn relation(schema: &str, name: &str) -> ViewSource {
    ViewSource { schema: schema.to_string(), name: name.to_string(), kind: ViewSourceKind::Relation }
}

fn significant(s: &str) -> Vec<Token> {
    tokenize_in(s, Dialect::ClickHouse).into_iter().filter(|t| !t.kind.is_trivia()).collect()
}

fn is_name(t: &Token) -> bool {
    matches!(t.kind, TokenKind::Ident | TokenKind::QuotedIdent | TokenKind::Keyword)
}

fn is_word(s: &str, t: &Token, w: &str) -> bool {
    is_name(t) && ident_value_in(s, t, Dialect::ClickHouse).eq_ignore_ascii_case(w)
}

fn is_punct(s: &str, t: &Token, p: &str) -> bool {
    t.kind == TokenKind::Punct && t.text(s) == p
}

/// `name(a, b(c), 'd')` as its top level arguments.
fn call_args<'t>(s: &str, toks: &'t [Token]) -> Option<Vec<&'t [Token]>> {
    let open = toks.iter().position(|t| is_punct(s, t, "("))?;
    let (mut depth, mut from, mut out) = (0usize, open + 1, Vec::new());
    for (i, t) in toks.iter().enumerate().skip(open) {
        match t.text(s) {
            "(" if t.kind == TokenKind::Punct => depth += 1,
            ")" if t.kind == TokenKind::Punct => {
                depth -= 1;
                if depth == 0 {
                    if i > from {
                        out.push(&toks[from..i]);
                    }
                    return Some(out);
                }
            }
            "," if t.kind == TokenKind::Punct && depth == 1 => {
                out.push(&toks[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    None
}

/// A table's or database's name in an argument: a string, or a bare name.
fn name_arg(s: &str, arg: &[Token]) -> Option<String> {
    match arg {
        [t] if t.kind == TokenKind::String => Some(string_value_in(s, t, Dialect::ClickHouse)),
        [t] if is_name(t) => Some(ident_value_in(s, t, Dialect::ClickHouse)),
        _ => None,
    }
}

/// `TO db.t` of a materialized view's statement: after its own name, before its columns.
fn view_target(create: &str, own_db: &str) -> Option<Key> {
    let toks = significant(create);
    let d = Dialect::ClickHouse;
    let view = toks.iter().position(|t| is_word(create, t, "VIEW"))?;
    let mut i = view + 1;
    while toks.get(i + 1).is_some_and(|t| is_punct(create, t, ".")) {
        i += 2;
    }
    let mut at = i + 1;
    while let Some(t) = toks.get(at) {
        if is_punct(create, t, "(") || is_word(create, t, "AS") {
            return None;
        }
        if is_word(create, t, "TO") {
            at += 1;
            break;
        }
        at += 1;
    }
    let first = toks.get(at).filter(|t| is_name(t))?;
    if is_word(create, first, "INNER") {
        return None;
    }
    let first_name = ident_value_in(create, first, d);
    match (toks.get(at + 1), toks.get(at + 2)) {
        (Some(dot), Some(t)) if is_punct(create, dot, ".") && is_name(t) => Some((first_name, ident_value_in(create, t, d))),
        _ => Some((own_db.to_string(), first_name)),
    }
}

impl ReadThrough {
    pub(super) async fn read(c: &Db) -> Result<ReadThrough> {
        let limit = MOST_OBJECTS + 1;
        let all = format!("SELECT database, name, engine FROM system.tables WHERE NOT is_temporary LIMIT {limit}");
        let rows = first_ok(c, &[&all]).await?;
        if rows.len() > MOST_OBJECTS {
            bail!("more than {MOST_OBJECTS} tables to follow");
        }
        let mut objects: HashMap<Key, Object> = HashMap::with_capacity(rows.len());
        for r in rows {
            let (Some(db), Some(name)) = (get(&r, 0), get(&r, 1)) else { bail!("a row of system.tables has no name") };
            objects.insert((db, name), Object { engine: get(&r, 2).unwrap_or_default(), full: String::new(), create: String::new(), select: String::new() });
        }
        let readers: Vec<String> = ENGINE_READS.iter().map(|(e, _)| lit(e)).collect();
        let texts = format!(
            "SELECT database, name, engine_full, create_table_query, as_select, toString(uuid) FROM system.tables WHERE engine IN ({}) AND NOT is_temporary LIMIT {limit}",
            readers.join(", ")
        );
        let (mut fed_by, mut by_uuid): (HashMap<Key, Vec<Key>>, HashMap<String, Key>) = Default::default();
        for r in first_ok(c, &[&texts]).await? {
            let (Some(db), Some(name)) = (get(&r, 0), get(&r, 1)) else { bail!("a row of system.tables has no name") };
            let key = (db, name);
            let Some(o) = objects.get_mut(&key) else { continue };
            o.full = get(&r, 2).unwrap_or_default();
            o.create = get(&r, 3).unwrap_or_default();
            o.select = get(&r, 4).unwrap_or_default();
            if o.engine == "MaterializedView" {
                if let Some(target) = view_target(&o.create, &key.0) {
                    fed_by.entry(target).or_default().push(key.clone());
                }
                if let Some(uuid) = get(&r, 5) {
                    by_uuid.insert(uuid, key);
                }
            }
        }
        let routines = user_functions(c).await?.into_iter().map(|f| f.to_lowercase()).collect();
        Ok(ReadThrough { objects, fed_by, by_uuid, routines })
    }

    /// What `schema.name` reads besides what it stores. Each source is a relation to look at
    /// (and follow) or what cannot be followed, which keeps it closed under rules. An object the
    /// catalog does not show is an error: it says nothing of what it reads.
    pub(super) async fn sources(&self, c: &Db, schema: &str, name: &str) -> Result<Vec<ViewSource>> {
        let key = (schema.to_string(), name.to_string());
        let o = self.objects.get(&key).ok_or_else(|| anyhow!("{schema}.{name} is not in the catalog"))?;
        let mut out: Vec<ViewSource> = Vec::new();
        for (d, n) in self.fed_by.get(&key).into_iter().flatten() {
            out.push(relation(d, n));
        }
        if let Some(rest) = name.strip_prefix(".inner_id.") {
            out.push(self.by_uuid.get(rest).map_or_else(|| unknown(format!("the view that stores in {name}")), |(d, n)| relation(d, n)));
        } else if let Some(view) = name.strip_prefix(".inner.") {
            out.push(relation(schema, view));
        }
        match ENGINE_READS.iter().find(|(e, _)| *e == o.engine).map(|(_, r)| *r) {
            Some(Reads::Query) => {
                if o.select.is_empty() {
                    out.push(unknown(format!("the statement of {name}")));
                } else {
                    out.extend(self.query_sources(&o.select, Some(schema)));
                }
                if o.engine == "MaterializedView"
                    && let Some((d, n)) = view_target(&o.create, schema)
                {
                    out.push(relation(&d, &n));
                }
            }
            Some(Reads::Merge) => out.extend(self.merged(c, o, &key).await?),
            Some(Reads::Named { db, table }) => match named_source(o, db, table) {
                Some(Some(s)) => out.push(s),
                Some(None) => {}
                None => out.push(unknown(format!("the table of {name}"))),
            },
            Some(Reads::Dictionary) => out.extend(self.dictionary(o, schema, name)),
            // `MergeTree` under any prefix: the cloud's `SharedMergeTree` too.
            None if o.engine.ends_with("MergeTree") || crate::sql::functions::clickhouse_plain_engine(&o.engine) => {}
            None => out.push(unknown(format!("the {} engine of {name}", o.engine))),
        }
        for s in out.iter_mut() {
            if s.kind == ViewSourceKind::Relation && crate::Engine::ClickHouse.is_system_db(&s.schema) {
                *s = unknown(format!("{}.{}", s.schema, s.name));
            }
        }
        Ok(out)
    }

    fn query_sources(&self, query: &str, own: Option<&str>) -> Vec<ViewSource> {
        let known = |d: &str, n: &str| self.objects.contains_key(&(d.to_string(), n.to_string()));
        super::agents::query_sources(query, own, Dialect::ClickHouse, &known, &self.routines)
    }

    /// The tables a Merge table matches now, by the server's own regular expressions.
    async fn merged(&self, c: &Db, o: &Object, key: &Key) -> Result<Vec<ViewSource>> {
        let d = Dialect::ClickHouse;
        let closed = || vec![unknown(format!("what {} merges", key.1))];
        let toks = significant(&o.full);
        let Some(args) = call_args(&o.full, &toks) else { return Ok(closed()) };
        let [db, table, ..] = args.as_slice() else { return Ok(closed()) };
        let db_is = match *db {
            [t] if t.kind == TokenKind::String => format!("database = {}", t.text(&o.full)),
            [t] if is_name(t) => format!("database = {}", lit(&ident_value_in(&o.full, t, d))),
            [f, open, t, close] if is_word(&o.full, f, "REGEXP") && is_punct(&o.full, open, "(") && t.kind == TokenKind::String && is_punct(&o.full, close, ")") => {
                format!("match(database, {})", t.text(&o.full))
            }
            _ => return Ok(closed()),
        };
        let [t] = *table else { return Ok(closed()) };
        if t.kind != TokenKind::String {
            return Ok(closed());
        }
        let q = format!("SELECT database, name FROM system.tables WHERE {db_is} AND match(name, {}) LIMIT {}", t.text(&o.full), MOST_MERGED + 1);
        let rows = first_ok(c, &[&q]).await?;
        if rows.len() > MOST_MERGED {
            return Ok(closed());
        }
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let (Some(dbn), Some(n)) = (get(&r, 0), get(&r, 1)) else { bail!("a row of system.tables has no name") };
            if (&dbn, &n) != (&key.0, &key.1) {
                out.push(relation(&dbn, &n));
            }
        }
        Ok(out)
    }

    fn dictionary(&self, o: &Object, schema: &str, name: &str) -> Vec<ViewSource> {
        let d = Dialect::ClickHouse;
        let closed = |what: &str| vec![unknown(format!("the {what} of {name}"))];
        if !o.full.is_empty() {
            // `ENGINE = Dictionary(d)`: reads the dictionary, by a name with its database.
            let toks = significant(&o.full);
            let Some(arg) = call_args(&o.full, &toks).and_then(|a| a.first().copied().and_then(|x| name_arg(&o.full, x))) else { return closed("dictionary") };
            return vec![match arg.rsplit_once('.') {
                Some((db, dict)) => relation(db, dict),
                None => relation(schema, &arg),
            }];
        }
        let s = &o.create;
        let toks = significant(s);
        let Some(at) = toks.iter().position(|t| is_word(s, t, "SOURCE")).filter(|&i| toks.get(i + 1).is_some_and(|t| is_punct(s, t, "("))) else { return closed("source") };
        let Some(kind) = toks.get(at + 2) else { return closed("source") };
        if is_word(s, kind, "NULL") {
            return Vec::new();
        }
        if !is_word(s, kind, "CLICKHOUSE") {
            return vec![unknown(format!("the {} source of {name}", kind.text(s)))];
        }
        if !toks.get(at + 3).is_some_and(|t| is_punct(s, t, "(")) {
            return closed("source");
        }
        let (mut db, mut table, mut query, mut remote, mut filter) = (None, None, None, false, None);
        let mut i = at + 4;
        while let (Some(k), Some(v)) = (toks.get(i), toks.get(i + 1)) {
            if is_punct(s, k, ")") {
                break;
            }
            let value = (v.kind == TokenKind::String).then(|| string_value_in(s, v, d));
            match ident_value_in(s, k, d).to_ascii_uppercase().as_str() {
                "DB" => db = value,
                "TABLE" => table = value,
                "QUERY" => query = value,
                "WHERE" => filter = value,
                "HOST" => remote = value.is_none_or(|h| !matches!(h.as_str(), "localhost" | "127.0.0.1" | "::1")),
                _ => {}
            }
            i += 2;
        }
        if remote {
            return closed("other server");
        }
        if filter.is_some_and(|w| w.to_ascii_lowercase().contains("select")) {
            return closed("filter");
        }
        match (query, db, table) {
            // Its names resolve where the dictionary's connection lands, which the catalog
            // does not say: a name without its database is not followed.
            (Some(q), ..) => self.query_sources(&q, None),
            (None, Some(db), Some(t)) => vec![relation(&db, &t)],
            _ => closed("source"),
        }
    }
}

/// The table an engine's arguments name; `Some(None)`: none (a Buffer with no table flushes nowhere).
fn named_source(o: &Object, db: usize, table: usize) -> Option<Option<ViewSource>> {
    let toks = significant(&o.full);
    let args = call_args(&o.full, &toks)?;
    let (d, t) = (name_arg(&o.full, args.get(db)?)?, name_arg(&o.full, args.get(table)?)?);
    Some((!t.is_empty()).then(|| relation(&d, &t)))
}

/// The server's functions made with `CREATE FUNCTION` or defined by a program.
pub(super) async fn user_functions(c: &Db) -> Result<Vec<String>> {
    let rows = first_ok(c, &["SELECT name FROM system.functions WHERE origin <> 'System'"]).await?;
    Ok(rows.iter().filter_map(|r| get(r, 0)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_materialized_view_names_its_target() {
        let t = |s: &str| view_target(s, "own");
        assert_eq!(t("CREATE MATERIALIZED VIEW a.b TO c.d (`id` UInt64) AS SELECT 1"), Some(("c".into(), "d".into())));
        assert_eq!(t("CREATE MATERIALIZED VIEW a.b TO `x y`.`to` AS SELECT 1"), Some(("x y".into(), "to".into())));
        assert_eq!(t("CREATE MATERIALIZED VIEW a.b REFRESH EVERY 1 HOUR TO d AS SELECT 1"), Some(("own".into(), "d".into())));
        assert_eq!(t("CREATE MATERIALIZED VIEW a.`to` (`id` UInt64) ENGINE = MergeTree ORDER BY id AS SELECT 1"), None);
    }

    #[test]
    fn defaults_say_their_kind() {
        assert_eq!(default_of("DEFAULT", Some("now()".into())).as_deref(), Some("now()"));
        assert_eq!(default_of("MATERIALIZED", Some("a + 1".into())).as_deref(), Some("MATERIALIZED a + 1"));
        assert_eq!(default_of("", Some(String::new())), None);
        assert_eq!(lit("a\\'b"), "'a\\\\''b'");
        assert_eq!(rel_kind("MaterializedView"), RelKind::MaterializedView);
        assert_eq!(rel_kind("ReplacingMergeTree"), RelKind::Table);
        assert_eq!(key_parts("id, day"), ["id", "day"]);
        assert_eq!(key_parts(r"toYYYYMM(c), intHash32(a), concat(b, \'x, y\')".replace('\\', "").as_str()), ["toYYYYMM(c)", "intHash32(a)", "concat(b, 'x, y')"]);
        assert_eq!(key_parts("`p, q`"), ["p, q"]);
    }
}
