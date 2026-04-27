//! `borg mcp stdio` subcommand — runs the MCP server bound to stdin/stdout.
//!
//! Spawned by Claude Desktop's MCP config or by MCP Inspector. The
//! tool registry is wired with read-only tools only — mutating tools
//! are excluded by design (see `gateway::mcp` module docs).

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::Subcommand;
use cortex::config::Config;
use gateway::mcp::{stdio, McpError, McpServer, McpToolDescriptor, ReadOnlyToolHandler};

#[derive(Subcommand)]
pub(crate) enum McpAction {
    /// Run the MCP server over stdio (Content-Length-framed JSON-RPC).
    Stdio,
}

/// Dispatch for `borg mcp …`.
pub(crate) async fn dispatch_mcp(action: Option<McpAction>) -> Result<()> {
    match action.unwrap_or(McpAction::Stdio) {
        McpAction::Stdio => run_stdio().await,
    }
}

async fn run_stdio() -> Result<()> {
    let tools = build_tool_registry();
    let server = Arc::new(McpServer::new(tools, env!("CARGO_PKG_VERSION")));
    stdio::run(server, tokio::io::stdin(), tokio::io::stdout())
        .await
        .context("MCP stdio loop failed")
}

/// Build the production tool registry. Each tool wraps a
/// `cortex::tool_handlers::*` invocation. v1 wires only read-only
/// tools per the task spec's risks-to-watch.
fn build_tool_registry() -> Vec<Box<dyn ReadOnlyToolHandler>> {
    vec![
        Box::new(MemorySearchTool),
        Box::new(ReadMemoryTool),
        Box::new(WebFetchTool),
        Box::new(WebSearchTool),
        Box::new(ListTool),
    ]
}

/// Helper: build a uniform "args were not the expected shape" error.
fn args_error(detail: &str) -> McpError {
    McpError::invalid_params(format!("argument error: {detail}"))
}

struct MemorySearchTool;

#[async_trait]
impl ReadOnlyToolHandler for MemorySearchTool {
    fn descriptor(&self) -> McpToolDescriptor {
        McpToolDescriptor {
            name: "memory_search".into(),
            description:
                "Hybrid vector + BM25 search over Borg's persistent memory and session history."
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"},
                    "top_k": {"type": "integer", "description": "Max results", "default": 10}
                },
                "required": ["query"]
            }),
        }
    }
    async fn call(&self, arguments: serde_json::Value) -> Result<serde_json::Value, McpError> {
        let config = Config::load_from_db()
            .map_err(|e| McpError::internal(format!("config load failed: {e}")))?;
        let result = cortex::tool_handlers::handle_memory_search(&arguments, &config)
            .await
            .map_err(|e| McpError::internal(format!("memory_search failed: {e}")))?;
        Ok(serde_json::Value::String(result))
    }
}

struct ReadMemoryTool;
#[async_trait]
impl ReadOnlyToolHandler for ReadMemoryTool {
    fn descriptor(&self) -> McpToolDescriptor {
        McpToolDescriptor {
            name: "read_memory".into(),
            description: "Read a memory entry by scope+name.".into(),
            input_schema: serde_json::json!({
                "type":"object",
                "properties":{
                    "scope":{"type":"string"},
                    "name":{"type":"string"}
                },
                "required":["scope","name"]
            }),
        }
    }
    async fn call(&self, arguments: serde_json::Value) -> Result<serde_json::Value, McpError> {
        let name = arguments
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| args_error("missing string `name`"))?;
        let scope = arguments
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global");
        let content = cortex::memory::read_memory_db_or_not_found(name, scope)
            .map_err(|e| McpError::internal(format!("read_memory failed: {e}")))?;
        Ok(serde_json::Value::String(content))
    }
}

struct WebFetchTool;
#[async_trait]
impl ReadOnlyToolHandler for WebFetchTool {
    fn descriptor(&self) -> McpToolDescriptor {
        McpToolDescriptor {
            name: "web_fetch".into(),
            description: "Fetch URL content. Subject to web.enabled policy.".into(),
            input_schema: serde_json::json!({
                "type":"object",
                "properties":{"url":{"type":"string"}},
                "required":["url"]
            }),
        }
    }
    async fn call(&self, arguments: serde_json::Value) -> Result<serde_json::Value, McpError> {
        let config = Config::load_from_db()
            .map_err(|e| McpError::internal(format!("config load failed: {e}")))?;
        let result = cortex::tool_handlers::handle_web_fetch(&arguments, &config)
            .await
            .map_err(|e| McpError::internal(format!("web_fetch failed: {e}")))?;
        Ok(serde_json::Value::String(result))
    }
}

struct WebSearchTool;
#[async_trait]
impl ReadOnlyToolHandler for WebSearchTool {
    fn descriptor(&self) -> McpToolDescriptor {
        McpToolDescriptor {
            name: "web_search".into(),
            description: "Web search. Subject to web.enabled policy.".into(),
            input_schema: serde_json::json!({
                "type":"object",
                "properties":{"query":{"type":"string"}},
                "required":["query"]
            }),
        }
    }
    async fn call(&self, arguments: serde_json::Value) -> Result<serde_json::Value, McpError> {
        let config = Config::load_from_db()
            .map_err(|e| McpError::internal(format!("config load failed: {e}")))?;
        let result = cortex::tool_handlers::handle_web_search(&arguments, &config)
            .await
            .map_err(|e| McpError::internal(format!("web_search failed: {e}")))?;
        Ok(serde_json::Value::String(result))
    }
}

struct ListTool;
#[async_trait]
impl ReadOnlyToolHandler for ListTool {
    fn descriptor(&self) -> McpToolDescriptor {
        McpToolDescriptor {
            name: "list".into(),
            description: "List Borg resources (tools/skills/channels/agents) — read-only.".into(),
            input_schema: serde_json::json!({
                "type":"object",
                "properties":{"kind":{"type":"string","enum":["tools","skills","channels","agents"]}},
                "required":["kind"]
            }),
        }
    }
    async fn call(&self, arguments: serde_json::Value) -> Result<serde_json::Value, McpError> {
        // The core handler takes `what` as the discriminator; remap
        // from MCP's `kind` so the schema we advertise is more
        // self-explanatory to MCP clients.
        let kind = arguments
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| args_error("missing string `kind`"))?;
        let remapped = serde_json::json!({"what": kind});
        let config = Config::load_from_db()
            .map_err(|e| McpError::internal(format!("config load failed: {e}")))?;
        // `agent_control = None`: MCP can't list spawned sub-agents
        // without a live agent loop. The core handler returns a clear
        // error in that case for `kind=agents`, which is fine.
        let result = cortex::tool_handlers::handle_list(&remapped, &config, None)
            .map_err(|e| McpError::internal(format!("list failed: {e}")))?;
        Ok(serde_json::Value::String(result))
    }
}
