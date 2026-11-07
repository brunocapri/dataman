use crate::config::Engine;

/// Which build of the engine answers (MySQL or MariaDB on the MySQL protocol, SQL Server or
/// Azure SQL on TDS). Every one but `Standard` has its row in `FLAVORS`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Flavor {
    #[default]
    Standard,
    MariaDb,
    Azure,
    Valkey,
    Timescale,
    Yugabyte,
    Percona,
    Dragonfly,
    KeyDb,
    Garnet,
    FerretDb,
}

/// A server that speaks an engine's protocol and is its own product (or an extension that makes
/// one): what Dataman calls it, how the server gives it away, what it changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlavorSpec {
    pub flavor: Flavor,
    pub engine: Engine,
    pub label: &'static str,
    /// Words the engine picker matches: its names and its vendor's. Empty: no tile of its own.
    pub search: &'static str,
    /// None: the engine's (no trustworthy logo of its own that reads on every theme).
    pub logo: Option<&'static str>,
    /// None: the backend tells it by other means (Azure SQL by its engine edition).
    pub clue: Option<Clue>,
    /// Where a new connection to it goes when not to the engine's port.
    pub default_port: Option<u16>,
    /// Schemas it keeps for itself, as `EngineSpec::system_schemas`. They hide on every server of
    /// the engine: the names are the flavor's own, and the extension may be in another database.
    pub system_schemas: &'static [&'static str],
}

/// What in a server's report names the flavor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clue {
    /// The fact it is in ("" = the version text): MySQL's `version_comment`, a Redis INFO field,
    /// a PostgreSQL extension, a key of MongoDB's buildInfo.
    pub fact: &'static str,
    /// What the fact holds; "" = being there is enough.
    pub holds: &'static str,
    /// Its own release follows `holds` (`-YB-2026.1.3.0`, the extension's version): the label
    /// shows it and the engine's after it. Else its release is the engine's.
    pub own_release: bool,
}

const fn clue(fact: &'static str, holds: &'static str, own_release: bool) -> Option<Clue> {
    Some(Clue { fact, holds, own_release })
}

const fn row(flavor: Flavor, engine: Engine, label: &'static str, search: &'static str, clue: Option<Clue>) -> FlavorSpec {
    FlavorSpec { flavor, engine, label, search, logo: None, clue, default_port: None, system_schemas: &[] }
}

/// In `Flavor`'s declaration order, `Standard` left out: `Flavor::spec` indexes it. Checked in
/// order: the first whose clue the report holds names the server.
pub static FLAVORS: [FlavorSpec; 10] = [
    row(Flavor::MariaDb, Engine::MySql, "MariaDB", "mariadb maria", clue("", "MariaDB", false)),
    row(Flavor::Azure, Engine::SqlServer, "Azure SQL", "", None),
    row(Flavor::Valkey, Engine::Redis, "Valkey", "valkey", clue("valkey_version", "", true)),
    FlavorSpec {
        logo: Some("logos/timescale.svg"),
        system_schemas: &["_timescaledb_*", "timescaledb_information", "timescaledb_experimental"],
        ..row(Flavor::Timescale, Engine::Postgres, "TimescaleDB", "timescaledb timescale tigerdata tiger", clue("timescaledb", "", true))
    },
    FlavorSpec { default_port: Some(5433), ..row(Flavor::Yugabyte, Engine::Postgres, "YugabyteDB", "yugabytedb yugabyte ysql", clue("", "-YB-", true)) },
    row(Flavor::Percona, Engine::MySql, "Percona Server", "percona", clue("version_comment", "Percona Server", false)),
    row(Flavor::Dragonfly, Engine::Redis, "Dragonfly", "dragonfly dragonflydb", clue("dragonfly_version", "", true)),
    // KeyDB reports Redis' fields with its own release in `redis_version`; only its own INFO
    // section (`# KeyDB`, in the default INFO) is its.
    row(Flavor::KeyDb, Engine::Redis, "KeyDB", "keydb snap", clue("mvcc_depth", "", false)),
    row(Flavor::Garnet, Engine::Redis, "Garnet", "garnet microsoft", clue("garnet_version", "", true)),
    row(Flavor::FerretDb, Engine::MongoDb, "FerretDB", "ferretdb ferret", clue("ferretdb", "", true)),
];

impl Flavor {
    pub fn spec(self) -> Option<&'static FlavorSpec> {
        (self != Flavor::Standard).then(|| &FLAVORS[self as usize - 1])
    }

    /// Its logo, else its engine's.
    pub fn logo(self, engine: Engine) -> &'static str {
        self.spec().and_then(|s| s.logo).unwrap_or(engine.logo())
    }

    /// Its name, else its engine's.
    pub fn label(self, engine: Engine) -> &'static str {
        self.spec().map_or(engine.label(), |s| s.label)
    }
}

