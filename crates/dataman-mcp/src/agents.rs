//! The agents Dataman's server can be added to, one row each: how to tell one is on this machine,
//! what Add does, and what Copy gives instead. Add only ever runs on the user's click, and one
//! agent's answer says nothing about another's. Logos: simple-icons 15.22.0, devicon 2.16.0.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dataman_core::cloudauth;
use serde_json::{Value, json};

const SERVER: &str = "dataman";
const TIMEOUT: Duration = Duration::from_secs(30);

/// What Add does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum How {
    /// `program args… dataman -- command…`. `dashes: false`: the command right after, with `--`
    /// only before its own arguments (Gemini's positional form).
    Cli { program: &'static str, args: &'static [&'static str], dashes: bool },
    /// `code --add-mcp {json}`.
    AddMcp { program: &'static str },
    /// A link the app confirms in itself (Cursor's).
    Deeplink,
    /// The server merged into a JSON file the agent reads when it starts.
    Json(ConfigFile),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigFile {
    ClaudeDesktop,
    Windsurf,
}

pub struct Agent {
    pub id: &'static str,
    pub label: &'static str,
    pub logo: &'static str,
    /// The logo is one shape, drawn in the theme's text color.
    pub mono: bool,
    how: How,
    /// Programs, applications and folders under the home that mean it is here.
    bins: &'static [&'static str],
    apps: &'static [&'static str],
    home: &'static [&'static str],
    /// The answer once Add is done, and whether the user still has something to do.
    pub done: &'static str,
    pub waits: bool,
    pub add_hint: &'static str,
    pub copy_hint: &'static str,
}

pub static AGENTS: [Agent; 7] = [
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
        done: "Added",
        waits: false,
        add_hint: "Runs claude mcp add",
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
        done: "Added",
        waits: false,
        add_hint: "Runs codex mcp add",
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
        done: "Added",
        waits: false,
        add_hint: "Runs gemini mcp add",
        copy_hint: "Run in a terminal",
    },
    Agent {
        id: "vscode",
        label: "VS Code",
        logo: "agents/vscode.svg",
        mono: false,
        how: How::AddMcp { program: "code" },
        bins: &["code"],
        apps: &["/Applications/Visual Studio Code.app"],
        home: &[],
        done: "Confirm in VS Code",
        waits: true,
        add_hint: "Opens VS Code to install it",
        copy_hint: "Run in a terminal",
    },
    Agent {
        id: "cursor",
        label: "Cursor",
        logo: "agents/cursor.svg",
        mono: true,
        how: How::Deeplink,
        bins: &["cursor"],
        apps: &["/Applications/Cursor.app"],
        home: &[".cursor"],
        done: "Confirm in Cursor",
        waits: true,
        add_hint: "Opens Cursor to install it",
        copy_hint: "Add to ~/.cursor/mcp.json",
    },
    Agent {
        id: "desktop",
        label: "Claude Desktop",
        logo: "agents/claude.svg",
        mono: false,
        how: How::Json(ConfigFile::ClaudeDesktop),
        bins: &[],
        apps: &["/Applications/Claude.app"],
        home: &[],
        done: "Added, restart it",
        waits: false,
        add_hint: "Adds it to claude_desktop_config.json",
        copy_hint: "Add to claude_desktop_config.json",
    },
    Agent {
        id: "windsurf",
        label: "Windsurf",
        logo: "agents/windsurf.svg",
        mono: true,
        how: How::Json(ConfigFile::Windsurf),
        bins: &["windsurf"],
        apps: &["/Applications/Windsurf.app"],
        home: &[".codeium/windsurf"],
        done: "Added, restart it",
        waits: false,
        add_hint: "Adds it to mcp_config.json",
        copy_hint: "Add to ~/.codeium/windsurf/mcp_config.json",
    },
];

/// Where the agents' CLIs go when an installer or a package manager puts them outside the PATH an
/// app started from the Dock gets, under the home folder.
const HOME_BINS: [&str; 6] = [".claude/local", ".npm-global/bin", ".bun/bin", ".volta/bin", ".asdf/shims", ".cargo/bin"];
const APP_BINS: [&str; 2] = ["/Applications/Visual Studio Code.app/Contents/Resources/app/bin", "/Applications/Cursor.app/Contents/Resources/app/bin"];

/// This machine, as the agents see it.
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
        dirs.extend(APP_BINS.map(PathBuf::from));
        Some(Env { home, settings, dirs })
    }

    fn config_file(&self, f: ConfigFile) -> PathBuf {
        match f {
            ConfigFile::ClaudeDesktop => self.settings.join("Claude").join("claude_desktop_config.json"),
            ConfigFile::Windsurf => self.home.join(".codeium").join("windsurf").join("mcp_config.json"),
        }
    }

    pub fn has(&self, a: &Agent) -> bool {
        a.bins.iter().any(|b| cloudauth::locate(b, &self.dirs).is_some())
            || a.apps.iter().any(|p| Path::new(p).exists())
            || a.home.iter().any(|p| self.home.join(p).exists())
            || matches!(a.how, How::Json(f) if self.config_file(f).parent().is_some_and(Path::exists))
    }
}

