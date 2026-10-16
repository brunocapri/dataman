//! Who may do what: the permission tiers, pure functions of the global switch, the
//! connection's MCP tier (`off | read | write | full`), its environment, the database's
//! read-only setting and what the statements are (`dataman_core::guard::agent_write_class`).
//!
//! | statements | off | read | write | full |
//! |---|---|---|---|---|
//! | read | refused | runs | runs | runs |
//! | safe write | refused | refused | runs | runs |
//! | destructive | refused | refused | refused | asks |
//! | more rows than the limit | — | — | asks | asks |
//!
//! "Asks": the write waits for a human's Allow in Dataman (held in its transaction when it
//! can be, else asked about before it runs); without the app it is refused. On a production
//! connection every write asks. A read-only database refuses every write in every tier. Nothing
//! the agent sends (the old `confirm`) changes any of this.

use dataman_core::guard::WriteClass;
use dataman_core::sql::Dialect;
use dataman_core::{ConnectionProfile, Environment, McpAccess};

/// What a tool needs on its connection at least.
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

/// The connection facts the matrix reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub access: McpAccess,
    pub env: Environment,
    pub read_only: bool,
    /// What its statements are read as.
    pub dialect: Dialect,
}

impl Target {
    pub fn of(p: &ConnectionProfile) -> Target {
        Target { access: p.mcp_access(), env: p.env, read_only: p.read_only, dialect: p.engine.dialect() }
    }

    /// [`Target::of`] for one of its databases: read-only as that database is (its own
    /// setting in Dataman, else the connection's).
    pub fn of_in(p: &ConnectionProfile, database: &str) -> Target {
        Target { read_only: p.read_only_for(database), ..Target::of(p) }
    }
}

pub const SWITCHED_OFF: &str = "MCP access is turned off in Dataman (Settings › MCP)";

/// Whether a tool needing `need` may touch the connection at all.
pub fn allow(enabled: bool, t: Target, need: Need) -> Result<(), String> {
    if !enabled {
        return Err(SWITCHED_OFF.into());
    }
    match (t.access, need) {
        (McpAccess::Off, _) => Err("MCP access to this connection is off (connection settings › MCP)".into()),
        (McpAccess::Read, Need::Write) => Err("this connection allows reads only; writes need its MCP tier set to write (non-destructive) or full in Dataman".into()),
        (McpAccess::Write | McpAccess::Full, Need::Write) if t.read_only => Err("the database is read-only in Dataman".into()),
        _ => Ok(()),
    }
}

/// Whether a draft (`open_query` with `run: false`) may go into an agent tab of the connection:
/// every tier but off, whatever the statements are, the environment or read-only (nothing runs;
/// the user runs it later as the user).
pub fn draft(enabled: bool, t: Target) -> Result<(), String> {
    allow(enabled, t, Need::Read)
}

/// What happens to a write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// It runs and commits (unless it changes more rows than the limit: [`over_limit`]).
    Run,
    /// It waits for a human's Allow in the app; why, tersely.
    Ask(Vec<String>),
    Refuse(String),
}

/// Why a production connection asks.
pub const PRODUCTION: &str = "production";

/// The tier's answer to statements of `class` on `t`.
pub fn write_verdict(enabled: bool, t: Target, class: &WriteClass) -> Verdict {
    if let Err(e) = allow(enabled, t, Need::Write) {
        return Verdict::Refuse(e);
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

/// The question a write that changed `affected` rows raises against `limit`, if any.
pub fn over_limit(affected: u64, limit: u64) -> Option<String> {
    (affected > limit).then(|| format!("{affected} rows (limit {limit})"))
}

/// The refusal of a write that needs a human when nobody can answer (stdio without the app).
pub fn needs_app(reasons: &[String]) -> String {
    format!("{} needs approval in Dataman; open the app and try again", if reasons.is_empty() { "this write".to_string() } else { reasons.join(", ") })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(access: McpAccess, env: Environment, read_only: bool) -> Target {
        Target { access, env, read_only, dialect: Dialect::Postgres }
    }

    fn safe() -> WriteClass {
        WriteClass::SafeWrite
    }

    fn bad() -> WriteClass {
        WriteClass::Destructive { reasons: vec!["DELETE without WHERE".into()] }
    }

    /// Reads: off refuses, every other tier reads, whatever the environment or read-only.
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
        // Defaults: read everywhere, off on production.
        let mut p = ConnectionProfile::new("x");
        assert_eq!(Target::of(&p).access, McpAccess::Read);
        p.env = Environment::Production;
        assert!(allow(true, Target::of(&p), Need::Read).is_err());
    }

    /// The whole write matrix: tier × environment × read-only × class, and the switch.
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
        // The refusals say what is needed.
        let Refuse(e) = write_verdict(true, t(Write, Environment::Dev, false), &bad()) else { panic!() };
        assert!(e.contains("DELETE without WHERE") && e.contains("full") && e.contains("Allow"), "{e}");
        let Refuse(e) = write_verdict(true, t(Read, Environment::Dev, false), &safe()) else { panic!() };
        assert!(e.contains("reads only"), "{e}");
        let Refuse(e) = write_verdict(true, t(Full, Environment::Dev, true), &safe()) else { panic!() };
        assert!(e.contains("read-only"), "{e}");
    }

    /// Writes follow the database's own read-only setting, else the connection's, in any tier.
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

    /// Drafts: every tier but off, in every environment, read-only or not; the switch off
    /// refuses them.
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

    /// The row limit: at it runs, past it asks (in write and full alike).
    #[test]
    fn row_limit() {
        assert_eq!(over_limit(100, 100), None);
        assert_eq!(over_limit(0, 0), None);
        assert_eq!(over_limit(101, 100).as_deref(), Some("101 rows (limit 100)"));
        assert!(needs_app(&["101 rows (limit 100)".into()]).contains("needs approval in Dataman; open the app"));
        assert!(needs_app(&[]).contains("this write"));
    }

    /// Real statements through the classifier and the matrix.
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
}
