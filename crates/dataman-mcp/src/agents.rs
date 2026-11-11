//! The agents Dataman's server can be added to, one row each: how to tell one is on this machine,
//! and what Copy gives the user to add it. Nothing here runs or writes: the user adds it.
//! Logos: simple-icons 15.22.0, devicon 2.16.0.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dataman_core::cloudauth;
use serde_json::{Value, json};

const SERVER: &str = "dataman";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum How {
    /// `program args… dataman -- command…`. `dashes: false`: the command right after, with `--`
    /// only before its own arguments (Gemini's positional form).
    Cli { program: &'static str, args: &'static [&'static str], dashes: bool },
    /// The server's entry for a JSON file the agent reads when it starts.
    Json,
}

pub struct Agent {
    pub id: &'static str,
    pub label: &'static str,
    pub logo: &'static str,
    pub mono: bool,
    how: How,
    /// Programs, applications, folders under the home and under the apps' settings folder that
    /// mean it is here.
    bins: &'static [&'static str],
    apps: &'static [&'static str],
    home: &'static [&'static str],
    settings: &'static [&'static str],
    pub copy_hint: &'static str,
}

pub static AGENTS: [Agent; 6] = [
    Agent {
        id: "claude",
        label: "Claude Code",
        logo: "agents/claude.svg",
        mono: false,
        // Without `-s user` it lands in this folder's local scope.
        how: How::Cli { program: "claude", args: &["mcp", "add", "-s", "user"], dashes: true },
        bins: &["claude"],
        apps: &[],
        home: &[],
        settings: &[],
        copy_hint: "Run in a terminal",
    },
    Agent {
        id: "codex",
        label: "Codex",
        logo: "agents/openai.svg",
        mono: true,
        how: How::Cli { program: "codex", args: &["mcp", "add"], dashes: true },
        bins: &["codex"],
        apps: &[],
        home: &[],
        settings: &[],
        copy_hint: "Run in a terminal",
    },
    Agent {
        id: "gemini",
        label: "Gemini CLI",
        logo: "agents/gemini.svg",
        mono: false,
        // Without `-s user` it lands in this project's settings.
        how: How::Cli { program: "gemini", args: &["mcp", "add", "-s", "user"], dashes: false },
        bins: &["gemini"],
        apps: &[],
        home: &[],
        settings: &[],
        copy_hint: "Run in a terminal",
    },
    Agent {
        id: "cursor",
        label: "Cursor",
        logo: "agents/cursor.svg",
        mono: true,
        how: How::Json,
        // Not `~/.cursor`, which stays after uninstalling.
        bins: &["cursor"],
        apps: &["/Applications/Cursor.app"],
        home: &[],
        settings: &[],
        copy_hint: "Add to ~/.cursor/mcp.json",
    },
    Agent {
        id: "desktop",
        label: "Claude Desktop",
        logo: "agents/claude.svg",
        mono: false,
        how: How::Json,
        bins: &[],
        apps: &["/Applications/Claude.app"],
        home: &[],
        settings: &["Claude"],
        copy_hint: "Add to claude_desktop_config.json",
    },
    Agent {
        id: "windsurf",
        // Windsurf became Devin: the app and its CLI changed names, its settings stayed.
        label: "Devin (Windsurf)",
        logo: "agents/windsurf.svg",
        mono: true,
        how: How::Json,
        bins: &["devin-desktop", "windsurf"],
        apps: &["/Applications/Devin.app", "/Applications/Windsurf.app"],
        home: &[".codeium/windsurf"],
        settings: &[],
        copy_hint: "Add to ~/.codeium/windsurf/mcp_config.json",
    },
];

/// Where the agents' CLIs go when an installer or a package manager puts them outside the PATH an
/// app started from the Dock gets, under the home folder.
const HOME_BINS: [&str; 10] = [
    ".claude/local",
    ".npm-global/bin",
    ".bun/bin",
    ".volta/bin",
    ".asdf/shims",
    ".cargo/bin",
    "Library/pnpm",
    ".local/share/pnpm",
    "Library/Application Support/fnm/aliases/default/bin",
    ".local/share/fnm/aliases/default/bin",
];
const APP_BINS: [&str; 1] = ["/Applications/Cursor.app/Contents/Resources/app/bin"];

