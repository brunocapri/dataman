use crate::config::Engine;

/// Which build of the engine answers (MySQL or MariaDB on the MySQL protocol, SQL Server or
/// Azure SQL on TDS).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Flavor {
    #[default]
    Standard,
    MariaDb,
    Azure,
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
    /// The label and the release read from `raw` (the server's own version text).
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
    fn search_paths() {
        assert_eq!(parse_search_path("{public,app}", "u"), vec!["public", "app"]);
        assert_eq!(parse_search_path("[public]", "app"), vec!["public"]);
        assert_eq!(parse_search_path("\"$user\", public", "bruno"), vec!["bruno", "public"]);
    }
}