/// One row of the list.
pub struct Row {
    /// Its place in [`AGENTS`].
    pub index: usize,
    pub agent: &'static Agent,
    pub installed: bool,
    /// What Copy gives.
    pub copy: String,
}

/// The list as this machine shows it: installed first, and what they all run.
pub struct Listing {
    pub rows: Vec<Row>,
    env: Option<Env>,
    argv: Vec<String>,
}

impl Listing {
    /// Looks at the disk: off the UI thread. No program runs.
    pub fn here() -> Arc<Listing> {
        let argv = crate::stable_server_argv(&crate::server_binary());
        Arc::new(Listing::of(Env::here(), argv))
    }

    fn of(env: Option<Env>, argv: Vec<String>) -> Listing {
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        let mut rows: Vec<Row> = AGENTS
            .iter()
            .enumerate()
            .map(|(index, agent)| Row { index, agent, installed: env.as_ref().is_some_and(|e| e.has(agent)), copy: copy_text(agent, &words) })
            .collect();
        rows.sort_by_key(|r| !r.installed);
        Listing { rows, env, argv }
    }
}

/// The program's arguments for the CLIs.
fn cli_args(how: How, argv: &[&str]) -> (&'static str, Vec<String>) {
    let (command, rest) = argv.split_first().map_or(("", &[][..]), |(c, r)| (*c, r));
    match how {
        How::Cli { program, args, dashes } => {
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
        How::AddMcp { program } => {
            let mut server = server_json(argv);
            server.as_object_mut().expect("server_json makes an object").insert("name".into(), json!(SERVER));
            (program, vec!["--add-mcp".into(), server.to_string()])
        }
        How::Deeplink | How::Json(_) => ("", Vec::new()),
    }
}

fn server_json(argv: &[&str]) -> Value {
    match argv.split_first() {
        Some((command, [])) => json!({ "command": command }),
        Some((command, args)) => json!({ "command": command, "args": args }),
        None => json!({}),
    }
}

/// A word that holds JSON, quoted for this platform's shell (`shell_word` takes `"` out on
/// Windows, where a path never has one).
fn json_word(s: &str) -> String {
    if cfg!(windows) { format!("\"{}\"", s.replace('"', "\\\"")) } else { crate::shell_word(s) }
}

/// What Copy gives: the command for a terminal, or the JSON for a settings file.
pub fn copy_text(a: &Agent, argv: &[&str]) -> String {
    match a.how {
        How::Cli { .. } => {
            let (program, args) = cli_args(a.how, argv);
            let words: Vec<String> = args.iter().map(|w| crate::shell_word(w)).collect();
            format!("{program} {}", words.join(" "))
        }
        How::AddMcp { .. } => {
            let (program, args) = cli_args(a.how, argv);
            format!("{program} {} {}", args[0], json_word(&args[1]))
        }
        How::Deeplink | How::Json(_) => json!({ "mcpServers": { SERVER: server_json(argv) } }).to_string(),
    }
}

/// `file` (its text, when it exists) with this server in it, and every other key kept. Anything
/// that does not read as such a file is an error: it is never overwritten.
fn merged(file: Option<&str>, argv: &[&str]) -> Result<String, String> {
    let mut root: Value = match file.map(str::trim) {
        None | Some("") => json!({}),
        Some(text) => serde_json::from_str(text).map_err(|e| format!("its settings file is not JSON: {e}"))?,
    };
    let top = root.as_object_mut().ok_or("its settings file is not a JSON object")?;
    let servers = top.entry("mcpServers").or_insert_with(|| json!({})).as_object_mut().ok_or("its mcpServers is not a JSON object")?;
    servers.insert(SERVER.into(), server_json(argv));
    serde_json::to_string_pretty(&root).map(|s| s + "\n").map_err(|e| e.to_string())
}

fn merge_into(path: &Path, argv: &[&str]) -> Result<(), String> {
    let shown = path.display();
    let text = match std::fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{shown}: {e}")),
    };
    let out = merged(text.as_deref(), argv).map_err(|e| format!("{shown}: {e}"))?;
    // Through a link (a dotfiles repository's), and as private as it was.
    let real = dataman_core::os::canonical(path).unwrap_or_else(|_| path.to_path_buf());
    #[cfg(unix)]
    let private = {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(&real).is_ok_and(|m| m.permissions().mode() & 0o077 == 0)
    };
    #[cfg(not(unix))]
    let private = false;
    dataman_core::os::atomic_write(&real, out.as_bytes(), private).map_err(|e| format!("{shown}: {e}"))
}

async fn run(program: &str, args: &[String], env: &Env) -> Result<(), String> {
    let path = cloudauth::locate(program, &env.dirs).ok_or_else(|| format!("{program} not found"))?;
    let mut cmd = cloudauth::command(&path, &env.dirs);
    cmd.args(args);
    let out = match tokio::time::timeout(TIMEOUT, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("{program} did not start: {e}")),
        Err(_) => return Err(format!("{program} did not answer in {} s", TIMEOUT.as_secs())),
    };
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Added before: what the click asked for is already so.
    if stderr.to_ascii_lowercase().contains("already exists") {
        return Ok(());
    }
    Err(stderr.lines().map(str::trim).find(|l| !l.is_empty()).map_or_else(|| format!("{program} exited with {}", out.status), str::to_string))
}