/// Each Node that nvm installed, newest name first: an app from the Dock has none of them on its
/// PATH, and npm's CLIs are there.
fn nvm_bins(home: &Path) -> Vec<PathBuf> {
    let Ok(versions) = std::fs::read_dir(home.join(".nvm").join("versions").join("node")) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = versions.flatten().map(|v| v.path().join("bin")).collect();
    dirs.sort_by(|a, b| b.cmp(a));
    dirs
}

pub struct Env {
    home: PathBuf,
    /// The folder apps keep their settings in: Application Support, `%APPDATA%`, `~/.config`.
    settings: PathBuf,
    dirs: Vec<PathBuf>,
}

impl Env {
    /// None without a home folder, where no agent keeps its settings.
    pub fn here() -> Option<Env> {
        let home = dataman_core::os::home()?;
        let settings = if cfg!(target_os = "macos") { Some(home.join("Library").join("Application Support")) } else if cfg!(windows) { dataman_core::os::app_data() } else { dataman_core::os::config_home() }?;
        let mut dirs = cloudauth::default_dirs();
        dirs.extend(HOME_BINS.map(|d| home.join(d)));
        dirs.extend(nvm_bins(&home));
        dirs.extend(APP_BINS.map(PathBuf::from));
        Some(Env { home, settings, dirs })
    }

    pub fn has(&self, a: &Agent) -> bool {
        a.bins.iter().any(|b| cloudauth::locate(b, &self.dirs).is_some())
            || a.apps.iter().any(|p| Path::new(p).exists())
            || a.home.iter().any(|p| self.home.join(p).exists())
            || a.settings.iter().any(|p| self.settings.join(p).exists())
    }
}

pub struct Row {
    pub agent: &'static Agent,
    pub installed: bool,
    pub copy: String,
}

/// The list as this machine shows it: installed first.
pub struct Listing {
    pub rows: Vec<Row>,
}

impl Listing {
    /// Looks at the disk: off the UI thread. No program runs.
    pub fn here() -> Arc<Listing> {
        let argv = crate::stable_server_argv(&crate::server_binary());
        Arc::new(Listing::of(Env::here(), argv))
    }

    fn of(env: Option<Env>, argv: Vec<String>) -> Listing {
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        let mut rows: Vec<Row> = AGENTS.iter().map(|agent| Row { agent, installed: env.as_ref().is_some_and(|e| e.has(agent)), copy: copy_text(agent, &words) }).collect();
        rows.sort_by_key(|r| !r.installed);
        Listing { rows }
    }
}

fn cli_args(how: How, argv: &[&str]) -> (&'static str, Vec<String>) {
    let (command, rest) = argv.split_first().map_or(("", &[][..]), |(c, r)| (*c, r));
    let How::Cli { program, args, dashes } = how else { return ("", Vec::new()) };
    let mut v: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    v.push(SERVER.into());
    if dashes {
        v.push("--".into());
    }
    v.push(command.into());
    if !dashes && !rest.is_empty() {
        v.push("--".into());
    }
    v.extend(rest.iter().map(|a| a.to_string()));
    (program, v)
}

fn server_json(argv: &[&str]) -> Value {
    match argv.split_first() {
        Some((command, [])) => json!({ "command": command }),
        Some((command, args)) => json!({ "command": command, "args": args }),
        None => json!({}),
    }
}