/// The longest release text kept from a server's report: the rest of it is not a number anyone reads.
const RELEASE_MAX: usize = 24;

/// The dotted number that starts at the first digit of `s` (`df-v2.1` → `2.1`,
/// `2026.1.3.0-b0` → `2026.1.3.0`), at most `RELEASE_MAX` long.
fn number(s: &str) -> &str {
    let Some(start) = s.find(|c: char| c.is_ascii_digit()) else { return "" };
    let s = &s[start..];
    let end = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len()).min(RELEASE_MAX);
    s[..end].trim_end_matches('.')
}

/// Which flavor of `engine` sent this report, and its label ("YugabyteDB 2026.1.3.0 (PostgreSQL
/// 15.12)"). `version` is the server's version text; `fact` reads one of its other facts. What
/// the server sends only ever picks a row and gives a number: names come from the table.
pub fn detect<'a>(engine: Engine, version: &'a str, fact: impl Fn(&str) -> Option<&'a str>) -> Option<(Flavor, String)> {
    FLAVORS.iter().filter(|f| f.engine == engine).find_map(|f| {
        let c = f.clue?;
        let text = if c.fact.is_empty() { version } else { fact(c.fact)? };
        let at = text.find(c.holds)?;
        let label = if c.own_release {
            format!("{} {} ({} {})", f.label, number(&text[at + c.holds.len()..]), engine.label(), number(version))
        } else {
            format!("{} {}", f.label, number(version))
        };
        Some((f.flavor, label))
    })
}

/// What the server is: `engine`, `flavor` and `release` are what behavior goes by; `version` is
/// its label, for display only (its wording may change).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerInfo {
    pub version: String,
    pub engine: Engine,
    pub flavor: Flavor,
    /// (major, minor, patch); zeros when the server did not say.
    pub release: (u32, u32, u32),
    pub database: String,
    pub user: String,
    pub search_path: Vec<String>,
}

/// The first dotted number in `text` (`16.6`, `10.11.6-MariaDB`, `17beta1`).
pub fn parse_release(text: &str) -> (u32, u32, u32) {
    let Some(word) = text.split_whitespace().find(|w| w.starts_with(|c: char| c.is_ascii_digit())) else { return (0, 0, 0) };
    let mut parts = word.split('.').map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u32>().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

impl ServerInfo {
    pub fn of(engine: Engine, flavor: Flavor, label: String, raw: &str) -> ServerInfo {
        ServerInfo { version: label, engine, flavor, release: parse_release(raw), ..Default::default() }
    }

    pub fn is_mariadb(&self) -> bool {
        self.flavor == Flavor::MariaDb
    }

    pub fn short_version(&self) -> String {
        let words: Vec<&str> = self.version.split_whitespace().collect();
        match words.iter().position(|w| w.starts_with(|c: char| c.is_ascii_digit())) {
            Some(i) if i > 0 => words[..=i].join(" "),
            _ => self.version.clone(),
        }
    }

    pub fn major(&self) -> u32 {
        if self.release.0 > 0 {
            return self.release.0;
        }
        let v = self.version.split_whitespace().nth(1).unwrap_or("");
        v.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0)
    }
}

