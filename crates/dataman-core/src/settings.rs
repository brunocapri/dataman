//! App preferences (`settings.toml`, next to `connections.toml`), edited in the Settings sheet
//! (⌘,). Every field has a serde default, so older files keep loading.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::config::Dirs;

/// Which theme the UI uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref {
    #[default]
    Dark,
    Light,
    /// Follows the system appearance.
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

/// Whether the blurred desktop shows through the window. Files from before the switch hold a
/// level: `high`, `medium` and `low` read as on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Translucency {
    /// Opaque window ("reduce transparency"): a little cheaper for the compositor while the
    /// window redraws continuously.
    Off,
    #[default]
    #[serde(alias = "high", alias = "medium", alias = "low")]
    On,
}

impl Translucency {
    pub fn is_on(self) -> bool {
        self == Translucency::On
    }

    pub fn from_bool(on: bool) -> Translucency {
        if on { Translucency::On } else { Translucency::Off }
    }
}

/// The one monospace family of the whole UI (editor, grid, explorer, menus), all embedded.
/// Stored as its family name; a name the app does not know reads as the default.
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

    /// The family name inside the font files (what the text system resolves).
    pub fn family(self) -> &'static str {
        match self {
            UiFont::GeistMono => "Geist Mono",
            UiFont::Lilex => "Lilex",
            UiFont::ZedMono => "Zed Mono",
            UiFont::JetBrainsMono => "JetBrains Mono",
        }
    }

    /// Any spelling of a family name: case, spaces, `-` and `_` do not matter.
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
    /// The UI's monospace family.
    pub font: UiFont,
    /// An installed system family instead of one of the four embedded ones; `None` uses `font`.
    /// If it is no longer installed, the app falls back to `font` quietly.
    pub system_font: Option<String>,
    /// The title bar, explorer and status bar sit on the system blur (on), or the window is
    /// opaque (off).
    pub translucency: Translucency,
    /// The first on/off switch: read once (`false` → `Off`), never written back.
    #[serde(skip_serializing)]
    transparency: Option<bool>,
    /// The UI zoom (⌘= ⌘- ⌘0): 1.0 is 100 %, kept within [`ZOOM_MIN`, `ZOOM_MAX`]; a value of
    /// the wrong type reads as 1.0 without losing the other keys.
    #[serde(deserialize_with = "de_zoom")]
    pub zoom: f32,
    /// Editor font size in points (12, 13 or 14).
    pub editor_font_size: u8,
    /// Rows the fetcher keeps loaded past what the grid shows (it pauses there).
    pub prefetch_rows: usize,
    /// Rows per page of a table's data view (opened from the explorer).
    pub page_size: usize,
    /// Grid edits: ⌘S opens the review sheet first (`false`: ⌘S commits right away).
    pub confirm_commit: bool,
    /// Where the export dialog proposes to save (`None`: ~/Downloads).
    pub export_dir: Option<String>,
    /// No explorer slide and no pop on a clicked value or a find: highlights appear at once
    /// (macOS's own Reduce motion switch has the same effect).
    pub reduce_motion: bool,
    /// Each connection's database logo, small, in the explorer and the connection picker.
    pub db_logos: bool,
    /// The SQL formatter (⌥⇧F): keyword case and spaces per indent level (2 or 4).
    pub keyword_case: crate::format::KeywordCase,
    pub indent: u8,
    /// Agents may use the connections through the MCP server (`dataman-mcp`); off refuses every
    /// tool call.
    pub mcp_enabled: bool,
    /// The most rows one tool call may return (`run_query` asks for 200 unless told otherwise).
    pub mcp_max_rows: usize,
    /// The most rows an agent's write may change before it waits for a human in the app
    /// (a connection's `mcp_row_limit` overrides it).
    pub mcp_write_row_limit: u64,
    /// While the app runs, agents' `run_query` and `execute` run visibly in an agent tab, as
    /// `open_query` does (exploration tools never open tabs).
    pub mcp_show_in_tabs: bool,
    /// Checks the update feed once a day (and on demand from the menu).
    pub update_check: bool,
    /// The update feed's URL, over the one baked in at build time (`DATAMAN_UPDATE_FEED`).
    pub update_feed: Option<String>,
    /// Where "Send" posts a crash report; unset, reports stay on disk.
    pub crash_report_url: Option<String>,
    /// `[keys]`: a command's id → its keystroke (GPUI syntax, `cmd-shift-c`), or "" for none.
    /// Only what differs from the defaults; the app ignores unknown ids and invalid keystrokes,
    /// and a value that is not a string is dropped without losing the others. Last: a table.
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

