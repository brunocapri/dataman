//! PostgreSQL's catalog queries for the explorer's other objects (`schema::objects`), and the
//! parsers of their rows. Pure functions, tested below.

use std::collections::HashMap;

use super::super::objects::{
    Constraint, ConstraintKind, Extension, FuncKind, Function, Grant, ObjectCounts, Partition, Privileges, RelStats, Role, Rows, Sequence,
    TableExtras, Trigger, TypeKind, UserType, get, split_args, trigger_events,
};
use super::super::{lit, parse_bool};

pub fn in_schema(alias_ns: &str, schema: &str) -> String {
    format!("{alias_ns}.nspname = {}", lit(schema))
}

/// The per-kind count queries of a schema, by name.
pub fn count_queries(schema: &str) -> [(&'static str, String); 4] {
    let w = in_schema("n", schema);
    [
        (
            "functions",
            format!(
                "SELECT count(*) FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
                 WHERE {w} AND p.prokind IN ('f', 'p')"
            ),
        ),
        (
            "sequences",
            format!(
                "SELECT count(*) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE {w} AND c.relkind = 'S'"
            ),
        ),
        (
            "types",
            format!(
                "SELECT count(*) FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
                 LEFT JOIN pg_catalog.pg_class c ON c.oid = t.typrelid \
                 WHERE {w} AND (t.typtype IN ('e', 'd') OR (t.typtype = 'c' AND c.relkind = 'c'))"
            ),
        ),
        (
            "triggers",
            format!(
                "SELECT count(*) FROM pg_catalog.pg_trigger g JOIN pg_catalog.pg_class c ON c.oid = g.tgrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE {w} AND NOT g.tgisinternal"
            ),
        ),
    ]
}

/// All four counts in one row.
pub fn counts_sql(schema: &str) -> String {
    let parts: Vec<String> = count_queries(schema).iter().map(|(name, q)| format!("({q}) AS {name}")).collect();
    format!("SELECT {}", parts.join(", "))
}

pub fn parse_counts(row: &[Option<String>]) -> ObjectCounts {
    let n = |i: usize| get(row, i).and_then(|s| s.parse::<u64>().ok());
    ObjectCounts { functions: n(0), sequences: n(1), types: n(2), triggers: n(3) }
}

pub fn functions_sql(schema: &str) -> [String; 2] {
    let cols = "p.oid, p.proname, {kind}, pg_catalog.pg_get_function_identity_arguments(p.oid), \
                pg_catalog.oidvectortypes(p.proargtypes), pg_catalog.pg_get_function_result(p.oid), p.proretset";
    let from = format!("FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace WHERE {}", in_schema("n", schema));
    [
        format!("SELECT {} {from} AND p.prokind IN ('f', 'p') ORDER BY p.proname, 4", cols.replace("{kind}", "p.prokind")),
        // Before PostgreSQL 11: no procedures, no prokind.
        format!("SELECT {} {from} AND NOT p.proisagg AND NOT p.proiswindow ORDER BY p.proname, 4", cols.replace("{kind}", "'f'")),
    ]
}

pub fn parse_function(r: &[Option<String>]) -> Option<Function> {
    let types = get(r, 4).unwrap_or_default();
    Some(Function {
        oid: get(r, 0)?.parse().ok()?,
        name: get(r, 1)?,
        kind: if get(r, 2).as_deref() == Some("p") { FuncKind::Procedure } else { FuncKind::Function },
        identity: get(r, 3).unwrap_or_default(),
        arg_types: split_args(&types),
        result: get(r, 5),
        returns_set: parse_bool(&get(r, 6)).unwrap_or(false),
    })
}

pub fn sequences_sql(schema: &str) -> [String; 3] {
    let w = in_schema("n", schema);
    [
        format!(
            "SELECT c.relname, pg_catalog.format_type(s.seqtypid, NULL), dc.relname, a.attname \
             FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_catalog.pg_sequence s ON s.seqrelid = c.oid \
             LEFT JOIN pg_catalog.pg_depend d ON d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass AND d.objid = c.oid \
                  AND d.refobjsubid > 0 AND d.deptype IN ('a', 'i') \
             LEFT JOIN pg_catalog.pg_class dc ON dc.oid = d.refobjid \
             LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid = d.refobjid AND a.attnum = d.refobjsubid \
             WHERE {w} AND c.relkind = 'S' ORDER BY c.relname"
        ),
        format!(
            "SELECT c.relname, NULL, NULL, NULL FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE {w} AND c.relkind = 'S' ORDER BY c.relname"
        ),
        format!(
            "SELECT sequence_name, data_type, NULL, NULL FROM information_schema.sequences WHERE sequence_schema = {} ORDER BY 1",
            lit(schema)
        ),
    ]
}

pub fn parse_sequence(r: &[Option<String>]) -> Option<Sequence> {
    Some(Sequence { name: get(r, 0)?, type_name: get(r, 1), owned_by: get(r, 2).zip(get(r, 3)) })
}

/// Separates an enum's labels in one text column.
const LABEL_SEP: char = '\u{1f}';

pub fn types_sql(schema: &str) -> String {
    format!(
        "SELECT t.typname, t.typtype, pg_catalog.format_type(t.typbasetype, t.typtypmod), \
         (SELECT pg_catalog.string_agg(e.enumlabel, E'\\x1f' ORDER BY e.enumsortorder) FROM pg_catalog.pg_enum e WHERE e.enumtypid = t.oid) \
         FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
         LEFT JOIN pg_catalog.pg_class c ON c.oid = t.typrelid \
         WHERE {} AND (t.typtype IN ('e', 'd') OR (t.typtype = 'c' AND c.relkind = 'c')) ORDER BY t.typname",
        in_schema("n", schema)
    )
}

pub fn parse_type(r: &[Option<String>]) -> Option<UserType> {
    let kind = match get(r, 1)?.as_str() {
        "e" => TypeKind::Enum,
        "c" => TypeKind::Composite,
        "d" => TypeKind::Domain,
        _ => return None,
    };
    Some(UserType {
        name: get(r, 0)?,
        kind,
        base: get(r, 2).filter(|_| kind == TypeKind::Domain),
        labels: get(r, 3).map(|s| s.split(LABEL_SEP).map(str::to_string).collect()).unwrap_or_default(),
    })
}

pub fn triggers_sql(schema: &str) -> String {
    format!(
        "SELECT g.tgname, c.relname, g.tgenabled, g.tgtype, pg_catalog.pg_get_triggerdef(g.oid) \
         FROM pg_catalog.pg_trigger g JOIN pg_catalog.pg_class c ON c.oid = g.tgrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE {} AND NOT g.tgisinternal ORDER BY c.relname, g.tgname",
        in_schema("n", schema)
    )
}

/// A trigger row: name, table, tgenabled, tgtype, definition.
pub fn parse_trigger(r: &[Option<String>]) -> Option<Trigger> {
    Some(Trigger {
        name: get(r, 0)?,
        table: get(r, 1).unwrap_or_default(),
        enabled: get(r, 2).as_deref() != Some("D"),
        events: trigger_events(get(r, 3).and_then(|t| t.parse().ok()).unwrap_or(0)),
        definition: get(r, 4),
    })
}

/// The pieces of a table's extras, each a query on its own: constraints, triggers, children,
/// parent. Every row starts with a tag.
pub fn extras_queries(oid: u32) -> [String; 4] {
    [
        format!(
            "SELECT 'c', con.conname, con.contype::text, pg_catalog.pg_get_constraintdef(con.oid), NULL \
             FROM pg_catalog.pg_constraint con WHERE con.conrelid = {oid} AND con.contype IN ('p', 'u', 'f', 'c', 'x')"
        ),
        format!(
            "SELECT 't', g.tgname, g.tgenabled::text, pg_catalog.pg_get_triggerdef(g.oid), g.tgtype::text \
             FROM pg_catalog.pg_trigger g WHERE g.tgrelid = {oid} AND NOT g.tgisinternal"
        ),
        format!(
            "SELECT 'p', c.relname, n.nspname, pg_catalog.pg_get_expr(c.relpartbound, c.oid), NULL \
             FROM pg_catalog.pg_inherits i JOIN pg_catalog.pg_class c ON c.oid = i.inhrelid \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE i.inhparent = {oid}"
        ),
        format!(
            "SELECT 'P', c.relname, n.nspname, NULL, NULL \
             FROM pg_catalog.pg_inherits i JOIN pg_catalog.pg_class c ON c.oid = i.inhparent \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE i.inhrelid = {oid}"
        ),
    ]
}

/// The four pieces in one query.
pub fn extras_sql(oid: u32) -> String {
    extras_queries(oid).join(" UNION ALL ")
}

/// Fills `t` from tagged rows (`c` constraint, `t` trigger, `p` child, `P` parent); `table` is
/// the table's own name (for its triggers).
pub fn parse_extras(rows: &Rows, table: &str, t: &mut TableExtras) {
    for r in rows {
        let Some(tag) = get(r, 0) else { continue };
        let Some(name) = get(r, 1) else { continue };
        match tag.as_str() {
            "c" => {
                if let Some(kind) = get(r, 2).as_deref().and_then(ConstraintKind::from_contype) {
                    t.constraints.get_or_insert_with(Vec::new).push(Constraint { name, kind, definition: get(r, 3) });
                }
            }
            "t" => t.triggers.get_or_insert_with(Vec::new).push(Trigger {
                name,
                table: table.to_string(),
                enabled: get(r, 2).as_deref() != Some("D"),
                events: trigger_events(get(r, 4).and_then(|x| x.parse().ok()).unwrap_or(0)),
                definition: get(r, 3),
            }),
            "p" => t.partitions.get_or_insert_with(Vec::new).push(Partition { schema: get(r, 2).unwrap_or_default(), name, bound: get(r, 3) }),
            "P" => t.parent = Some((get(r, 2).unwrap_or_default(), name)),
            _ => {}
        }
    }
    if let Some(c) = t.constraints.as_mut() {
        c.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
    }
    if let Some(v) = t.triggers.as_mut() {
        v.sort_by(|a, b| a.name.cmp(&b.name));
    }
    if let Some(v) = t.partitions.as_mut() {
        v.sort_by(|a, b| a.name.cmp(&b.name));
    }
}

pub fn stats_sql(schema: &str) -> [String; 3] {
    let w = in_schema("n", schema);
    let from = format!("FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE {w} AND c.relkind IN ('r', 'p', 'm', 'f')");
    [
        // PostgreSQL 12+: a partitioned table counts its partitions.
        format!(
            "SELECT c.relname, \
             CASE WHEN c.relkind = 'p' THEN (SELECT sum(GREATEST(l.reltuples, 0)) FROM pg_catalog.pg_partition_tree(c.oid) pt \
                  JOIN pg_catalog.pg_class l ON l.oid = pt.relid WHERE pt.isleaf) ELSE c.reltuples END, \
             CASE WHEN c.relkind = 'p' THEN (SELECT sum(pg_catalog.pg_total_relation_size(pt.relid)) FROM pg_catalog.pg_partition_tree(c.oid) pt) \
                  ELSE pg_catalog.pg_total_relation_size(c.oid) END {from}"
        ),
        format!("SELECT c.relname, c.reltuples, pg_catalog.pg_total_relation_size(c.oid) {from}"),
        // A relation dropped while sizing makes the size query fail: the estimates alone.
        format!("SELECT c.relname, c.reltuples, NULL {from}"),
    ]
}

/// Name → stats. A catalog that answers zero for every table (LAR) reports nothing: empty.
pub fn parse_stats(rows: &Rows) -> HashMap<String, RelStats> {
    let mut m = HashMap::new();
    let mut any = false;
    for r in rows {
        let Some(name) = get(r, 0) else { continue };
        // PostgreSQL 14+: -1 when never vacuumed or analyzed.
        let rows = get(r, 1).and_then(|s| s.parse::<f64>().ok()).filter(|n| *n >= 0.);
        let bytes = get(r, 2).and_then(|s| s.parse::<f64>().ok()).map(|b| b as i64).filter(|b| *b >= 0);
        any |= rows.is_some_and(|n| n > 0.) || bytes.is_some_and(|b| b > 0);
        m.insert(name, RelStats { rows, bytes });
    }
    if !any {
        m.clear();
    }
    m
}

pub fn extensions_sql() -> [&'static str; 2] {
    [
        "SELECT e.extname, e.extversion, n.nspname, a.default_version FROM pg_catalog.pg_extension e \
         LEFT JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace \
         LEFT JOIN pg_catalog.pg_available_extensions a ON a.name = e.extname ORDER BY 1",
        "SELECT e.extname, e.extversion, NULL, NULL FROM pg_catalog.pg_extension e ORDER BY 1",
    ]
}

pub fn parse_extension(r: &[Option<String>]) -> Option<Extension> {
    Some(Extension { name: get(r, 0)?, version: get(r, 1).unwrap_or_default(), schema: get(r, 2), default_version: get(r, 3) })
}

pub fn roles_sql() -> [&'static str; 2] {
    [
        "SELECT r.rolname, r.rolsuper, r.rolcanlogin, r.rolcreatedb, r.rolcreaterole, r.rolreplication, r.rolinherit, \
         r.rolbypassrls, r.rolconnlimit, r.rolvaliduntil FROM pg_catalog.pg_roles r ORDER BY 1",
        "SELECT r.rolname, r.rolsuper, r.rolcanlogin FROM pg_catalog.pg_roles r ORDER BY 1",
    ]
}

/// A role row; the built-in `pg_*` roles are left out.
pub fn parse_role(r: &[Option<String>]) -> Option<Role> {
    let name = get(r, 0)?;
    if name.starts_with("pg_") {
        return None;
    }
    let b = |i: usize| parse_bool(&get(r, i)).unwrap_or(false);
    Some(Role {
        name,
        superuser: b(1),
        login: b(2),
        createdb: b(3),
        createrole: b(4),
        replication: b(5),
        inherit: parse_bool(&get(r, 6)).unwrap_or(true),
        bypassrls: b(7),
        conn_limit: get(r, 8).and_then(|s| s.parse().ok()).unwrap_or(-1),
        valid_until: get(r, 9).filter(|v| v != "infinity"),
    })
}

/// The grants on a table (`table` given) or a schema: owner, grantee, privilege, grantable.
pub fn privileges_sql(schema: &str, table: Option<&str>) -> Vec<String> {
    let grantee = "CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.pg_get_userbyid(a.grantee) END";
    match table {
        Some(t) => vec![
            format!(
                "SELECT pg_catalog.pg_get_userbyid(c.relowner), {grantee}, a.privilege_type, a.is_grantable \
                 FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 CROSS JOIN LATERAL pg_catalog.aclexplode(COALESCE(c.relacl, pg_catalog.acldefault('r', c.relowner))) a \
                 WHERE {} AND c.relname = {}",
                in_schema("n", schema),
                lit(t)
            ),
            format!(
                "SELECT NULL, grantee, privilege_type, is_grantable FROM information_schema.role_table_grants \
                 WHERE table_schema = {} AND table_name = {}",
                lit(schema),
                lit(t)
            ),
        ],
        None => vec![format!(
            "SELECT pg_catalog.pg_get_userbyid(n.nspowner), {grantee}, a.privilege_type, a.is_grantable \
             FROM pg_catalog.pg_namespace n \
             CROSS JOIN LATERAL pg_catalog.aclexplode(COALESCE(n.nspacl, pg_catalog.acldefault('n', n.nspowner))) a \
             WHERE {}",
            in_schema("n", schema)
        )],
    }
}

/// `None` when the server reported no grant at all.
pub fn parse_privileges(rows: &Rows, schema_level: bool) -> Option<Privileges> {
    let mut p = Privileges { schema_level, owner: None, grants: Vec::new() };
    for r in rows {
        if p.owner.is_none() {
            p.owner = get(r, 0);
        }
        let (Some(grantee), Some(privilege)) = (get(r, 1), get(r, 2)) else { continue };
        let g = Grant { grantee, privilege: privilege.to_uppercase(), grantable: parse_bool(&get(r, 3)).unwrap_or(false) };
        if !p.grants.contains(&g) {
            p.grants.push(g);
        }
    }
    (!p.grants.is_empty()).then_some(p)
}

#[cfg(test)]
mod tests {
    use super::super::super::objects::{Cell, partition_bound_summary};
    use super::*;

    fn row(v: &[Option<&str>]) -> Vec<Option<String>> {
        v.iter().map(|c| c.map(str::to_string)).collect()
    }

    #[test]
    fn query_builders_quote_the_schema() {
        let s = "o'brien";
        for (_, q) in count_queries(s) {
            assert!(q.contains("n.nspname = 'o''brien'"), "{q}");
        }
        let c = counts_sql("public");
        assert!(c.starts_with("SELECT (SELECT count(*)") && c.contains(") AS functions") && c.contains(") AS triggers"), "{c}");
        assert!(functions_sql(s)[0].contains("p.prokind IN ('f', 'p')"));
        assert!(functions_sql(s)[1].contains("NOT p.proisagg"));
        assert!(sequences_sql(s)[0].contains("c.relkind = 'S'") && sequences_sql(s)[2].contains("information_schema.sequences"));
        assert!(types_sql(s).contains("t.typtype IN ('e', 'd')"));
        assert!(triggers_sql(s).contains("NOT g.tgisinternal"));
        assert!(stats_sql(s).iter().all(|q| q.contains("c.relkind IN ('r', 'p', 'm', 'f')")));
        assert!(stats_sql(s)[0].contains("pg_partition_tree"));
        let e = extras_sql(42);
        assert_eq!(e.matches("UNION ALL").count(), 3);
        assert!(e.contains("conrelid = 42") && e.contains("tgrelid = 42") && e.contains("inhparent = 42") && e.contains("inhrelid = 42"));
        let p = privileges_sql("s", Some("t"));
        assert!(p[0].contains("aclexplode") && p[0].contains("c.relname = 't'") && p[1].contains("role_table_grants"));
        assert!(privileges_sql("s", None)[0].contains("nspacl"));
    }

    #[test]
    fn parsers() {
        assert_eq!(
            parse_counts(&row(&[Some("3"), Some("0"), None, Some("x")])),
            ObjectCounts { functions: Some(3), sequences: Some(0), types: None, triggers: None }
        );
        let f = parse_function(&row(&[Some("1234"), Some("add"), Some("f"), Some("a integer, b integer"), Some("integer, integer"), Some("integer"), Some("f")])).unwrap();
        assert_eq!((f.oid, f.kind, f.arg_types.len(), f.result.as_deref(), f.returns_set), (1234, FuncKind::Function, 2, Some("integer"), false));
        let p = parse_function(&row(&[Some("9"), Some("archive"), Some("p"), Some(""), Some(""), None, Some("f")])).unwrap();
        assert_eq!((p.kind, p.arg_types.len(), p.result), (FuncKind::Procedure, 0, None));
        let s = parse_sequence(&row(&[Some("orders_id_seq"), Some("bigint"), Some("orders"), Some("id")])).unwrap();
        assert_eq!(s.owned_by, Some(("orders".into(), "id".into())));
        assert_eq!(parse_sequence(&row(&[Some("free"), None, None, None])).unwrap().owned_by, None);
        let e = parse_type(&row(&[Some("mood"), Some("e"), Some("-"), Some("sad\u{1f}ok, fine\u{1f}happy")])).unwrap();
        assert_eq!((e.kind, e.labels.clone(), e.base), (TypeKind::Enum, vec!["sad".to_string(), "ok, fine".into(), "happy".into()], None));
        let d = parse_type(&row(&[Some("money2"), Some("d"), Some("numeric(12,2)"), None])).unwrap();
        assert_eq!((d.kind, d.base.as_deref()), (TypeKind::Domain, Some("numeric(12,2)")));
        assert!(parse_type(&row(&[Some("x"), Some("b"), None, None])).is_none());
        let t = parse_trigger(&row(&[Some("audit"), Some("orders"), Some("D"), Some("21"), Some("CREATE TRIGGER audit …")])).unwrap();
        assert_eq!((t.enabled, t.events.as_str()), (false, "after insert/update"));
        let ext = parse_extension(&row(&[Some("pg_trgm"), Some("1.5"), Some("public"), Some("1.6")])).unwrap();
        assert_eq!(ext.update(), Some("1.6"));
        assert_eq!(parse_extension(&row(&[Some("plpgsql"), Some("1.0"), None, Some("1.0")])).unwrap().update(), None);
        assert!(parse_role(&row(&[Some("pg_monitor"), Some("f"), Some("f")])).is_none());
        let r = parse_role(&row(&[Some("app"), Some("f"), Some("t"), Some("f"), Some("f"), Some("f"), Some("t"), Some("f"), Some("10"), Some("infinity")])).unwrap();
        assert_eq!((r.login, r.superuser, r.conn_limit, r.valid_until), (true, false, 10, None));
        // The short query (old servers): defaults.
        let r = parse_role(&row(&[Some("old"), Some("t"), Some("f")])).unwrap();
        assert_eq!((r.superuser, r.inherit, r.conn_limit), (true, true, -1));
    }

    #[test]
    fn extras_from_tagged_rows() {
        let rows = vec![
            row(&[Some("c"), Some("orders_total_check"), Some("c"), Some("CHECK ((total >= 0))"), None]),
            row(&[Some("c"), Some("orders_pkey"), Some("p"), Some("PRIMARY KEY (id)"), None]),
            row(&[Some("c"), Some("nn"), Some("n"), Some("NOT NULL id"), None]),
            row(&[Some("t"), Some("audit"), Some("O"), Some("CREATE TRIGGER audit …"), Some("7")]),
            row(&[Some("p"), Some("events_2026_02"), Some("s"), Some("FOR VALUES FROM ('2026-02-01') TO ('2026-03-01')"), None]),
            row(&[Some("p"), Some("events_2026_01"), Some("s"), Some("FOR VALUES FROM ('2026-01-01') TO ('2026-02-01')"), None]),
            row(&[Some("P"), Some("all_events"), Some("s"), None, None]),
        ];
        let mut t = TableExtras { constraints: Some(vec![]), triggers: Some(vec![]), partitions: Some(vec![]), parent: None };
        parse_extras(&rows, "orders", &mut t);
        let c = t.constraints.unwrap();
        assert_eq!(c.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["orders_pkey", "orders_total_check"], "PK first, NOT NULL left out");
        let tr = t.triggers.unwrap();
        assert_eq!((tr[0].table.as_str(), tr[0].enabled, tr[0].events.as_str()), ("orders", true, "before insert"));
        let p = t.partitions.unwrap();
        assert_eq!(p[0].name, "events_2026_01");
        assert_eq!(partition_bound_summary(p[0].bound.as_deref().unwrap()), "FROM ('2026-01-01') TO ('2026-02-01')");
        assert_eq!(t.parent, Some(("s".into(), "all_events".into())));
        // Pieces that were not reported stay None.
        let mut u = TableExtras::default();
        parse_extras(&vec![row(&[Some("t"), Some("x"), Some("O"), None, Some("4")])], "t", &mut u);
        assert!(u.constraints.is_none() && u.partitions.is_none() && u.triggers.is_some());
    }

    #[test]
    fn stats_rows() {
        let rows = vec![row(&[Some("a"), Some("1200"), Some("98304")]), row(&[Some("b"), Some("-1"), Some("8192")]), row(&[Some("c"), Some("0"), None])];
        let m = parse_stats(&rows);
        assert_eq!(m["a"], RelStats { rows: Some(1200.), bytes: Some(98304) });
        assert_eq!(m["b"].rows, None, "never analyzed");
        assert_eq!(m["c"].bytes, None);
        // All zeros (LAR): not reported.
        assert!(parse_stats(&vec![row(&[Some("a"), Some("0"), Some("0")]), row(&[Some("b"), Some("0"), Some("0")])]).is_empty());
    }

    #[test]
    fn privilege_matrix() {
        let rows = vec![
            row(&[Some("alice"), Some("alice"), Some("SELECT"), Some("f")]),
            row(&[Some("alice"), Some("alice"), Some("INSERT"), Some("f")]),
            row(&[Some("alice"), Some("PUBLIC"), Some("SELECT"), Some("f")]),
            row(&[Some("alice"), Some("bob"), Some("SELECT"), Some("t")]),
            row(&[Some("alice"), Some("bob"), Some("MAINTAIN"), Some("f")]),
        ];
        let p = parse_privileges(&rows, false).unwrap();
        assert_eq!(p.owner.as_deref(), Some("alice"));
        assert_eq!(p.roles(), vec!["alice", "bob", "PUBLIC"]);
        assert_eq!(p.columns().len(), 8, "the standard seven and MAINTAIN");
        assert_eq!(p.cell("bob", "SELECT"), Cell::Grantable);
        assert_eq!(p.cell("alice", "INSERT"), Cell::Yes);
        assert_eq!(p.cell("PUBLIC", "INSERT"), Cell::No);
        assert!(parse_privileges(&vec![], true).is_none());
        let s = parse_privileges(&vec![row(&[Some("o"), Some("o"), Some("usage"), Some("f")])], true).unwrap();
        assert_eq!(s.columns(), vec!["USAGE", "CREATE"]);
        assert_eq!(s.cell("o", "USAGE"), Cell::Yes);
    }
}
