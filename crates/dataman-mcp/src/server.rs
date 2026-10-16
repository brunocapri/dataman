//! The MCP surface (rmcp's `ServerHandler`): tools from the [`Registry`], schema resources, and
//! the audit line of every call.

use std::sync::Arc;
use std::time::Instant;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, InitializeResult, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    Resource, ResourceContents, ResourceTemplate, ServerCapabilities, ServerConfig, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::Value;

use crate::audit::{Entry, Outcome, full, redact};
use crate::hub::Hub;
use crate::policy::Need;
use crate::resources;
use crate::tool::{Args, Call, Registry, Tool, ToolError};

pub const INSTRUCTIONS: &str = "Databases through Dataman's saved connections. Start with list_connections, then list_tables / \
search_objects / describe_table (or the dataman:// schema resources). run_query is read-only and bounded (rows, bytes, time); \
pass values as params; several reads separated by ; come back as results: [...], one per statement. open_query runs SQL \
visibly in the user's Dataman (an agent tab); set run: false to only place the statement in the user's tab for them to \
review and run themselves (use this for writes the user wants to run personally). The user's favorite queries: \
list_favorites, and run_favorite runs one by its name (close is enough; an unclear name returns the candidates to choose \
from and runs nothing). Writes (execute, open_query, run_favorite) follow \
each connection's MCP tier, set by the user: read refuses them; write runs non-destructive DML (INSERT, UPDATE/DELETE with a real \
WHERE) within a row limit; full also allows destructive statements, which wait for the user's Allow in the app, as do writes over \
the row limit and every write on production. You cannot approve anything yourself. Results: columns as [name, type], rows as \
arrays, null is SQL NULL. create_connection and update_connection save connections; passwords are write-only (never returned).";

/// The handler for one transport. Clones share the hub and the registry.
#[derive(Clone)]
pub struct Server {
    pub hub: Arc<Hub>,
    pub registry: Arc<Registry>,
    pub transport: &'static str,
    /// Tool calls go to the running app first ([`crate::ipc`]): the stdio binary's.
    pub forward: bool,
}

impl Server {
    pub fn new(hub: Arc<Hub>, registry: Arc<Registry>, transport: &'static str) -> Server {
        Server { hub, registry, transport, forward: false }
    }

    /// This server hands its tool calls to the app while it runs.
    pub fn forwarding(mut self) -> Server {
        self.forward = true;
        self
    }

    fn client(cx: &RequestContext<RoleServer>) -> String {
        cx.client_info().map(|i| i.name).unwrap_or_else(|| "unknown".into())
    }

    fn audit(&self, call: &Call, started: Instant, outcome: Outcome, error: Option<String>) {
        let n = call.note();
        self.hub.audit.write(&Entry {
            ts: chrono::Utc::now(),
            client: call.client.clone(),
            transport: self.transport.into(),
            tool: call.tool.clone(),
            connection: n.connection,
            database: n.database,
            sql: n.sql.as_deref().map(redact),
            query: n.sql.as_deref().and_then(full),
            rows: n.rows,
            ms: (started.elapsed().as_secs_f64() * 1e4).round() / 10.0,
            outcome,
            error,
            approval: n.approval,
            draft: n.draft,
            favorite: n.favorite,
        });
    }

