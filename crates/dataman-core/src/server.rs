#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerInfo {
    pub version: String,
    pub database: String,
    pub user: String,
    pub search_path: Vec<String>,
}

impl ServerInfo {
    pub fn short_version(&self) -> String {
        let words: Vec<&str> = self.version.split_whitespace().collect();
        match words.iter().position(|w| w.starts_with(|c: char| c.is_ascii_digit())) {
            Some(i) if i > 0 => words[..=i].join(" "),
            _ => self.version.clone(),
        }
    }

    pub fn major(&self) -> u32 {
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
    fn search_paths() {
        assert_eq!(parse_search_path("{public,app}", "u"), vec!["public", "app"]);
        assert_eq!(parse_search_path("[public]", "app"), vec!["public"]);
        assert_eq!(parse_search_path("\"$user\", public", "bruno"), vec!["bruno", "public"]);
    }
}
