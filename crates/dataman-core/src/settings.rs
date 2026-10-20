//! App preferences (`settings.toml`, next to `connections.toml`), edited in the Settings sheet
//! (⌘,). Every field has a serde default, so older files keep loading.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::config::Dirs;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref {
    #[default]
    Dark,
    Light,
    System,
}

impl ThemePref {
    pub fn label(self) -> &'static str {
        match self {
            ThemePref::Dark => "dark",
            ThemePref::Light => "light",
            ThemePref::System => "system",
        }
    }
}

// Files from before the switch hold a level: `high`, `medium` and `low` read as on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Translucency {
    Off,
    #[default]
    #[serde(alias = "high", alias = "medium", alias = "low")]
    On,
}

impl Translucency {
    pub fn is_on(self) -> bool {
        self == Translucency::On
    }

}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UiFont {
    #[default]
    GeistMono,
    Lilex,
    ZedMono,
    JetBrainsMono,
}

impl UiFont {
    pub const ALL: [UiFont; 4] = [UiFont::GeistMono, UiFont::Lilex, UiFont::ZedMono, UiFont::JetBrainsMono];

    pub fn family(self) -> &'static str {
        match self {
            UiFont::GeistMono => "Geist Mono",
            UiFont::Lilex => "Lilex",
            UiFont::ZedMono => "Zed Mono",
            UiFont::JetBrainsMono => "JetBrains Mono",
        }
    }

    pub fn parse(s: &str) -> Option<UiFont> {
        let key = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
        let k = key(s);
        UiFont::ALL.into_iter().find(|f| key(f.family()) == k)
    }
}

impl Serialize for UiFont {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.family())
    }
}

impl<'de> Deserialize<'de> for UiFont {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        // Never fails: a family from a newer version (or a typo) must not reset the other keys.
        let v = toml::Value::deserialize(d).ok();
        Ok(v.as_ref().and_then(|v| v.as_str()).and_then(UiFont::parse).unwrap_or_default())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemePref,
    pub font: UiFont,
    pub system_font: Option<String>,
    pub translucency: Translucency,
    #[serde(skip_serializing)]
    transparency: Option<bool>,
    #[serde(deserialize_with = "de_zoom")]
    pub zoom: f32,
    pub editor_font_size: u8,
    pub prefetch_rows: usize,
    pub page_size: usize,
    pub confirm_commit: bool,
    pub export_dir: Option<String>,
    pub reduce_motion: bool,
    pub db_logos: bool,
    pub keyword_case: crate::format::KeywordCase,
    pub indent: u8,
    pub mcp_enabled: bool,
    pub mcp_max_rows: usize,
    pub mcp_write_row_limit: u64,
    pub mcp_show_in_tabs: bool,
    pub update_check: bool,
    pub update_feed: Option<String>,
    pub crash_report_url: Option<String>,
    /// Sensitive data types for agents, in every connection (only changes are written).
    #[serde(skip_serializing_if = "crate::privacy::TypeSettings::is_unchanged", deserialize_with = "de_types")]
    pub agent_types: crate::privacy::TypeSettings,
    // Last: a table (TOML writes it after the plain keys).
    #[serde(deserialize_with = "de_keys")]
    pub keys: std::collections::BTreeMap<String, String>,
}

fn de_keys<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<std::collections::BTreeMap<String, String>, D::Error> {
    let v = toml::Value::deserialize(d).ok();
    Ok(match v {
        Some(toml::Value::Table(t)) => t.into_iter().filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string()))).collect(),
        _ => Default::default(),
    })
}

fn de_types<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<crate::privacy::TypeSettings, D::Error> {
    // Never fails: a hand-edited table must not reset the other keys.
    Ok(toml::Value::deserialize(d).ok().and_then(|v| v.try_into().ok()).unwrap_or_default())
}

pub const MCP_WRITE_ROW_LIMIT: u64 = 100;

pub const ZOOM_MIN: f32 = 0.8;
pub const ZOOM_MAX: f32 = 2.0;

pub fn clamp_zoom(z: f32) -> f32 {
    if z.is_finite() { z.clamp(ZOOM_MIN, ZOOM_MAX) } else { 1.0 }
}

fn de_zoom<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<f32, D::Error> {
    // Never fails: a string or a table must not reset the other keys.
    let v = toml::Value::deserialize(d).ok();
    let z = match v {
        Some(toml::Value::Float(f)) => f as f32,
        Some(toml::Value::Integer(i)) => i as f32,
        _ => 1.0,
    };
    Ok(clamp_zoom(z))
}

