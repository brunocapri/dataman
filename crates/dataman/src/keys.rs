use gpui::{Action, KeyBinding, Keystroke};

pub const LINUX: bool = cfg!(target_os = "linux");

/// The keymap is written as on macOS (and so are `[keys]` in settings.toml); on Linux ⌘ is
/// Ctrl, with the desktop's own text keys (⌘←/→ Home/End, ⌘↑/↓ Ctrl+Home/End, ⌥←/→ ⌥⌫ ⌥⌦ by
/// word with Ctrl, ⌘⌫ Ctrl+Shift+⌫), and where Ctrl would take a key from something else the
/// key moves: ⌃⌘←/→ (back, forward) to Alt+←/→, ⌃⌘↩/⌫ (commit, rollback) to Alt+Shift+↩/⌫, ⌥⌘←/→ (pages) to Alt+PgUp/PgDn, ⌥⌘L to Ctrl+Shift+L (Ctrl+Alt+L locks
/// the screen).
pub fn native(s: &str) -> String {
    if !LINUX {
        return s.to_string();
    }
    let Ok(k) = Keystroke::parse(s) else { return s.to_string() };
    let m = k.modifiers;
    let (mut ctrl, mut alt, mut shift) = (m.control, m.alt, m.shift);
    let mut key = k.key.clone();
    let arrow = matches!(key.as_str(), "left" | "right" | "up" | "down");
    if m.platform {
        if m.control {
            match key.as_str() {
                "left" | "right" => (ctrl, alt) = (false, true),
                "enter" | "backspace" => (ctrl, alt, shift) = (false, true, true),
                _ => return s.to_string(),
            }
        } else if !alt && (arrow || matches!(key.as_str(), "backspace" | "delete")) {
            match key.as_str() {
                "left" => key = "home".into(),
                "right" => key = "end".into(),
                "up" => (ctrl, key) = (true, "home".into()),
                "down" => (ctrl, key) = (true, "end".into()),
                _ => (ctrl, shift) = (true, true),
            }
        } else if alt && !shift && matches!(key.as_str(), "left" | "right") {
            key = if key == "left" { "pageup".into() } else { "pagedown".into() };
        } else if alt && key == "l" {
            (ctrl, alt, shift) = (true, false, true);
        } else {
            ctrl = true;
        }
    } else if alt && !ctrl && matches!(key.as_str(), "left" | "right" | "backspace" | "delete") {
        (ctrl, alt) = (true, false);
    }
    let mut out = String::new();
    for (on, m) in [(m.function, "fn-"), (ctrl, "ctrl-"), (alt, "alt-"), (shift, "shift-")] {
        if on {
            out.push_str(m);
        }
    }
    out.push_str(&key);
    out
}

/// A key someone chose (a `[keys]` override, a typed shortcut): written with ⌘ it is the
/// macOS spelling, anything else is already this platform's.
pub fn native_chosen(s: &str) -> String {
    match Keystroke::parse(s) {
        Ok(k) if k.modifiers.platform => native(s),
        _ => s.to_string(),
    }
}

/// ⌃-click is a right click on macOS; on Linux Ctrl-click is the ⌘-click (`Modifiers::secondary`).
pub fn menu_click(m: &gpui::Modifiers) -> bool {
    !LINUX && m.control
}

/// A binding in this platform's keys (`native`).
pub fn kb<A: Action>(keys: &str, action: A, context: Option<&str>) -> KeyBinding {
    KeyBinding::new(&native(keys), action, context)
}

/// A key press in this platform's keys: the checks and tests type as on macOS.
#[cfg(any(test, feature = "checks"))]
pub fn keystroke(s: &str) -> Keystroke {
    Keystroke::parse(&native(s)).unwrap()
}

/// A key written as on macOS, as this platform shows it: glyphs on macOS (⇧⌘C), names on
/// Linux (Ctrl+Shift+C).
pub fn label(keystroke: &str) -> String {
    label_native(&native(keystroke))
}

/// A key already in this platform's keys (`native`), as this platform shows it.
pub fn label_native(keystroke: &str) -> String {
    if LINUX {
        return linux_label(keystroke);
    }
    let Ok(k) = Keystroke::parse(keystroke) else { return keystroke.to_string() };
    let m = k.modifiers;
    let mut s = String::new();
    for (on, sym) in [(m.function, "fn"), (m.control, "⌃"), (m.alt, "⌥"), (m.shift, "⇧"), (m.platform, "⌘")] {
        if on {
            s.push_str(sym);
        }
    }
    let key = match k.key.as_str() {
        "enter" => "↩".to_string(),
        "backspace" => "⌫".to_string(),
        "delete" => "⌦".to_string(),
        "escape" => "esc".to_string(),
        "tab" => "Tab".to_string(),
        "space" => "Space".to_string(),
        "up" => "↑".to_string(),
        "down" => "↓".to_string(),
        "left" => "←".to_string(),
        "right" => "→".to_string(),
        "pageup" => "⇞".to_string(),
        "pagedown" => "⇟".to_string(),
        "home" => "↖".to_string(),
        "end" => "↘".to_string(),
        k if k.len() > 1 && k.starts_with('f') && k[1..].chars().all(|c| c.is_ascii_digit()) => k.to_uppercase(),
        k => k.to_uppercase(),
    };
    s.push_str(&key);
    s
}

