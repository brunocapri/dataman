//! `diskman-mcp`: Diskman's MCP server.
//!
//! ```text
//! diskman-mcp            stdio (claude mcp add diskman -- diskman-mcp)
//! diskman-mcp --claude   prints the claude mcp add command for this binary
//! ```
//!
//! While the app runs, every tool call goes to it through `app.sock`. Alone, it reads the app's
//! files (`$XDG_CONFIG_HOME/diskman` or `~/.config/diskman`) and the Keychain. `DISKMAN_MCP_SECRETS=memory` keeps it off the Keychain (tests; passwords then
//! come from pgpass only). Logs go to stderr (`DISKMAN_MCP_LOG=debug|info|warn`).

use std::sync::Arc;

use diskman_core::{Keychain, MemorySecrets, SecretStore};

struct Stderr(log::LevelFilter);

impl log::Log for Stderr {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.0 && (m.target().starts_with("diskman") || m.level() <= log::Level::Warn)
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("diskman-mcp {}: {}", r.level().as_str().to_lowercase(), r.args());
        }
    }
    fn flush(&self) {}
}

fn usage() -> ! {
    eprintln!("usage: diskman-mcp [--claude] [--version]");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let level = match std::env::var("DISKMAN_MCP_LOG").ok().as_deref() {
        Some("debug") => log::LevelFilter::Debug,
        Some("info") => log::LevelFilter::Info,
        Some("off") => log::LevelFilter::Off,
        _ => log::LevelFilter::Warn,
    };
    let logger: &'static Stderr = Box::leak(Box::new(Stderr(level)));
    let _ = log::set_logger(logger).map(|()| log::set_max_level(level));

    for a in &args {
        match a.as_str() {
            "--version" | "-V" => {
                println!("diskman-mcp {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--claude" => {
                let bin = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "diskman-mcp".into());
                println!("{}", diskman_mcp::claude_stdio_command(&bin));
                return;
            }
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }

    let secrets: Arc<dyn SecretStore> = match std::env::var("DISKMAN_MCP_SECRETS").ok().as_deref() {
        Some("memory") => Arc::new(MemorySecrets::default()),
        _ => Arc::new(Keychain),
    };
    let hub = match diskman_mcp::default_hub(secrets) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("diskman-mcp: {e:#}");
            std::process::exit(1);
        }
    };
    let reg = diskman_mcp::registry(&hub.dirs);
    let code = match hub.core.block_on(diskman_mcp::serve_stdio(hub.clone(), reg)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("diskman-mcp: {e:#}");
            1
        }
    };
    hub.close_all();
    // The hub's runtime may still hold tasks with references to it: leave without dropping it.
    std::process::exit(code);
}