    /// Runs one tool call: validation, the tool, the audit line. Errors the agent should read
    /// come back as tool results with `isError`.
    pub async fn run_tool(&self, tool: Arc<dyn Tool>, args: serde_json::Map<String, Value>, client: String, ct: tokio_util::sync::CancellationToken) -> CallToolResult {
        let started = Instant::now();
        let call = Call::new(self.hub.clone(), client, self.transport, tool.name(), ct);
        let r = match tool.schema().validate(&args) {
            Err(e) => Err(ToolError::Invalid(e)),
            Ok(()) => tool.call(&call, Args(args)).await,
        };
        match r {
            Ok(v) => {
                self.audit(&call, started, Outcome::Ok, None);
                let text = serde_json::to_string(&v).unwrap_or_default();
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            Err(e) => {
                let msg = e.message();
                self.audit(&call, started, e.outcome(), Some(msg.clone()));
                CallToolResult::error(vec![ContentBlock::text(msg)])
            }
        }
    }
}

/// The MCP form of a tool, with hints from what it needs.
pub fn describe(t: &dyn Tool) -> rmcp::model::Tool {
    let ann = match t.need() {
        Some(Need::Write) => ToolAnnotations::new().read_only(false).destructive(true).idempotent(false).open_world(false),
        // They change the saved connections (not a database).
        None if matches!(t.name(), "create_connection" | "update_connection") => ToolAnnotations::new().read_only(false).destructive(false).idempotent(true).open_world(false),
        _ => ToolAnnotations::new().read_only(true).destructive(false).idempotent(true).open_world(false),
    };
    rmcp::model::Tool::new(t.name().to_string(), t.description().to_string(), Arc::new(t.schema().0.clone())).with_annotations(ann)
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        let caps = ServerCapabilities::builder().enable_tools().enable_tool_list_changed().enable_resources().build();
        InitializeResult::new(caps)
            .with_server_info(Implementation::new("dataman", env!("CARGO_PKG_VERSION")).with_title("Dataman"))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(&self, _: Option<PaginatedRequestParams>, _: RequestContext<RoleServer>) -> Result<ListToolsResult, McpError> {
        let tools = self.registry.all().iter().map(|t| describe(t.as_ref())).collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(&self, req: CallToolRequestParams, cx: RequestContext<RoleServer>) -> Result<CallToolResponse, McpError> {
        let changed = self.registry.refresh();
        if changed {
            let _ = cx.peer.notify_tool_list_changed().await;
        }
        let Some(tool) = self.registry.find(&req.name) else {
            return Err(McpError::invalid_params(format!("unknown tool {:?}", req.name), None));
        };
        let client = Self::client(&cx);
        let args = req.arguments.unwrap_or_default();
        // The app runs it when it is up (its tabs, its human); else it runs here, headless.
        if self.forward {
            match crate::ipc::call(&self.hub.dirs, &req.name, args.clone(), &client, cx.ct.clone()).await {
                Ok(r) => return Ok(r.into()),
                Err(crate::ipc::CallError::Absent(e)) => log::debug!("running {} here: {e}", req.name),
                Err(e) => return Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())]).into()),
            }
        }
        Ok(self.run_tool(tool, args, client, cx.ct.clone()).await.into())
    }

    async fn list_resources(&self, _: Option<PaginatedRequestParams>, cx: RequestContext<RoleServer>) -> Result<ListResourcesResult, McpError> {
        let call = Call::new(self.hub.clone(), Self::client(&cx), self.transport, "resources/list", cx.ct.clone());
        let items = resources::list(&call)
            .into_iter()
            .map(|(uri, name, json)| Resource::new(uri, name).with_mime_type(if json { "application/json" } else { "text/markdown" }))
            .collect();
        Ok(ListResourcesResult::with_all_items(items))
    }

    async fn list_resource_templates(&self, _: Option<PaginatedRequestParams>, _: RequestContext<RoleServer>) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("dataman://{connection}/{database}/schema", "Schema snapshot (Markdown)")
                .with_description("Tables, columns, keys and row estimates of a database; add ?refresh=true to rebuild.")
                .with_mime_type("text/markdown"),
            ResourceTemplate::new("dataman://{connection}/{database}/schema.json", "Schema snapshot (JSON)")
                .with_description("The same snapshot as JSON.")
                .with_mime_type("application/json"),
        ]))
    }

    async fn read_resource(&self, req: ReadResourceRequestParams, cx: RequestContext<RoleServer>) -> Result<ReadResourceResponse, McpError> {
        let Some(u) = resources::parse(&req.uri) else {
            return Err(McpError::resource_not_found(format!("unknown resource {}", req.uri), None));
        };
        let started = Instant::now();
        let call = Call::new(self.hub.clone(), Self::client(&cx), self.transport, "resources/read", cx.ct.clone());
        match resources::read(&call, &u).await {
            Ok(text) => {
                self.audit(&call, started, Outcome::Ok, None);
                let mime = if u.json { "application/json" } else { "text/markdown" };
                let contents = ResourceContents::TextResourceContents { uri: req.uri.clone(), mime_type: Some(mime.into()), text: text.to_string(), meta: None };
                Ok(ReadResourceResult::new(vec![contents]).into())
            }
            Err(e) => {
                let msg = e.message();
                self.audit(&call, started, e.outcome(), Some(msg.clone()));
                Err(McpError::invalid_params(msg, None))
            }
        }
    }
}