/// The rows an agent's write may change before it waits for a human, by default.
pub const MCP_WRITE_ROW_LIMIT: u64 = 100;

/// The UI zoom's range (the View menu steps from 80 % to 200 %).
pub const ZOOM_MIN: f32 = 0.8;
pub const ZOOM_MAX: f32 = 2.0;

/// A zoom factor within the range (1.0 for anything that is not a finite number).
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
    /// The file still had the MCP HTTP server's keys (`mcp_http`, `mcp_port`): the server is
    /// gone, and its token may still be in the Keychain.
    legacy_http: bool,
}

/// Whether a settings file has the removed MCP HTTP server's keys.
fn has_http_keys(text: &str) -> bool {
    toml::from_str::<toml::Table>(text).is_ok_and(|t| t.contains_key("mcp_http") || t.contains_key("mcp_port"))
}

impl SettingsStore {
    /// Reads the file; a missing or unreadable file gives the defaults (a broken preferences
    /// file must never keep the app from starting).
    pub fn open(dirs: &Dirs) -> SettingsStore {
        let path = dirs.settings_file();
        let mut legacy_http = false;
        let current = match std::fs::read_to_string(&path) {
            Ok(text) => {
                legacy_http = has_http_keys(&text);
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
        SettingsStore { path, current: Mutex::new(current), legacy_http }
    }

    /// Whether the file was written while the app had an MCP HTTP server (true until it is
    /// saved again, which drops those keys).
    pub fn legacy_http(&self) -> bool {
        self.legacy_http
    }

    pub fn get(&self) -> Settings {
        self.current.lock().clone()
    }

    /// Changes the settings and saves the file atomically.
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

    /// A file from when the app had an MCP HTTP server: its keys are ignored, nothing else is
    /// lost, the store says so until it is saved again (which drops them).
    #[test]
    fn old_http_server_keys_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("dataman"));
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(dirs.settings_file(), "theme = \"light\"\nmcp_enabled = false\nmcp_http = true\nmcp_port = 47321\nmcp_write_row_limit = 10\nmcp_show_in_tabs = false\n\n[keys]\n\"editor.run\" = \"cmd-r\"\n").unwrap();
        let st = SettingsStore::open(&dirs);
        let s = st.get();
        assert!(st.legacy_http());
        assert_eq!((s.theme, s.mcp_enabled, s.mcp_write_row_limit, s.mcp_show_in_tabs), (ThemePref::Light, false, 10, false));
        assert_eq!(s.keys.get("editor.run").map(String::as_str), Some("cmd-r"));
        st.update(|_| {}).unwrap();
        let text = std::fs::read_to_string(dirs.settings_file()).unwrap();
        assert!(!text.contains("mcp_http") && !text.contains("mcp_port") && text.contains("mcp_write_row_limit = 10"), "{text}");
        assert!(!SettingsStore::open(&dirs).legacy_http());
        std::fs::write(dirs.settings_file(), "theme = \"light\"\n").unwrap();
        assert!(!SettingsStore::open(&dirs).legacy_http());
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
        // Unknown keys and a broken file fall back to the defaults instead of failing.
        std::fs::write(dirs.settings_file(), "theme = \"light\"\nfuture = 3\n").unwrap();
        assert_eq!(SettingsStore::open(&dirs).get().theme, ThemePref::Light);
        std::fs::write(dirs.settings_file(), "theme = 12").unwrap();
        assert_eq!(SettingsStore::open(&dirs).get().theme, ThemePref::Dark);
        // A file from before the newer fields gets their defaults.
        std::fs::write(dirs.settings_file(), "theme = \"light\"\ntransparency = false\n").unwrap();
        let s = SettingsStore::open(&dirs).get();
        assert_eq!((s.editor_font_size, s.prefetch_rows, s.confirm_commit, s.export_dir), (13, 10_000, true, None));
        assert!(!s.reduce_motion, "motion is on unless asked otherwise");
        assert_eq!(s.page_size, 1000);
        // MCP: allowed, 100 rows per write, agents' queries in tabs.
        assert_eq!((s.mcp_enabled, s.mcp_max_rows), (true, 1000));
        assert_eq!((s.mcp_write_row_limit, s.mcp_show_in_tabs), (100, true));
        // The old "Writes over HTTP" switch is gone: Allow there no longer skips anything.
        std::fs::write(dirs.settings_file(), "mcp_confirm_writes = false\nmcp_write_row_limit = 5\n").unwrap();
        let m = SettingsStore::open(&dirs).get();
        assert_eq!((m.mcp_write_row_limit, m.mcp_show_in_tabs, m.theme), (5, true, ThemePref::Dark));
        // Updates checked daily, no feed override, no crash endpoint.
        assert_eq!((s.update_check, s.update_feed.as_deref(), s.crash_report_url.as_deref()), (true, None, None));
        // ... and its old on/off switch becomes the new one, written back as such.
        assert_eq!(s.translucency, Translucency::Off);
        std::fs::write(dirs.settings_file(), "transparency = true\n").unwrap();
        assert_eq!(SettingsStore::open(&dirs).get().translucency, Translucency::On);
        // The levels that came after it: any of them is on, off stays off.
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
        // The formatter's options.
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
        // No file, and a file without the key: Geist Mono.
        assert_eq!(SettingsStore::open(&dirs).get().font, UiFont::GeistMono);
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(dirs.settings_file(), "theme = \"light\"\n").unwrap();
        assert_eq!(SettingsStore::open(&dirs).get().font, UiFont::GeistMono);
        // Each family is written as its name and read back.
        for f in UiFont::ALL {
            SettingsStore::open(&dirs).update(|s| s.font = f).unwrap();
            let text = std::fs::read_to_string(dirs.settings_file()).unwrap();
            assert!(text.contains(&format!("font = \"{}\"", f.family())), "{text}");
            let s = SettingsStore::open(&dirs).get();
            assert_eq!((s.font, s.theme), (f, ThemePref::Light));
        }
        // Other spellings of a known family.
        for (v, want) in [("jetbrains-mono", UiFont::JetBrainsMono), ("ZedMono", UiFont::ZedMono), ("LILEX", UiFont::Lilex), ("geist_mono", UiFont::GeistMono)] {
            std::fs::write(dirs.settings_file(), format!("font = \"{v}\"\n")).unwrap();
            assert_eq!(SettingsStore::open(&dirs).get().font, want, "{v}");
        }
        // An unknown family or a wrong type is the default, and the other keys still load.
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
        // No file, and an older file without the key: no system font.
        assert_eq!(SettingsStore::open(&dirs).get().system_font, None);
        std::fs::create_dir_all(&dirs.config).unwrap();
        std::fs::write(dirs.settings_file(), "font = \"Lilex\"\n").unwrap();
        assert_eq!(SettingsStore::open(&dirs).get().system_font, None);
        // A chosen one is written and read back, beside the embedded fallback it ignores.
        SettingsStore::open(&dirs).update(|s| s.system_font = Some("Menlo".into())).unwrap();
        let text = std::fs::read_to_string(dirs.settings_file()).unwrap();
        assert!(text.contains("system_font = \"Menlo\""), "{text}");
        let s = SettingsStore::open(&dirs).get();
        assert_eq!((s.system_font.as_deref(), s.font), (Some("Menlo"), UiFont::Lilex));
        // Choosing none again clears it.
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
        // Out of range is clamped; an integer reads; a wrong type is 100 % and the rest loads.
        for (v, want) in [("9.0", ZOOM_MAX), ("0.1", ZOOM_MIN), ("2", 2.0), ("\"big\"", 1.0), ("nan", 1.0), ("[1.5]", 1.0)] {
            std::fs::write(dirs.settings_file(), format!("theme = \"light\"\nzoom = {v}\npage_size = 300\n")).unwrap();
            let s = SettingsStore::open(&dirs).get();
            assert_eq!((s.zoom, s.theme, s.page_size), (want, ThemePref::Light, 300), "{v}");
        }
    }

    #[test]
    fn a_removed_result_layout_key_still_loads() {
        // Older versions wrote `result_layout` (results stacked or in tabs): it is ignored, and
        // the other keys load.
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
        // A value of the wrong type goes; the other keys and settings stay.
        std::fs::write(dirs.settings_file(), "theme = \"light\"\n[keys]\nrun = 5\nhistory = \"cmd-h\"\n").unwrap();
        let s = SettingsStore::open(&dirs).get();
        assert_eq!((s.theme, s.keys.len(), s.keys.get("history").map(String::as_str)), (ThemePref::Light, 1, Some("cmd-h")));
        // `keys` that is not a table reads as none.
        std::fs::write(dirs.settings_file(), "keys = 3\ntheme = \"light\"\n").unwrap();
        let s = SettingsStore::open(&dirs).get();
        assert_eq!((s.theme, s.keys.len()), (ThemePref::Light, 0));
    }
}
