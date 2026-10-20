use std::sync::Arc;

use dataman_core::{Keychain, MemorySecrets, SecretStore};

struct Stderr(log::LevelFilter);

impl log::Log for Stderr {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.0 && (m.target().starts_with("dataman") || m.level() <= log::Level::Warn)
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("dataman-mcp {}: {}", r.level().as_str().to_lowercase(), r.args());
        }
    }
    fn flush(&self) {}
}

fn usage() -> ! {
    eprintln!("usage: dataman-mcp [--claude] [--version]");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let level = match std::env::var("DATAMAN_MCP_LOG").ok().as_deref() {
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
                println!("dataman-mcp {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--claude" => {
                let bin = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "dataman-mcp".into());
                println!("{}", dataman_mcp::claude_stdio_command(&bin));
                return;
            }
            "-h" | "--help" => usage(),
            _ => usage(),
        }
    }

    let secrets: Arc<dyn SecretStore> = match std::env::var("DATAMAN_MCP_SECRETS").ok().as_deref() {
        Some("memory") => Arc::new(MemorySecrets::default()),
        _ => Arc::new(Keychain),
    };
    let hub = match dataman_mcp::default_hub(secrets) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("dataman-mcp: {e:#}");
            std::process::exit(1);
        }
    };
    let reg = dataman_mcp::registry(&hub.dirs);
    let code = match hub.core.block_on(dataman_mcp::serve_stdio(hub.clone(), reg)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("dataman-mcp: {e:#}");
            1
        }
    };
    hub.close_all();
    // The hub's runtime may still hold tasks with references to it: leave without dropping it.
    std::process::exit(code);
}