fn linux_label(keystroke: &str) -> String {
    let Ok(k) = Keystroke::parse(keystroke) else { return keystroke.to_string() };
    let m = k.modifiers;
    let mut parts: Vec<String> = Vec::new();
    for (on, name) in [(m.function, "Fn"), (m.control, "Ctrl"), (m.alt, "Alt"), (m.shift, "Shift"), (m.platform, "Super")] {
        if on {
            parts.push(name.into());
        }
    }
    parts.push(match k.key.as_str() {
        "enter" => "Enter".into(),
        "backspace" => "Backspace".into(),
        "delete" => "Delete".into(),
        "escape" => "Esc".into(),
        "tab" => "Tab".into(),
        "space" => "Space".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "pageup" => "PgUp".into(),
        "pagedown" => "PgDn".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        k => k.to_uppercase(),
    });
    parts.join("+")
}


pub fn typed_key(k: &str) -> &str {
    match k {
        "↩" => "enter",
        "⌫" => "backspace",
        "⌦" => "delete",
        "↑" => "up",
        "↓" => "down",
        "←" => "left",
        "→" => "right",
        "⇞" => "pageup",
        "⇟" => "pagedown",
        "↖" => "home",
        "↘" => "end",
        "pgup" => "pageup",
        "pgdn" => "pagedown",
        "esc" => "escape",
        "del" => "delete",
        k => k,
    }
}

const MODS: &str = "⌃⌥⇧⌘";

/// Text holding macOS key glyphs (⌘↩, ⇧⌘G · ⇧↩, ⌘-click) in this platform's key names.
pub fn glyphs(s: &str) -> String {
    if !LINUX || !s.chars().any(|c| "⌃⌥⇧⌘↩⌫⌦⇥⇞⇟↖↘".contains(c)) {
        return s.to_string();
    }
    s.split(' ').map(glyph_token).collect::<Vec<_>>().join(" ")
}

fn glyph_token(t: &str) -> String {
    if t == "⌃-click" {
        return "right-click".into();
    }
    let split = t.char_indices().find(|(_, c)| !MODS.contains(*c)).map_or(t.len(), |(i, _)| i);
    let (mods, key) = t.split_at(split);
    let spelled: Vec<&str> = mods
        .chars()
        .map(|c| match c {
            '⌃' => "ctrl",
            '⌥' => "alt",
            '⇧' => "shift",
            _ => "cmd",
        })
        .collect();
    let names: Vec<&str> = spelled.iter().map(|m| match *m {
        "ctrl" | "cmd" => "Ctrl",
        "alt" => "Alt",
        _ => "Shift",
    }).collect();
    if let Some(rest) = key.strip_prefix("-click").or_else(|| key.strip_prefix("click")) {
        return format!("{}+click{rest}", names.join("+"));
    }
    if mods.is_empty() {
        return t
            .chars()
            .map(|c| match c {
                '↩' => "Enter".to_string(),
                '⌫' => "Backspace".to_string(),
                '⌦' => "Delete".to_string(),
                '⇥' => "Tab".to_string(),
                c => c.to_string(),
            })
            .collect();
    }
    if key.is_empty() || key == "+" {
        return format!("{}{key}", names.join("+"));
    }
    let key = match key {
        "⇥" => "tab",
        k => typed_key(k),
    };
    let spelling = format!("{}-{}", spelled.join("-"), key.to_lowercase());
    if Keystroke::parse(&spelling).is_err() {
        return t.to_string();
    }
    label(&spelling)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_text() {
        let cases: &[(&str, &str)] = if LINUX {
            &[
                ("⌘↩", "Ctrl+Enter"),
                ("⇧⌘G · ⇧↩", "Ctrl+Shift+G · Shift+Enter"),
                ("⌥⌘F", "Ctrl+Alt+F"),
                ("⌘-click", "Ctrl+click"),
                ("⌘click", "Ctrl+click"),
                ("⇧F10 / ⌃-click", "Shift+F10 / right-click"),
                ("⇥ / ⇧⇥", "Tab / Shift+Tab"),
                ("⇧ + arrows", "Shift + arrows"),
                ("⌘⌫", "Ctrl+Shift+Backspace"),
                ("↑↓←→", "↑↓←→"),
                ("run it with ⌘↩", "run it with Ctrl+Enter"),
                ("Ctrl+Shift+C", "Ctrl+Shift+C"),
                ("esc", "esc"),
            ]
        } else {
            &[("⌘↩", "⌘↩"), ("⇧F10 / ⌃-click", "⇧F10 / ⌃-click")]
        };
        for (mac, want) in cases {
            assert_eq!(glyphs(mac), *want, "{mac}");
            assert_eq!(glyphs(want), *want, "{want} twice");
        }
    }
}
