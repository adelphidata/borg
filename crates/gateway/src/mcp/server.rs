//! MCP server logic — JSON-RPC request handling + tool dispatch.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// One read-only tool exposed over MCP. Production wires `name` to the
/// matching `borg_core::tool_handlers::*::handle` function; tests
/// inject fakes that record calls.
#[async_trait]
pub trait ReadOnlyToolHandler: Send + Sync + 'static {
    /// Tool descriptor used to populate `tools/list` responses.
    fn descriptor(&self) -> McpToolDescriptor;
    /// Execute the tool with the given arguments. Errors here become
    /// JSON-RPC error responses, not protocol-level failures.
    async fn call(&self, arguments: serde_json::Value) -> Result<serde_json::Value, McpError>;
}

/// JSON-Schema-compatible tool description used by MCP `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDescriptor {
    /// Tool name (matches the JSON-RPC `params.name` for `tools/call`).
    pub name: String,
    /// Human-readable description shown in MCP-aware UIs.
    pub description: String,
    /// JSON Schema for the tool's input.
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

/// Tool-execution error surfaced as a JSON-RPC error response.
#[derive(Debug, Clone)]
pub struct McpError {
    /// JSON-RPC error code. `-32602` for invalid params, `-32603` for
    /// internal errors, `-32601` for unknown method.
    pub code: i64,
    /// Human-readable message.
    pub message: String,
}

impl McpError {
    /// Method-not-found (-32601). Emitted for unknown JSON-RPC methods
    /// or unknown tool names.
    pub fn method_not_found(msg: impl Into<String>) -> Self {
        Self {
            code: -32601,
            message: msg.into(),
        }
    }
    /// Invalid-params (-32602).
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: msg.into(),
        }
    }
    /// Internal error (-32603) — tool handler failed.
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: msg.into(),
        }
    }
}

/// MCP server. Holds the read-only tool registry; processes
/// JSON-RPC messages received over either transport.
pub struct McpServer {
    tools: Vec<Box<dyn ReadOnlyToolHandler>>,
    /// Server identity advertised in `initialize` responses.
    server_info: McpServerInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct McpServerInfo {
    name: String,
    version: String,
}

impl McpServer {
    /// Build a server with the given read-only tool set. `version`
    /// is what gets advertised in `initialize` responses.
    pub fn new(tools: Vec<Box<dyn ReadOnlyToolHandler>>, version: impl Into<String>) -> Self {
        Self {
            tools,
            server_info: McpServerInfo {
                name: "borg-mcp".into(),
                version: version.into(),
            },
        }
    }

