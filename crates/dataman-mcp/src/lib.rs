pub mod audit;
pub mod builtin;
pub mod cli;
pub mod hub;
pub mod ipc;
pub mod policy;
pub mod resources;
pub mod server;
pub mod tool;
pub mod user;

use std::sync::Arc;

pub use hub::{Approval, Drafted, Front, Hub, TabDraft, TabRead, TabWrite, WriteShown, shown_client, shown_text};
pub use server::Server;
pub use tool::{Args, Call, Registry, Schema, Tool, ToolError};

use dataman_core::{Dirs, SecretStore};

pub fn registry(dirs: &Dirs) -> Arc<Registry> {
    Arc::new(Registry::standard(Some(dirs.mcp_tools_file())))
}

pub async fn serve_stdio(hub: Arc<Hub>, registry: Arc<Registry>) -> anyhow::Result<()> {
    let server = Server::new(hub, registry, "stdio").forwarding();
    let running = rmcp::serve_server(server, rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

pub fn default_hub(secrets: Arc<dyn SecretStore>) -> anyhow::Result<Arc<Hub>> {
    Hub::new(Dirs::default_dirs()?, secrets)
}

pub fn claude_stdio_command(bin: &str) -> String {
    claude_stdio_command_with(&[bin])
}

/// The server's command line (the binary and its arguments), quoted for a POSIX shell.
pub fn claude_stdio_command_with(argv: &[&str]) -> String {
    let words: Vec<String> = argv.iter().map(|a| shell_word(a)).collect();
    format!("claude mcp add dataman -- {}", words.join(" "))
}

fn shell_word(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+=:@%,".contains(&b)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The server's binary: `dataman-mcp` beside the running one (in the packages, a link to the
/// app's binary), or the running one when it is not there.
pub fn server_binary() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("dataman-mcp")))
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "dataman-mcp".into())
}

/// Run from an AppImage, the binary sits in a mount that changes on every launch: the stable
/// command is the AppImage itself, whose AppRun starts the server for `--mcp`.
pub fn stable_server_argv(bin: &str) -> Vec<String> {
    match std::env::var("APPIMAGE") {
        Ok(img) if cfg!(target_os = "linux") && !img.is_empty() && std::path::Path::new(&img).is_file() => vec![img, "--mcp".into()],
        _ => vec![bin.to_string()],
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn claude_commands() {
        assert_eq!(super::claude_stdio_command("/usr/local/bin/dataman-mcp"), "claude mcp add dataman -- /usr/local/bin/dataman-mcp");
        assert_eq!(super::claude_stdio_command("/Applications/My Apps/Dataman.app/x"), "claude mcp add dataman -- '/Applications/My Apps/Dataman.app/x'");
        assert_eq!(super::claude_stdio_command("/home/o'neil/$x `y`"), "claude mcp add dataman -- '/home/o'\\''neil/$x `y`'");
        assert_eq!(super::claude_stdio_command_with(&["/home/u/Apps/Dataman-x86_64.AppImage", "--mcp"]), "claude mcp add dataman -- /home/u/Apps/Dataman-x86_64.AppImage --mcp");
    }
}
