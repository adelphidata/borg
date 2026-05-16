//! Production [`ReadOnlyToolHandler`] impls — adapt the read-only
//! cortex tool handlers (`memory_search`, `read_memory`, `web_fetch`,
//! `web_search`, `list`) to the MCP tool surface.
//!
//! Every handler returns its result as a plain JSON string so the
//! MCP route's content-block wrapper renders cleanly in MCP-aware UIs.
//! Errors from the underlying handler become `McpError::internal` so
//! the JSON-RPC layer surfaces them as `-32603`.

use async_trait::async_trait;
use cortex::config::Config;

use super::server::{McpError, McpToolDescriptor, ReadOnlyToolHandler};

/// `memory_search` — hybrid vector+BM25 search across memory + sessions.
pub struct MemorySearchHandler {
    config: Config,
}

impl MemorySearchHandler {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

#[async_trait]
impl ReadOnlyToolHandler for MemorySearchHandler {
    fn descriptor(&self) -> McpToolDescriptor {
        McpToolDescriptor {
            name: "memory_search".into(),
            description:
                "Hybrid vector+BM25 search across long-term memory and past session transcripts."
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 50},
                    "min_score": {"type": "number"},
                    "sources": {
                        "type": "array",
                        "items": {"type": "string", "enum": ["memory", "messages"]}
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<serde_json::Value, McpError> {
        cortex::tool_handlers::handle_memory_search(&arguments, &self.config)
            .await
            .map(serde_json::Value::String)
            .map_err(|e| McpError::internal(format!("memory_search failed: {e}")))
    }
}

/// `read_memory` — fetch a long-term memory entry by name.
pub struct ReadMemoryHandler;

#[async_trait]
impl ReadOnlyToolHandler for ReadMemoryHandler {
    fn descriptor(&self) -> McpToolDescriptor {
        McpToolDescriptor {
            name: "read_memory".into(),
            description: "Read a named long-term memory entry. Use `scope=global` for shared, \
                          `scope=project:<id>` for project-scoped."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "filename": {"type": "string"},
                    "scope": {"type": "string"}
                },
                "required": ["filename"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<serde_json::Value, McpError> {
        cortex::tool_handlers::handle_read_memory(&arguments)
            .await
            .map(serde_json::Value::String)
            .map_err(|e| McpError::internal(format!("read_memory failed: {e}")))
    }
}

/// `web_fetch` — fetch a URL's main text content.
pub struct WebFetchHandler {
    config: Config,
}

impl WebFetchHandler {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

#[async_trait]
impl ReadOnlyToolHandler for WebFetchHandler {
    fn descriptor(&self) -> McpToolDescriptor {
        McpToolDescriptor {
            name: "web_fetch".into(),
            description: "Fetch and clean the textual content of a URL.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "max_chars": {"type": "integer", "minimum": 100}
                },
                "required": ["url"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<serde_json::Value, McpError> {
        cortex::tool_handlers::handle_web_fetch(&arguments, &self.config)
            .await
            .map(serde_json::Value::String)
            .map_err(|e| McpError::internal(format!("web_fetch failed: {e}")))
    }
}

/// `web_search` — issue a web search and return ranked results.
pub struct WebSearchHandler {
    config: Config,
}

impl WebSearchHandler {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

#[async_trait]
impl ReadOnlyToolHandler for WebSearchHandler {
    fn descriptor(&self) -> McpToolDescriptor {
        McpToolDescriptor {
            name: "web_search".into(),
            description: "Perform a web search and return ranked results.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                },
                "required": ["query"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<serde_json::Value, McpError> {
        cortex::tool_handlers::handle_web_search(&arguments, &self.config)
            .await
            .map(serde_json::Value::String)
            .map_err(|e| McpError::internal(format!("web_search failed: {e}")))
    }
}

/// `list` — enumerate read-only resources (skills, channels). Read-only
/// over MCP — `agents` is omitted because the multi-agent control surface
/// requires a live `AgentControl` handle that the MCP server doesn't have.
pub struct ListHandler {
    config: Config,
}

impl ListHandler {
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

#[async_trait]
impl ReadOnlyToolHandler for ListHandler {
    fn descriptor(&self) -> McpToolDescriptor {
        McpToolDescriptor {
            name: "list".into(),
            description: "Enumerate available resources. `what` ∈ {skills, channels}.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "what": {"type": "string", "enum": ["skills", "channels"]}
                },
                "required": ["what"]
            }),
        }
    }

    async fn call(&self, arguments: serde_json::Value) -> Result<serde_json::Value, McpError> {
        // Reject `agents` here rather than letting the cortex handler
        // return its "Multi-agent system is not enabled." string —
        // surfacing a JSON-RPC error makes the constraint visible in
        // MCP clients.
        if arguments.get("what").and_then(|v| v.as_str()) == Some("agents") {
            return Err(McpError::invalid_params("`agents` is not exposed over MCP"));
        }
        cortex::tool_handlers::handle_list(&arguments, &self.config, None)
            .map(serde_json::Value::String)
            .map_err(|e| McpError::internal(format!("list failed: {e}")))
    }
}

/// Build the v1 read-only tool set. The order here is the order
/// `tools/list` advertises it; the guard test in `server.rs` enforces
/// exact membership.
pub fn build_read_only_tools(config: Config) -> Vec<Box<dyn ReadOnlyToolHandler>> {
    vec![
        Box::new(MemorySearchHandler::new(config.clone())),
        Box::new(ReadMemoryHandler),
        Box::new(WebFetchHandler::new(config.clone())),
        Box::new(WebSearchHandler::new(config.clone())),
        Box::new(ListHandler::new(config)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_read_only_tools_emits_exactly_the_v1_set() {
        // Real failure mode: a regression that adds a mutating tool to
        // the production registry would be caught both here and by
        // `tools_list_only_exposes_read_only_tools` in `server.rs`.
        // This guard fires earlier (no MCP server boot needed).
        let tools = build_read_only_tools(Config::default());
        let names: Vec<String> = tools.iter().map(|t| t.descriptor().name).collect();
        assert_eq!(
            names,
            vec![
                "memory_search",
                "read_memory",
                "web_fetch",
                "web_search",
                "list"
            ]
        );
        for forbidden in super::super::server::McpServer::MUTATING_TOOLS {
            assert!(
                !names.iter().any(|n| n == forbidden),
                "mutating tool `{forbidden}` must never appear in the MCP tool set"
            );
        }
    }

    #[tokio::test]
    async fn list_handler_rejects_agents_with_invalid_params() {
        // Real failure mode: a regression that surfaced the cortex
        // handler's "Multi-agent system is not enabled." string would
        // mislead MCP clients into thinking the tool exists but the
        // agent feature is just disabled. The MCP layer's contract is
        // stricter — `agents` is not part of the surface.
        let h = ListHandler::new(Config::default());
        let err = h
            .call(serde_json::json!({"what": "agents"}))
            .await
            .expect_err("must error");
        assert_eq!(err.code, -32602, "must be invalid_params (-32602)");
    }
}