pub fn copy_text(a: &Agent, argv: &[&str]) -> String {
    match a.how {
        How::Cli { .. } => {
            let (program, args) = cli_args(a.how, argv);
            let words: Vec<String> = args.iter().map(|w| crate::shell_word(w)).collect();
            format!("{program} {}", words.join(" "))
        }
        How::Json => json!({ "mcpServers": { SERVER: server_json(argv) } }).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIN: &str = "/Applications/Dataman.app/Contents/MacOS/dataman-mcp";

    fn agent(id: &str) -> &'static Agent {
        AGENTS.iter().find(|a| a.id == id).unwrap()
    }

    fn env(dir: &Path) -> Env {
        Env { home: dir.join("home"), settings: dir.join("settings"), dirs: vec![dir.join("bin")] }
    }

    fn listing(dir: &Path) -> Listing {
        Listing::of(Some(env(dir)), vec![BIN.to_string()])
    }

    /// A fake program: a shell script that records its arguments, then does `then`.
    #[cfg(unix)]
    fn fake(dir: &Path, name: &str, then: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let f = bin.join(name);
        std::fs::write(&f, format!("#!/bin/sh\n[ -z \"$DATAMAN_SCRIPT_PROBE\" ] || exit 0\nfor a in \"$@\"; do echo \"$a\"; done > '{}/{name}.args'\n{then}\n", dir.display())).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A fork elsewhere in the test process can hold the file's write descriptor for an instant,
        // and exec then fails with ETXTBSY. Once one run gets through none is left.
        loop {
            match std::process::Command::new(&f).env("DATAMAN_SCRIPT_PROBE", "1").status() {
                Ok(_) => return,
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(e) => panic!("{}: {e}", f.display()),
            }
        }
    }

    #[test]
    fn copy_gives_the_user_scoped_commands() {
        let argv = [BIN];
        assert_eq!(copy_text(agent("claude"), &argv), format!("claude mcp add -s user dataman -- {BIN}"));
        assert_eq!(copy_text(agent("codex"), &argv), format!("codex mcp add dataman -- {BIN}"));
        assert_eq!(copy_text(agent("gemini"), &argv), format!("gemini mcp add -s user dataman {BIN}"));
        let want = serde_json::to_string(&json!({ "mcpServers": { "dataman": { "command": BIN } } })).unwrap();
        for id in ["cursor", "desktop", "windsurf"] {
            assert_eq!(copy_text(agent(id), &argv), want, "{id}");
        }
    }

    #[test]
    fn an_appimage_passes_its_argument_to_every_agent() {
        let argv = ["/home/u/Dataman.AppImage", "--mcp"];
        assert_eq!(copy_text(agent("claude"), &argv), "claude mcp add -s user dataman -- /home/u/Dataman.AppImage --mcp");
        assert_eq!(copy_text(agent("gemini"), &argv), "gemini mcp add -s user dataman /home/u/Dataman.AppImage -- --mcp");
        assert!(copy_text(agent("windsurf"), &argv).contains(r#""args":["--mcp"]"#));
    }

    #[test]
    fn a_folder_left_behind_is_not_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let e = env(dir.path());
        std::fs::create_dir_all(e.home.join(".cursor")).unwrap();
        // The check only holds where the app isn't installed.
        assert_eq!(e.has(agent("cursor")), Path::new("/Applications/Cursor.app").exists());
    }

    #[test]
    fn nvms_nodes_are_looked_in_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        for v in ["v20.1.0", "v22.3.0"] {
            std::fs::create_dir_all(home.join(".nvm/versions/node").join(v).join("bin")).unwrap();
        }
        let found = nvm_bins(home);
        assert!(found[0].ends_with("v22.3.0/bin") && found[1].ends_with("v20.1.0/bin"), "{found:?}");
        assert!(nvm_bins(&home.join("nowhere")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn installed_agents_come_first() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        fake(d, "gemini", "");
        std::fs::create_dir_all(d.join("settings").join("Claude")).unwrap();
        let l = listing(d);
        assert_eq!(l.rows[0].agent.id, "gemini");
        assert!(l.rows[0].installed && l.rows.iter().any(|r| r.agent.id == "desktop" && r.installed));
        assert!(!l.rows.iter().any(|r| r.agent.id == "claude" && r.installed));
    }
}
