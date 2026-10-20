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