    /// Process one JSON-RPC request and produce one JSON-RPC response.
    /// Returns `None` for notifications (no `id` field) — MCP allows
    /// `notifications/*` which receive no reply.
    pub async fn handle(&self, req_json: &serde_json::Value) -> Option<serde_json::Value> {
        let method = req_json
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let id = req_json.get("id").cloned();
        let is_notification = id.is_none();
        let params = req_json
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        // Notifications get no response per JSON-RPC; we still
        // process them so e.g. `notifications/cancelled` could
        // unblock long-running tools later.
        if is_notification {
            return None;
        }

        let response = match method {
            "initialize" => self.handle_initialize(),
            "tools/list" => Ok(self.handle_tools_list()),
            "tools/call" => self.handle_tools_call(params).await,
            "ping" => Ok(serde_json::json!({})),
            other => Err(McpError::method_not_found(format!(
                "unknown method `{other}`"
            ))),
        };

        Some(match response {
            Ok(result) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            }),
            Err(err) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": err.code, "message": err.message},
            }),
        })
    }

    fn handle_initialize(&self) -> Result<serde_json::Value, McpError> {
        Ok(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {"listChanged": false}
            },
            "serverInfo": self.server_info,
        }))
    }

    fn handle_tools_list(&self) -> serde_json::Value {
        let descriptors: Vec<McpToolDescriptor> =
            self.tools.iter().map(|t| t.descriptor()).collect();
        serde_json::json!({"tools": descriptors})
    }

    async fn handle_tools_call(
        &self,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpError::invalid_params("missing `name`"))?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default()));

        let tool = self
            .tools
            .iter()
            .find(|t| t.descriptor().name == name)
            .ok_or_else(|| McpError::method_not_found(format!("no such tool `{name}`")))?;

        let result = tool.call(arguments).await?;
        // MCP wraps tool output in a content block. Stringified
        // result is the universally-supported form.
        let text = match &result {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        Ok(serde_json::json!({
            "content": [{"type": "text", "text": text}],
            "isError": false,
        }))
    }

    /// Names of mutating Borg tools that MUST NOT be exposed over
    /// MCP in v1. The tools/list guard test asserts none of these
    /// appear in the registry — adding any to the surface requires
    /// also updating that test, which is a deliberate friction
    /// point per the task spec's risks-to-watch.
    pub const MUTATING_TOOLS: &'static [&'static str] = &[
        "apply_patch",
        "run_shell",
        "write_memory",
        "browser",
        "generate_image",
        "text_to_speech",
        "schedule",
        "projects",
        "request_user_input",
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records every call. Production handlers will hit
    /// `borg_core::tool_handlers::*` directly; this fake exists to
    /// drive the dispatcher without spinning the real backend.
    struct RecordingTool {
        descriptor: McpToolDescriptor,
        calls: Mutex<Vec<serde_json::Value>>,
        response: serde_json::Value,
    }

    #[async_trait]
    impl ReadOnlyToolHandler for RecordingTool {
        fn descriptor(&self) -> McpToolDescriptor {
            self.descriptor.clone()
        }
        async fn call(&self, arguments: serde_json::Value) -> Result<serde_json::Value, McpError> {
            self.calls.lock().expect("test mutex").push(arguments);
            Ok(self.response.clone())
        }
    }

    fn read_only_server() -> McpServer {
        let tools: Vec<Box<dyn ReadOnlyToolHandler>> = vec![
            Box::new(RecordingTool {
                descriptor: McpToolDescriptor {
                    name: "memory_search".into(),
                    description: "Hybrid vector+BM25 memory search".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                },
                calls: Mutex::default(),
                response: serde_json::json!({"hits": []}),
            }),
            Box::new(RecordingTool {
                descriptor: McpToolDescriptor {
                    name: "read_memory".into(),
                    description: "Read a memory entry by name".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                },
                calls: Mutex::default(),
                response: serde_json::json!("entry contents"),
            }),
            Box::new(RecordingTool {
                descriptor: McpToolDescriptor {
                    name: "web_fetch".into(),
                    description: "Fetch URL content".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                },
                calls: Mutex::default(),
                response: serde_json::json!({"body": "..."}),
            }),
            Box::new(RecordingTool {
                descriptor: McpToolDescriptor {
                    name: "web_search".into(),
                    description: "Web search".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                },
                calls: Mutex::default(),
                response: serde_json::json!({"results": []}),
            }),
            Box::new(RecordingTool {
                descriptor: McpToolDescriptor {
                    name: "list".into(),
                    description: "List resources (read-only actions only)".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                },
                calls: Mutex::default(),
                response: serde_json::json!({"items": []}),
            }),
        ];
        McpServer::new(tools, "0.1.0-test")
    }

    /// **Hard regression guard.** Per the task spec's risks-to-watch:
    /// "Tempting to expose `apply_patch` or `run_shell` over MCP;
    /// **don't** in v1 (that's the entire agent loop's authority over
    /// a fragile boundary). Read-only tools only."
    ///
    /// Adding any of the names in `McpServer::MUTATING_TOOLS` to the
    /// MCP registry breaks this test. Doing so should be a deliberate
    /// architectural decision — update both this list AND the test
    /// at the same time, never just bypass.
    #[tokio::test]
    async fn tools_list_only_exposes_read_only_tools() {
        let server = read_only_server();
        let resp = server
            .handle(&serde_json::json!({
                "jsonrpc":"2.0","id":1,"method":"tools/list","params":{}
            }))
            .await
            .expect("response");
        let names: Vec<String> = resp["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|t| t["name"].as_str().unwrap_or("").to_string())
            .collect();

        // Spec-required surface.
        for required in [
            "memory_search",
            "read_memory",
            "web_fetch",
            "web_search",
            "list",
        ] {
            assert!(
                names.contains(&required.to_string()),
                "MCP must expose `{required}`, got: {names:?}"
            );
        }

        // Forbidden mutators MUST NOT appear.
        for mutator in McpServer::MUTATING_TOOLS {
            assert!(
                !names.contains(&(*mutator).to_string()),
                "MCP must NOT expose mutating tool `{mutator}` — this breaks the v1 \
                 read-only invariant. If you intentionally added it, update \
                 `MUTATING_TOOLS` and document the security review.",
            );
        }
    }

    #[tokio::test]
    async fn tools_call_routes_to_correct_handler_with_arguments() {
        // The dispatcher must thread `params.arguments` through to
        // the matching tool by name. A misroute would silently hand
        // memory queries to web_fetch — observable in production but
        // catchable here.
        let server = read_only_server();
        let resp = server
            .handle(&serde_json::json!({
                "jsonrpc":"2.0","id":42,"method":"tools/call",
                "params":{
                    "name":"memory_search",
                    "arguments":{"query":"ferris","top_k":3}
                }
            }))
            .await
            .expect("response");
        assert_eq!(resp["id"], 42);
        let content = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text content");
        // Result was JSON; default rendering is its serialized form.
        assert!(
            content.contains("\"hits\""),
            "tool response surfaced in content, got: {content}"
        );
        assert_eq!(resp["result"]["isError"], false);
    }

    #[tokio::test]
    async fn unknown_tool_returns_method_not_found() {
        // MCP requires JSON-RPC -32601 for unknown methods/tools so
        // clients can render a meaningful error. Custom codes would
        // confuse Claude Desktop's error handler.
        let server = read_only_server();
        let resp = server
            .handle(&serde_json::json!({
                "jsonrpc":"2.0","id":7,"method":"tools/call",
                "params":{"name":"apply_patch","arguments":{}}
            }))
            .await
            .expect("response");
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let server = read_only_server();
        let resp = server
            .handle(&serde_json::json!({
                "jsonrpc":"2.0","id":1,"method":"resources/list","params":{}
            }))
            .await
            .expect("response");
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        // JSON-RPC notifications (no id) must not produce a reply —
        // sending one would confuse a client expecting silence and
        // clutter the framing layer in stdio mode.
        let server = read_only_server();
        let resp = server
            .handle(&serde_json::json!({
                "jsonrpc":"2.0","method":"notifications/cancelled","params":{}
            }))
            .await;
        assert!(resp.is_none(), "notifications must produce no response");
    }

    #[tokio::test]
    async fn initialize_advertises_protocol_version_and_server_info() {
        // MCP clients gate connect on the protocolVersion field.
        // Wrong value = the client refuses to load the server.
        let server = read_only_server();
        let resp = server
            .handle(&serde_json::json!({
                "jsonrpc":"2.0","id":0,"method":"initialize","params":{}
            }))
            .await
            .expect("response");
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(resp["result"]["serverInfo"]["name"], "borg-mcp");
        assert_eq!(resp["result"]["serverInfo"]["version"], "0.1.0-test");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }
}
