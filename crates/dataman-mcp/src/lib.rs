//! Dataman's MCP server: AI agents (Claude Code, Claude Desktop, any MCP client) use
//! Dataman's saved connections — the same `connections.toml`, settings, Keychain passwords,
//! pgpass and SSH tunnels — with read-only defaults, per-connection permissions, the app's
//! guard rules, limits on rows, bytes, time and concurrency, and an audit log.
//!
//! - [`tool`]: the `Tool` trait and the registry; [`builtin`]: the built-in tools; [`user`]:
//!   tools defined in `mcp-tools.toml`.
//! - [`policy`]: the permission matrix; [`audit`]: the JSONL log.
//! - [`server`]: the MCP handler (rmcp); [`serve_stdio`] the transport; [`ipc`] the socket
//!   through which the stdio binary hands its calls to the running app.
//! - [`resources`]: schema snapshots as `dataman://` resources.
//!
//! The binary is `dataman-mcp` (stdio); the app serves the socket.

pub mod audit;
pub mod builtin;
pub mod hub;
pub mod ipc;
pub mod policy;
pub mod resources;
pub mod server;
pub mod tool;
pub mod user;

use std::sync::Arc;

pub use hub::{Approval, Drafted, Front, Hub, TabDraft, TabRead, TabWrite, WriteShown};
pub use server::Server;
pub use tool::{Args, Call, Registry, Schema, Tool, ToolError};

use dataman_core::{Dirs, SecretStore};

/// The registry with the built-ins and the user tools of `dirs`.
pub fn registry(dirs: &Dirs) -> Arc<Registry> {
    Arc::new(Registry::standard(Some(dirs.mcp_tools_file())))
}

/// Serves MCP on stdin/stdout until the client closes them.
/// While the app runs, every tool call goes to it ([`ipc`]).
pub async fn serve_stdio(hub: Arc<Hub>, registry: Arc<Registry>) -> anyhow::Result<()> {
    let server = Server::new(hub, registry, "stdio").forwarding();
    let running = rmcp::serve_server(server, rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// A hub on the user's files and Keychain.
pub fn default_hub(secrets: Arc<dyn SecretStore>) -> anyhow::Result<Arc<Hub>> {
    Hub::new(Dirs::default_dirs()?, secrets)
}

/// `claude mcp add` for the stdio binary at `bin`.
pub fn claude_stdio_command(bin: &str) -> String {
    let bin = if bin.contains(' ') { format!("\"{bin}\"") } else { bin.to_string() };
    format!("claude mcp add dataman -- {bin}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn claude_commands() {
        assert_eq!(super::claude_stdio_command("/usr/local/bin/dataman-mcp"), "claude mcp add dataman -- /usr/local/bin/dataman-mcp");
        assert_eq!(super::claude_stdio_command("/Applications/My Apps/Dataman.app/x"), "claude mcp add dataman -- \"/Applications/My Apps/Dataman.app/x\"");
    }
}