impl Settings {
    pub fn format_options(&self) -> crate::format::FormatOptions {
        crate::format::FormatOptions { keyword_case: self.keyword_case, indent: if self.indent == 4 { 4 } else { 2 } }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: ThemePref::default(),
            font: UiFont::default(),
            system_font: None,
            translucency: Translucency::default(),
            transparency: None,
            zoom: 1.0,
            editor_font_size: 13,
            prefetch_rows: crate::result::PREFETCH,
            page_size: 1000,
            confirm_commit: true,
            export_dir: None,
            reduce_motion: false,
            db_logos: true,
            keyword_case: crate::format::KeywordCase::Upper,
            indent: 2,
            mcp_enabled: true,
            mcp_max_rows: 1000,
            mcp_write_row_limit: MCP_WRITE_ROW_LIMIT,
            mcp_show_in_tabs: true,
            update_check: true,
            update_feed: None,
            crash_report_url: None,
            agent_types: Default::default(),
            keys: Default::default(),
        }
    }
}

impl Dirs {
    pub fn settings_file(&self) -> PathBuf {
        self.config.join("settings.toml")
    }
}

pub struct SettingsStore {
    path: PathBuf,
    current: Mutex<Settings>,
}

impl SettingsStore {
    // A broken preferences file must never keep the app from starting.
    pub fn open(dirs: &Dirs) -> SettingsStore {
        let path = dirs.settings_file();
        let current = match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mut s = toml::from_str::<Settings>(&text).unwrap_or_else(|e| {
                    log::warn!("ignoring {}: {e}", path.display());
                    Settings::default()
                });
                // The oldest files: `transparency = false` meant an opaque window.
                if s.transparency.take() == Some(false) && !text.contains("translucency") {
                    s.translucency = Translucency::Off;
                }
                s
            }
            Err(_) => Settings::default(),
        };
        SettingsStore { path, current: Mutex::new(current) }
    }

    pub fn get(&self) -> Settings {
        self.current.lock().clone()
    }

    pub fn update(&self, f: impl FnOnce(&mut Settings)) -> Result<()> {
        let mut g = self.current.lock();
        f(&mut g);
        save(&self.path, &g)
    }
}