pub fn parse_search_path(s: &str, user: &str) -> Vec<String> {
    let s = s.trim().trim_start_matches(['{', '[']).trim_end_matches(['}', ']']);
    s.split(',')
        .map(|p| p.trim().trim_matches('"').to_string())
        .map(|p| if p == "$user" { user.to_string() } else { p })
        .filter(|p| !p.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        let mut info = ServerInfo { version: "PostgreSQL 16.6 on x86_64".into(), ..Default::default() };
        assert_eq!(info.short_version(), "PostgreSQL 16.6");
        assert_eq!(info.major(), 16);
        info.version = "PostgreSQL 18.6 (Debian 18.6-1.pgdg13+2) on aarch64-unknown-linux-gnu".into();
        assert_eq!(info.major(), 18);
        info.version = "PostgreSQL 9.6.24 on x86_64".into();
        assert_eq!(info.major(), 9);
        info.version = "PostgreSQL 17beta1".into();
        assert_eq!(info.major(), 17);
        info.version = "something else".into();
        assert_eq!(info.major(), 0);
    }

    #[test]
    fn releases() {
        assert_eq!(parse_release("10.11.6-MariaDB-log"), (10, 11, 6));
        assert_eq!(parse_release("PostgreSQL 17beta1 on x"), (17, 0, 0));
        assert_eq!(parse_release("none"), (0, 0, 0));
        let i = ServerInfo::of(Engine::MySql, Flavor::MariaDb, "MySQL Community 8.4".into(), "10.6.12");
        assert!(i.is_mariadb() && i.major() == 10, "the label says nothing about behavior");
    }

    #[test]
    fn one_flavor_row_each_in_order() {
        assert_eq!(Flavor::Standard.spec(), None);
        for (i, f) in FLAVORS.iter().enumerate() {
            assert_eq!(f.flavor as usize, i + 1, "{}", f.label);
            assert_eq!(f.flavor.spec(), Some(f));
        }
        assert_eq!((Flavor::Timescale.logo(Engine::Postgres), Flavor::KeyDb.logo(Engine::Redis)), ("logos/timescale.svg", "logos/redis.svg"));
    }

    /// Each server's report as it came from the real one (see the live tests).
    #[test]
    fn detects_what_servers_report() {
        let none = |_: &str| None;
        let pg = "PostgreSQL 15.12-YB-2026.1.3.0-b0 on aarch64-unknown-linux-gnu, compiled by clang version 21.1.1";
        assert_eq!(detect(Engine::Postgres, pg, none), Some((Flavor::Yugabyte, "YugabyteDB 2026.1.3.0 (PostgreSQL 15.12)".into())));
        let ext = |n: &str| (n == "timescaledb").then_some("2.31.0");
        assert_eq!(detect(Engine::Postgres, "PostgreSQL 17.11 on aarch64-unknown-linux-musl", ext), Some((Flavor::Timescale, "TimescaleDB 2.31.0 (PostgreSQL 17.11)".into())));
        assert_eq!(detect(Engine::Postgres, "PostgreSQL 17.11 on aarch64-unknown-linux-musl", none), None);
        let comment = |n: &str| (n == "version_comment").then_some("Percona Server (GPL), Release 11, Revision 57878ff8");
        assert_eq!(detect(Engine::MySql, "8.4.11-11", comment), Some((Flavor::Percona, "Percona Server 8.4.11".into())));
        assert_eq!(detect(Engine::MySql, "8.4.6", |_| Some("MySQL Community Server - GPL")), None);
        assert_eq!(detect(Engine::MySql, "10.6.12-MariaDB-log", none), Some((Flavor::MariaDb, "MariaDB 10.6.12".into())));
        let info = |text: &'static str| move |n: &str| text.lines().find_map(|l| l.strip_prefix(n)?.strip_prefix(':'));
        assert_eq!(detect(Engine::Redis, "7.4.0", info("redis_version:7.4.0\ndragonfly_version:df-v2.1")), Some((Flavor::Dragonfly, "Dragonfly 2.1 (Redis 7.4.0)".into())));
        assert_eq!(detect(Engine::Redis, "7.4.3", info("garnet_version:2.2.8\nredis_version:7.4.3")), Some((Flavor::Garnet, "Garnet 2.2.8 (Redis 7.4.3)".into())));
        assert_eq!(detect(Engine::Redis, "6.3.4", info("redis_version:6.3.4\n# KeyDB\nmvcc_depth:0")), Some((Flavor::KeyDb, "KeyDB 6.3.4".into())));
        assert_eq!(detect(Engine::Redis, "7.2.4", info("redis_version:7.2.4\nvalkey_version:8.1.1")), Some((Flavor::Valkey, "Valkey 8.1.1 (Redis 7.2.4)".into())));
        assert_eq!(detect(Engine::Redis, "7.4.1", info("redis_version:7.4.1\nredis_mode:standalone")), None);
        assert_eq!(detect(Engine::MongoDb, "7.0.77", |n: &str| (n == "ferretdb").then_some("v2.7.0")), Some((Flavor::FerretDb, "FerretDB 2.7.0 (MongoDB 7.0.77)".into())));
        assert_eq!(detect(Engine::MongoDb, "8.0.4", none), None);
        // A flavor's clue only counts on its own engine.
        assert_eq!(detect(Engine::MySql, "8.4.0", |n: &str| (n == "timescaledb").then_some("2.31.0")), None);
    }

    #[test]
    fn untrusted_releases_stay_numbers() {
        let long = format!("PostgreSQL 15.12-YB-{}<script>", "9.".repeat(500));
        let (_, label) = detect(Engine::Postgres, &long, |_| None).unwrap();
        assert_eq!(label.len(), "YugabyteDB  (PostgreSQL 15.12)".len() + RELEASE_MAX - 1, "{label}");
        assert!(!label.contains('<'));
        assert_eq!(number("df-v2.1"), "2.1");
        assert_eq!(number("no digits"), "");
    }

    #[test]
    fn search_paths() {
        assert_eq!(parse_search_path("{public,app}", "u"), vec!["public", "app"]);
        assert_eq!(parse_search_path("[public]", "app"), vec!["public"]);
        assert_eq!(parse_search_path("\"$user\", public", "bruno"), vec!["bruno", "public"]);
    }
}