/// What Add left to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Added {
    Done,
    /// A link for the app to open.
    Open(String),
}

fn cursor_link(argv: &[&str]) -> String {
    let config = data_encoding::BASE64.encode(server_json(argv).to_string().as_bytes());
    let config = config.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D");
    format!("cursor://anysphere.cursor-deeplink/mcp/install?name={SERVER}&config={config}")
}

/// Adds the server to `a`. Takes up to [`TIMEOUT`]: on the core's runtime, never the UI thread.
pub async fn add(a: &Agent, listing: &Listing) -> Result<Added, String> {
    let env = listing.env.as_ref().ok_or("no home folder")?;
    let argv: Vec<&str> = listing.argv.iter().map(String::as_str).collect();
    match a.how {
        How::Cli { .. } | How::AddMcp { .. } => {
            let (program, args) = cli_args(a.how, &argv);
            run(program, &args, env).await.map(|()| Added::Done)
        }
        How::Deeplink => Ok(Added::Open(cursor_link(&argv))),
        How::Json(f) => merge_into(&env.config_file(f), &argv).map(|()| Added::Done),
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
        std::fs::write(&f, format!("#!/bin/sh\nfor a in \"$@\"; do echo \"$a\"; done > '{}/{name}.args'\n{then}\n", dir.display())).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
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
        let vscode = copy_text(agent("vscode"), &argv);
        assert!(vscode.starts_with("code --add-mcp ") && vscode.contains(r#""name":"dataman""#) && vscode.contains(BIN), "{vscode}");
    }

    #[test]
    fn an_appimage_passes_its_argument_to_every_agent() {
        let argv = ["/home/u/Dataman.AppImage", "--mcp"];
        assert_eq!(copy_text(agent("claude"), &argv), "claude mcp add -s user dataman -- /home/u/Dataman.AppImage --mcp");
        assert_eq!(copy_text(agent("gemini"), &argv), "gemini mcp add -s user dataman /home/u/Dataman.AppImage -- --mcp");
        assert!(copy_text(agent("windsurf"), &argv).contains(r#""args":["--mcp"]"#));
    }

    #[test]
    fn merging_keeps_every_other_server_and_setting() {
        let before = r#"{"theme":"dark","mcpServers":{"other":{"command":"x"}}}"#;
        let v: Value = serde_json::from_str(&merged(Some(before), &[BIN]).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"]["dataman"]["command"], BIN);
        let v: Value = serde_json::from_str(&merged(None, &[BIN]).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["dataman"]["command"], BIN);
        for bad in ["{ not json", "[1,2]", r#"{"mcpServers":3}"#] {
            assert!(merged(Some(bad), &[BIN]).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn an_unreadable_settings_file_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let l = listing(dir.path());
        let f = l.env.as_ref().unwrap().config_file(ConfigFile::Windsurf);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(&f, "{ half a file").unwrap();
        assert!(add(agent("windsurf"), &l).await.is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "{ half a file");
        std::fs::write(&f, r#"{"mcpServers":{"other":{"command":"x"}}}"#).unwrap();
        assert_eq!(add(agent("windsurf"), &l).await, Ok(Added::Done));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!((v["mcpServers"]["other"]["command"].as_str(), v["mcpServers"]["dataman"]["command"].as_str()), (Some("x"), Some(BIN)));
        assert_eq!(add(agent("desktop"), &l).await, Ok(Added::Done), "a settings file not there yet is made");
        assert!(l.env.as_ref().unwrap().config_file(ConfigFile::ClaudeDesktop).is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_clis_run_with_their_scope_and_answer_for_themselves() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let l = listing(d);
        fake(d, "claude", "exit 0");
        fake(d, "codex", "echo 'boom: no config' >&2; exit 1");
        fake(d, "gemini", "echo 'MCP server dataman already exists in user' >&2; exit 1");
        assert_eq!(add(agent("claude"), &l).await, Ok(Added::Done));
        let args = std::fs::read_to_string(d.join("claude.args")).unwrap();
        assert_eq!(args.lines().collect::<Vec<_>>(), ["mcp", "add", "-s", "user", "dataman", "--", BIN]);
        assert_eq!(add(agent("codex"), &l).await, Err("boom: no config".to_string()));
        assert_eq!(add(agent("gemini"), &l).await, Ok(Added::Done), "added before");
        assert_eq!(add(agent("vscode"), &l).await, Err("code not found".to_string()));
    }

    #[test]
    fn cursors_link_carries_the_server_as_base64() {
        let link = cursor_link(&[BIN]);
        let config = link.split("config=").nth(1).unwrap().replace("%3D", "=").replace("%2F", "/").replace("%2B", "+");
        let raw = data_encoding::BASE64.decode(config.as_bytes()).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&raw).unwrap(), json!({ "command": BIN }));
        assert!(link.starts_with("cursor://anysphere.cursor-deeplink/mcp/install?name=dataman&"));
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