fn save(path: &Path, s: &Settings) -> Result<()> {
    let dir = path.parent().context("settings path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let text = toml::to_string_pretty(s)?;
    let tmp = dir.join(format!(".settings.toml.{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_http_server_keys_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(dirs.settings_file(), "theme = \"light\"\nmcp_enabled = false\nmcp_http = true\nmcp_port = 47321\nmcp_write_row_limit = 10\nmcp_show_in_tabs = false\n\n[keys]\n\"editor.run\" = \"cmd-r\"\n").unwrap();
        let st = SettingsStore::open(&dirs);
        let s = st.get();
        assert_eq!((s.theme, s.mcp_enabled, s.mcp_write_row_limit, s.mcp_show_in_tabs), (ThemePref::Light, false, 10, false));
        assert_eq!(s.keys.get("editor.run").map(String::as_str), Some("cmd-r"));
        st.update(|_| {}).unwrap();
        let text = std::fs::read_to_string(dirs.settings_file()).unwrap();
        assert!(!text.contains("mcp_http") && !text.contains("mcp_port") && text.contains("mcp_write_row_limit = 10"), "{text}");
    }

    #[test]
    fn theme_roundtrip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        let s = SettingsStore::open(&dirs);
        assert_eq!(s.get().theme, ThemePref::Dark);
        assert_eq!(s.get().translucency, Translucency::On);
        s.update(|s| s.theme = ThemePref::System).unwrap();
        assert_eq!(SettingsStore::open(&dirs).get().theme, ThemePref::System);
        std::fs::write(dirs.settings_file(), "theme = \"light\"\nfuture = 3\n").unwrap();
        assert_eq!(SettingsStore::open(&dirs).get().theme, ThemePref::Light);
        std::fs::write(dirs.settings_file(), "theme = 12").unwrap();
        assert_eq!(SettingsStore::open(&dirs).get().theme, ThemePref::Dark);
        std::fs::write(dirs.settings_file(), "theme = \"light\"\ntransparency = false\n").unwrap();
        let s = SettingsStore::open(&dirs).get();
        assert_eq!((s.editor_font_size, s.prefetch_rows, s.confirm_commit, s.export_dir), (13, 10_000, true, None));
        assert!(!s.reduce_motion, "motion is on unless asked otherwise");
        assert_eq!(s.page_size, 1000);
        assert_eq!((s.mcp_enabled, s.mcp_max_rows), (true, 1000));
        assert_eq!((s.mcp_write_row_limit, s.mcp_show_in_tabs), (100, true));
        std::fs::write(dirs.settings_file(), "mcp_confirm_writes = false\nmcp_write_row_limit = 5\n").unwrap();
        let m = SettingsStore::open(&dirs).get();
        assert_eq!((m.mcp_write_row_limit, m.mcp_show_in_tabs, m.theme), (5, true, ThemePref::Dark));
        assert_eq!((s.update_check, s.update_feed.as_deref(), s.crash_report_url.as_deref()), (true, None, None));
        assert_eq!(s.translucency, Translucency::Off);
        std::fs::write(dirs.settings_file(), "transparency = true\n").unwrap();
        assert_eq!(SettingsStore::open(&dirs).get().translucency, Translucency::On);
        for (level, want) in [("high", Translucency::On), ("medium", Translucency::On), ("low", Translucency::On), ("off", Translucency::Off)] {
            std::fs::write(dirs.settings_file(), format!("translucency = \"{level}\"\n")).unwrap();
            assert_eq!(SettingsStore::open(&dirs).get().translucency, want, "{level}");
        }
        let st = SettingsStore::open(&dirs);
        st.update(|s| s.translucency = Translucency::Off).unwrap();
        let text = std::fs::read_to_string(dirs.settings_file()).unwrap();
        assert!(text.contains("translucency = \"off\"") && !text.contains("transparency"), "{text}");
        assert_eq!(SettingsStore::open(&dirs).get().translucency, Translucency::Off);
        st.update(|s| s.translucency = Translucency::On).unwrap();
        assert!(std::fs::read_to_string(dirs.settings_file()).unwrap().contains("translucency = \"on\""));
        let st = SettingsStore::open(&dirs);
        st.update(|s| {
            s.prefetch_rows = 50_000;
            s.export_dir = Some("/tmp/x".into());
        })
        .unwrap();
        let s = SettingsStore::open(&dirs).get();
        assert_eq!((s.prefetch_rows, s.export_dir.as_deref()), (50_000, Some("/tmp/x")));
        assert!(!s.reduce_motion);
        SettingsStore::open(&dirs).update(|s| s.reduce_motion = true).unwrap();
        assert!(std::fs::read_to_string(dirs.settings_file()).unwrap().contains("reduce_motion = true"));
        assert!(SettingsStore::open(&dirs).get().reduce_motion);
        let o = SettingsStore::open(&dirs).get().format_options();
        assert_eq!((o.keyword_case, o.indent), (crate::format::KeywordCase::Upper, 2));
        SettingsStore::open(&dirs).update(|s| (s.keyword_case, s.indent) = (crate::format::KeywordCase::Lower, 4)).unwrap();
        assert!(std::fs::read_to_string(dirs.settings_file()).unwrap().contains("keyword_case = \"lower\""));
        let o = SettingsStore::open(&dirs).get().format_options();
        assert_eq!((o.keyword_case, o.indent), (crate::format::KeywordCase::Lower, 4));
    }

    #[test]
    fn font_defaults_roundtrips_and_ignores_unknown_names() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        assert_eq!(SettingsStore::open(&dirs).get().font, UiFont::GeistMono);
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(dirs.settings_file(), "theme = \"light\"\n").unwrap();
        assert_eq!(SettingsStore::open(&dirs).get().font, UiFont::GeistMono);
        for f in UiFont::ALL {
            SettingsStore::open(&dirs).update(|s| s.font = f).unwrap();
            let text = std::fs::read_to_string(dirs.settings_file()).unwrap();
            assert!(text.contains(&format!("font = \"{}\"", f.family())), "{text}");
            let s = SettingsStore::open(&dirs).get();
            assert_eq!((s.font, s.theme), (f, ThemePref::Light));
        }
        for (v, want) in [("jetbrains-mono", UiFont::JetBrainsMono), ("ZedMono", UiFont::ZedMono), ("LILEX", UiFont::Lilex), ("geist_mono", UiFont::GeistMono)] {
            std::fs::write(dirs.settings_file(), format!("font = \"{v}\"\n")).unwrap();
            assert_eq!(SettingsStore::open(&dirs).get().font, want, "{v}");
        }
        for v in ["\"Comic Mono\"", "12", "[\"Lilex\"]"] {
            std::fs::write(dirs.settings_file(), format!("theme = \"light\"\nfont = {v}\npage_size = 200\n")).unwrap();
            let s = SettingsStore::open(&dirs).get();
            assert_eq!((s.font, s.theme, s.page_size), (UiFont::GeistMono, ThemePref::Light, 200), "{v}");
        }
    }

    #[test]
    fn system_font_roundtrips_and_defaults_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        assert_eq!(SettingsStore::open(&dirs).get().system_font, None);
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(dirs.settings_file(), "font = \"Lilex\"\n").unwrap();
        assert_eq!(SettingsStore::open(&dirs).get().system_font, None);
        SettingsStore::open(&dirs).update(|s| s.system_font = Some("Menlo".into())).unwrap();
        let text = std::fs::read_to_string(dirs.settings_file()).unwrap();
        assert!(text.contains("system_font = \"Menlo\""), "{text}");
        let s = SettingsStore::open(&dirs).get();
        assert_eq!((s.system_font.as_deref(), s.font), (Some("Menlo"), UiFont::Lilex));
        SettingsStore::open(&dirs).update(|s| s.system_font = None).unwrap();
        assert_eq!(SettingsStore::open(&dirs).get().system_font, None);
    }

    #[test]
    fn zoom_defaults_roundtrips_and_clamps() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        assert_eq!(SettingsStore::open(&dirs).get().zoom, 1.0);
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(dirs.settings_file(), "theme = \"light\"\n").unwrap();
        assert_eq!(SettingsStore::open(&dirs).get().zoom, 1.0);
        SettingsStore::open(&dirs).update(|s| s.zoom = 1.25).unwrap();
        let text = std::fs::read_to_string(dirs.settings_file()).unwrap();
        assert!(text.contains("zoom = 1.25"), "{text}");
        assert_eq!(SettingsStore::open(&dirs).get().zoom, 1.25);
        for (v, want) in [("9.0", ZOOM_MAX), ("0.1", ZOOM_MIN), ("2", 2.0), ("\"big\"", 1.0), ("nan", 1.0), ("[1.5]", 1.0)] {
            std::fs::write(dirs.settings_file(), format!("theme = \"light\"\nzoom = {v}\npage_size = 300\n")).unwrap();
            let s = SettingsStore::open(&dirs).get();
            assert_eq!((s.zoom, s.theme, s.page_size), (want, ThemePref::Light, 300), "{v}");
        }
    }

    #[test]
    fn a_removed_result_layout_key_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        std::fs::create_dir_all(&dirs.config).unwrap();
        for v in ["\"stacked\"", "\"tabs\"", "3"] {
            std::fs::write(dirs.settings_file(), format!("theme = \"light\"\nresult_layout = {v}\npage_size = 200\n")).unwrap();
            let s = SettingsStore::open(&dirs).get();
            assert_eq!((s.theme, s.page_size), (ThemePref::Light, 200), "{v}");
        }
    }

    #[test]
    fn keys_roundtrip_and_drop_non_strings() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        assert!(SettingsStore::open(&dirs).get().keys.is_empty());
        SettingsStore::open(&dirs)
            .update(|s| {
                s.keys.insert("run".into(), "cmd-r".into());
                s.keys.insert("history".into(), String::new());
            })
            .unwrap();
        let text = std::fs::read_to_string(dirs.settings_file()).unwrap();
        assert!(text.contains("[keys]") && text.contains("run = \"cmd-r\""), "{text}");
        let s = SettingsStore::open(&dirs).get();
        assert_eq!((s.keys.get("run").map(String::as_str), s.keys.get("history").map(String::as_str)), (Some("cmd-r"), Some("")));
        std::fs::write(dirs.settings_file(), "theme = \"light\"\n[keys]\nrun = 5\nhistory = \"cmd-h\"\n").unwrap();
        let s = SettingsStore::open(&dirs).get();
        assert_eq!((s.theme, s.keys.len(), s.keys.get("history").map(String::as_str)), (ThemePref::Light, 1, Some("cmd-h")));
        std::fs::write(dirs.settings_file(), "keys = 3\ntheme = \"light\"\n").unwrap();
        let s = SettingsStore::open(&dirs).get();
        assert_eq!((s.theme, s.keys.len()), (ThemePref::Light, 0));
    }
}
