//! What always wins: the global switch, the tier off and a read-only database, which refuses
//! every write in every tier, allowed queries and YOLO included (read-only is the user's own
//! safety switch). Nothing the agent sends (the old `confirm`) changes any of this.

use dataman_core::guard::WriteClass;
use dataman_core::sql::Dialect;
use dataman_core::{ConnectionProfile, Environment, McpAccess};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    Read,
    Write,
}

impl Need {
    pub fn label(self) -> &'static str {
        match self {
            Need::Read => "read",
            Need::Write => "write",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub access: McpAccess,
    pub env: Environment,
    pub read_only: bool,
    pub dialect: Dialect,
    pub yolo: bool,
}

impl Target {
    pub fn of(p: &ConnectionProfile) -> Target {
        Target { access: p.mcp_access(), env: p.env, read_only: p.read_only, dialect: p.engine.dialect(), yolo: p.mcp_yolo_on() }
    }

    pub fn of_in(p: &ConnectionProfile, database: &str) -> Target {
        Target { read_only: p.read_only_for(database), ..Target::of(p) }
    }
}

pub const SWITCHED_OFF: &str = "MCP access is turned off in Dataman (Settings › MCP)";

pub const READ_ONLY: &str = "the database is read-only in Dataman";

pub fn allow(enabled: bool, t: Target, need: Need) -> Result<(), String> {
    if !enabled {
        return Err(SWITCHED_OFF.into());
    }
    match (t.access, need) {
        (McpAccess::Off, _) => Err("MCP access to this connection is off (connection settings › MCP)".into()),
        (McpAccess::Read, Need::Write) if !t.yolo => Err("this connection allows reads only; writes need its MCP tier set to write (non-destructive) or full in Dataman".into()),
        (_, Need::Write) if t.read_only => Err(READ_ONLY.into()),
        _ => Ok(()),
    }
}

pub fn draft(enabled: bool, t: Target) -> Result<(), String> {
    allow(enabled, t, Need::Read)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Run,
    Ask(Vec<String>),
    Refuse(String),
}

pub const PRODUCTION: &str = "production";

pub fn write_verdict(enabled: bool, t: Target, class: &WriteClass) -> Verdict {
    if let Err(e) = allow(enabled, t, Need::Write) {
        return Verdict::Refuse(e);
    }
    if t.yolo {
        return Verdict::Run;
    }
    let prod = t.env.is_production();
    match (class, t.access) {
        (WriteClass::Read, _) => Verdict::Run,
        (WriteClass::SafeWrite, _) if prod => Verdict::Ask(vec![PRODUCTION.into()]),
        (WriteClass::SafeWrite, _) => Verdict::Run,
        (WriteClass::Destructive { reasons }, McpAccess::Full) => {
            let mut r = reasons.clone();
            if prod {
                r.push(PRODUCTION.into());
            }
            Verdict::Ask(r)
        }
        (WriteClass::Destructive { reasons }, _) => Verdict::Refuse(format!(
            "{} is destructive: it needs the connection's MCP tier full in Dataman and a human's Allow there; this connection allows non-destructive writes only",
            reasons.join(", ")
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum By {
    Allowlist(Vec<String>),
    Yolo,
}

impl By {
    pub fn audit(&self) -> crate::audit::AllowedBy {
        match self {
            By::Allowlist(e) => crate::audit::AllowedBy::Allowlist { entries: e.iter().map(|s| crate::audit::redact(s)).collect() },
            By::Yolo => crate::audit::AllowedBy::Yolo,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judged {
    pub verdict: Verdict,
    pub by: Option<By>,
    pub row_limit: bool,
}

pub fn judge(enabled: bool, t: Target, sql: &str, allowed: &[String]) -> Judged {
    let class = dataman_core::guard::agent_write_class(sql, t.dialect);
    let tier = |class: &WriteClass| Judged { verdict: write_verdict(enabled, t, class), by: None, row_limit: true };
    if t.yolo {
        let mut j = tier(&class);
        if j.verdict == Verdict::Run {
            j.row_limit = false;
            j.by = class.is_write().then_some(By::Yolo);
        }
        return j;
    }
    let (used, rest) = dataman_core::guard::allowlisted(sql, t.dialect, allowed);
    if used.is_empty() {
        return tier(&class);
    }
    if let Err(e) = allow(enabled, t, Need::Read) {
        return Judged { verdict: Verdict::Refuse(e), by: None, row_limit: true };
    }
    if t.read_only && class.is_write() {
        return Judged { verdict: Verdict::Refuse(READ_ONLY.into()), by: None, row_limit: true };
    }
    let rest = dataman_core::guard::pieces_class(&rest, t.dialect);
    if !rest.is_write() {
        return Judged { verdict: Verdict::Run, by: Some(By::Allowlist(used)), row_limit: false };
    }
    let mut j = tier(&rest);
    if !matches!(j.verdict, Verdict::Refuse(_)) {
        j.by = Some(By::Allowlist(used));
    }
    j
}

pub fn over_limit(affected: u64, limit: u64) -> Option<String> {
    (affected > limit).then(|| format!("{affected} rows (limit {limit})"))
}

pub fn needs_app(reasons: &[String]) -> String {
    format!("{} needs approval in Dataman; open the app and try again", if reasons.is_empty() { "this write".to_string() } else { reasons.join(", ") })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(access: McpAccess, env: Environment, read_only: bool) -> Target {
        Target { access, env, read_only, dialect: Dialect::Postgres, yolo: false }
    }

    fn safe() -> WriteClass {
        WriteClass::SafeWrite
    }

    fn bad() -> WriteClass {
        WriteClass::Destructive { reasons: vec!["DELETE without WHERE".into()] }
    }

    #[test]
    fn reads() {
        for env in Environment::ALL {
            for ro in [false, true] {
                assert!(allow(true, t(McpAccess::Off, env, ro), Need::Read).is_err());
                for a in [McpAccess::Read, McpAccess::Write, McpAccess::Full] {
                    assert!(allow(true, t(a, env, ro), Need::Read).is_ok(), "{a:?} {env:?} {ro}");
                }
                assert!(allow(false, t(McpAccess::Full, env, ro), Need::Read).unwrap_err().contains("turned off"));
            }
        }
        let mut p = ConnectionProfile::new("x");
        assert_eq!(Target::of(&p).access, McpAccess::Read);
        p.env = Environment::Production;
        assert!(allow(true, Target::of(&p), Need::Read).is_err());
    }

    #[test]
    fn write_matrix() {
        use McpAccess::*;
        use Verdict::*;
        for env in Environment::ALL {
            let prod = env.is_production();
            for ro in [false, true] {
                for (class, name) in [(WriteClass::Read, "read"), (safe(), "safe"), (bad(), "bad")] {
                    for access in McpAccess::ALL {
                        let v = write_verdict(true, t(access, env, ro), &class);
                        let want = match (access, ro, name) {
                            (Off | Read, _, _) => "refuse",
                            (_, true, _) => "refuse",
                            (_, _, "read") => "run",
                            (_, _, "safe") if prod => "ask",
                            (_, _, "safe") => "run",
                            (Full, _, "bad") => "ask",
                            _ => "refuse",
                        };
                        let got = match &v {
                            Run => "run",
                            Ask(_) => "ask",
                            Refuse(_) => "refuse",
                        };
                        assert_eq!(got, want, "{access:?} {env:?} ro={ro} {name}: {v:?}");
                        if let Ask(r) = &v {
                            assert_eq!(r.iter().any(|x| x == PRODUCTION), prod, "{r:?}");
                            assert_eq!(r.iter().any(|x| x.contains("WHERE")), name == "bad", "{r:?}");
                        }
                    }
                    assert!(matches!(write_verdict(false, t(Full, env, ro), &class), Refuse(e) if e.contains("turned off")));
                }
            }
        }
        let Refuse(e) = write_verdict(true, t(Write, Environment::Dev, false), &bad()) else { panic!() };
        assert!(e.contains("DELETE without WHERE") && e.contains("full") && e.contains("Allow"), "{e}");
        let Refuse(e) = write_verdict(true, t(Read, Environment::Dev, false), &safe()) else { panic!() };
        assert!(e.contains("reads only"), "{e}");
        let Refuse(e) = write_verdict(true, t(Full, Environment::Dev, true), &safe()) else { panic!() };
        assert!(e.contains("read-only"), "{e}");
    }

    #[test]
    fn read_only_per_database() {
        for tier in [McpAccess::Write, McpAccess::Full] {
            let mut p = ConnectionProfile::new("x");
            p.mcp = Some(tier);
            p.set_read_only_for("shop", true);
            assert!(allow(true, Target::of_in(&p, "shop"), Need::Write).unwrap_err().contains("read-only"));
            assert!(matches!(write_verdict(true, Target::of_in(&p, "shop"), &bad()), Verdict::Refuse(_)));
            assert!(allow(true, Target::of_in(&p, "other"), Need::Write).is_ok());
            p.read_only = true;
            p.set_read_only_for("scratch", false);
            assert!(allow(true, Target::of_in(&p, "scratch"), Need::Write).is_ok());
            assert!(allow(true, Target::of_in(&p, "other"), Need::Write).is_err());
        }
    }

    #[test]
    fn drafts() {
        for env in Environment::ALL {
            for ro in [false, true] {
                for a in McpAccess::ALL {
                    let r = draft(true, t(a, env, ro));
                    assert_eq!(r.is_ok(), a != McpAccess::Off, "{a:?} {env:?} {ro}: {r:?}");
                    assert!(draft(false, t(a, env, ro)).unwrap_err().contains("turned off"));
                }
            }
        }
    }

    #[test]
    fn row_limit() {
        assert_eq!(over_limit(100, 100), None);
        assert_eq!(over_limit(0, 0), None);
        assert_eq!(over_limit(101, 100).as_deref(), Some("101 rows (limit 100)"));
        assert!(needs_app(&["101 rows (limit 100)".into()]).contains("needs approval in Dataman; open the app"));
        assert!(needs_app(&[]).contains("this write"));
    }

    #[test]
    fn statements_through_the_tiers() {
        use dataman_core::guard::agent_write_class;
        let v = |access, env, sql: &str| write_verdict(true, t(access, env, false), &agent_write_class(sql, Dialect::Postgres));
        use Environment::*;
        assert_eq!(v(McpAccess::Write, Dev, "UPDATE t SET a = 1 WHERE id = 2"), Verdict::Run);
        assert_eq!(v(McpAccess::Write, Dev, "SELECT 1"), Verdict::Run);
        for sql in ["DROP TABLE t", "DELETE FROM t WHERE 1=1", "TRUNCATE t", "UPDATE t SET a = 1", "ALTER TABLE t ADD c int", "CALL p()"] {
            assert!(matches!(v(McpAccess::Write, Dev, sql), Verdict::Refuse(_)), "{sql}");
            assert!(matches!(v(McpAccess::Full, Dev, sql), Verdict::Ask(_)), "{sql}");
        }
        assert_eq!(v(McpAccess::Full, Production, "INSERT INTO t VALUES (1)"), Verdict::Ask(vec!["production".into()]));
        assert_eq!(v(McpAccess::Full, Production, "DROP TABLE t"), Verdict::Ask(vec!["DROP TABLE".into(), "production".into()]));
        assert!(matches!(v(McpAccess::Read, Dev, "INSERT INTO t VALUES (1)"), Verdict::Refuse(_)));
        assert!(matches!(v(McpAccess::Off, Dev, "SELECT 1"), Verdict::Refuse(_)));
    }

    fn yolo(access: McpAccess, env: Environment, read_only: bool) -> Target {
        Target { yolo: true, ..t(access, env, read_only) }
    }

    #[test]
    fn yolo_matrix() {
        use McpAccess::*;
        for env in Environment::ALL {
            for ro in [false, true] {
                for (class, sql) in [(WriteClass::Read, "SELECT 1"), (safe(), "INSERT INTO t VALUES (1)"), (bad(), "DELETE FROM t")] {
                    for access in McpAccess::ALL {
                        let tg = yolo(access, env, ro);
                        let v = write_verdict(true, tg, &class);
                        let j = judge(true, tg, sql, &[]);
                        assert_eq!(j.verdict, v, "{access:?} {env:?} ro={ro} {sql}");
                        match (access, ro) {
                            (Off, _) => assert!(matches!(&v, Verdict::Refuse(e) if e.contains("off")), "{v:?}"),
                            (_, true) => assert_eq!(v, Verdict::Refuse(READ_ONLY.into()), "{access:?} {env:?} {sql}"),
                            _ => {
                                assert_eq!(v, Verdict::Run, "{access:?} {env:?} {sql}");
                                assert!(!j.row_limit, "no row limit under YOLO");
                                assert_eq!(j.by, class.is_write().then_some(By::Yolo));
                            }
                        }
                        assert!(matches!(judge(false, tg, sql, &[]).verdict, Verdict::Refuse(e) if e.contains("turned off")));
                    }
                }
            }
        }
        assert!(allow(true, yolo(Read, Environment::Production, false), Need::Write).is_ok());
        assert_eq!(allow(true, yolo(Read, Environment::Dev, true), Need::Write).unwrap_err(), READ_ONLY);
        assert!(allow(true, yolo(Off, Environment::Dev, false), Need::Read).is_err());
        let mut p = ConnectionProfile::new("x");
        (p.mcp, p.mcp_yolo, p.env) = (Some(Write), true, Environment::Production);
        if !dataman_core::MCP_ADVANCED {
            assert!(!Target::of(&p).yolo);
            return;
        }
        assert_eq!(judge(true, Target::of(&p), "DROP TABLE t", &[]).verdict, Verdict::Run);
        p.set_read_only_for("shop", true);
        assert!(matches!(judge(true, Target::of_in(&p, "shop"), "DROP TABLE t", &[]).verdict, Verdict::Refuse(_)));
        p.mcp = Some(Off);
        assert!(matches!(judge(true, Target::of(&p), "DROP TABLE t", &[]).verdict, Verdict::Refuse(_)));
    }

    #[test]
    fn allowed_queries() {
        use McpAccess::*;
        let allowed = vec!["TRUNCATE audit_tmp".to_string(), "DELETE FROM sessions WHERE expires_at < ?".to_string()];
        for env in Environment::ALL {
            let prod = env.is_production();
            for ro in [false, true] {
                for access in McpAccess::ALL {
                    let tg = t(access, env, ro);
                    for sql in ["truncate audit_tmp;", "DELETE FROM sessions WHERE expires_at < '2026-01-01'", "TRUNCATE audit_tmp; delete from sessions where expires_at < 3; SELECT 1"] {
                        let j = judge(true, tg, sql, &allowed);
                        match (access, ro) {
                            (Off, _) => assert!(matches!(&j.verdict, Verdict::Refuse(e) if e.contains("off")), "{j:?}"),
                            (_, true) => assert_eq!(j.verdict, Verdict::Refuse(READ_ONLY.into())),
                            _ => {
                                assert_eq!(j.verdict, Verdict::Run, "{access:?} {env:?} {sql}");
                                assert!(!j.row_limit && matches!(&j.by, Some(By::Allowlist(e)) if !e.is_empty()), "{j:?}");
                            }
                        }
                        assert!(matches!(judge(false, tg, sql, &allowed).verdict, Verdict::Refuse(e) if e.contains("turned off")));
                    }
                    for sql in ["TRUNCATE audit", "DELETE FROM sessions WHERE expires_at < now()", "DELETE FROM sessions"] {
                        let j = judge(true, tg, sql, &allowed);
                        assert_eq!(j, Judged { verdict: write_verdict(true, tg, &dataman_core::guard::agent_write_class(sql, Dialect::Postgres)), by: None, row_limit: true }, "{sql}");
                    }
                    if !ro && access != Off {
                        let j = judge(true, tg, "TRUNCATE audit_tmp; INSERT INTO t VALUES (1)", &allowed);
                        let want = match access {
                            Read => "refuse",
                            _ if prod => "ask",
                            _ => "run",
                        };
                        let got = match &j.verdict {
                            Verdict::Run => "run",
                            Verdict::Ask(r) => {
                                assert_eq!(r, &vec![PRODUCTION.to_string()]);
                                "ask"
                            }
                            Verdict::Refuse(_) => "refuse",
                        };
                        assert_eq!(got, want, "{access:?} {env:?}");
                        assert!(j.row_limit);
                        assert_eq!(j.by.is_some(), want != "refuse", "{j:?}");
                        let j = judge(true, tg, "TRUNCATE audit_tmp; DROP TABLE users", &allowed);
                        match access {
                            Full => assert!(matches!(&j.verdict, Verdict::Ask(r) if r[0] == "DROP TABLE" && !r.iter().any(|x| x.contains("TRUNCATE"))), "{j:?}"),
                            _ => assert!(matches!(&j.verdict, Verdict::Refuse(_)), "{j:?}"),
                        }
                    }
                }
            }
        }
        let j = judge(true, t(Read, Environment::Dev, false), "DELETE FROM sessions WHERE expires_at < 1; TRUNCATE audit_tmp; DELETE FROM sessions WHERE expires_at < 2", &allowed);
        assert_eq!(j.by, Some(By::Allowlist(allowed.clone())));
        assert_eq!(By::Allowlist(vec!["DELETE FROM t WHERE id = 5".into()]).audit(), crate::audit::AllowedBy::Allowlist { entries: vec!["DELETE FROM t WHERE id = ?".into()] });
    }
}
