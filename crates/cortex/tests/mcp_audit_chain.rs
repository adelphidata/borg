#![allow(
    clippy::await_holding_lock,
    clippy::expect_used,
    clippy::redundant_closure_for_method_calls,
    clippy::uninlined_format_args,
    clippy::unwrap_used
)]

//! Real-DB audit-chain E2E for MCP.
//!
//! Single `#[test]` in this binary because `install_global_audit` is a
//! process-singleton (`OnceLock`). Wires up an in-memory libsql DB,
//! installs the global writer, drives one `install_session` → one
//! `try_execute_tool` round-trip through a scripted transport, then
//! reads the `audit_events` rows and asserts the spec: `MCP_SERVER_STARTED`
//! lands with a server name + tool list, `MCP_TOOL_REGISTERED` lands
//! for each good tool, `MCP_SCHEMA_REJECTED` lands for each bad tool
//! with a reason, and `MCP_TOOL_EXECUTED` lands for the dispatched call.
//!
//! Goal: the audit chain is the operator's only post-hoc record that an
//! MCP server interacted with the agent. A regression that dropped any
//! of these rows would render the chain silent on whole classes of
//! activity. The in-memory `rejected_for` mirror in `mcp_integration.rs`
//! covers the *semantics* of rejection — this binary closes the gap by
//! proving the row actually persists.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use cortex::config::{Config, McpServerConfig, TransportSpec};
use cortex::db::{audit_events, install_global_audit, AuditWriter, Database};
use cortex::mcp_clients;
use mcp::manifest::ToolFilter;
use mcp::protocol::{methods, Message};
use mcp::transport::Transport;
use serde_json::{json, Value};
use tokio::sync::Mutex as TokioMutex;

type Responder = Box<dyn FnMut(&Value) -> Value + Send>;

struct ScriptedTransport {
    handlers: Arc<Mutex<HashMap<String, Responder>>>,
    inbound: Arc<Mutex<VecDeque<Message>>>,
    eof: Arc<Mutex<bool>>,
}

impl ScriptedTransport {
    fn new() -> Self {
        Self {
            handlers: Arc::new(Mutex::new(HashMap::new())),
            inbound: Arc::new(Mutex::new(VecDeque::new())),
            eof: Arc::new(Mutex::new(false)),
        }
    }
}

#[async_trait]
impl Transport for ScriptedTransport {
    // Notifications (no `id`) like `notifications/initialized` fall
    // through silently — they're outbound-only from the client side and
    // don't expect a scripted reply.
    async fn send(&mut self, msg: Value) -> Result<(), mcp::errors::McpError> {
        let method = msg
            .get("method")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string());
        let id = msg.get("id").and_then(|i| i.as_u64());
        if let (Some(method), Some(id)) = (method, id) {
            let mut handlers = self.handlers.lock().unwrap();
            if let Some(factory) = handlers.get_mut(&method) {
                let result = factory(&msg);
                self.inbound.lock().unwrap().push_back(Message {
                    jsonrpc: Some("2.0".into()),
                    id: Some(id),
                    method: None,
                    result: Some(result),
                    error: None,
                    params: None,
                });
            }
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<Message>, mcp::errors::McpError> {
        loop {
            if let Some(m) = self.inbound.lock().unwrap().pop_front() {
                return Ok(Some(m));
            }
            if *self.eof.lock().unwrap() {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn close(&mut self) -> Result<(), mcp::errors::McpError> {
        *self.eof.lock().unwrap() = true;
        Ok(())
    }
}

fn enabled_config() -> Config {
    let mut cfg = Config::default();
    cfg.mcp.enabled = true;
    cfg
}

fn server_config() -> McpServerConfig {
    McpServerConfig {
        enabled: true,
        transport: TransportSpec::Stdio {
            command: "/bin/false".into(),
            args: vec![],
            env: HashMap::new(),
        },
        connect_timeout_ms: 5_000,
        call_timeout_ms: 2_000,
        rate_limit_per_minute: 60,
        rate_limit_burst: 10,
        tools: ToolFilter {
            include: vec![],
            exclude: vec![],
            read_only: vec![],
            max: ToolFilter::DEFAULT_MAX,
        },
    }
}

fn well_formed_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "q": {"type": "string"} }
    })
}

fn oversized_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "blob": {
                "type": "string",
                "description": "x".repeat(mcp::registry::MAX_SCHEMA_BYTES + 1)
            }
        }
    })
}

fn ref_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "a": {"$ref": "#/definitions/Foo"} }
    })
}

fn tools_list_value(tools: &[(&str, Value)]) -> Value {
    let arr: Vec<Value> = tools
        .iter()
        .map(|(name, schema)| {
            json!({
                "name": name,
                "description": format!("tool {name}"),
                "inputSchema": schema,
            })
        })
        .collect();
    json!({ "tools": arr })
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "serverInfo": {"name": "test", "version": "0.1"}
    })
}

fn text_call_result(body: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": body}],
        "isError": false,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_audit_chain_records_server_started_registered_rejected_and_executed() {
    // ---- install the process-global audit writer once -----------------
    let db = Database::test_db().await;
    let dbref = Arc::new(TokioMutex::new(db));
    assert!(
        install_global_audit(AuditWriter::new(dbref.clone())),
        "audit writer must install on a fresh test binary — another test \
         in this binary already installed it",
    );
    mcp_clients::testing::reset_all();

    // ---- script a server with two bad + one good tool ----------------
    let transport = ScriptedTransport::new();
    {
        let mut h = transport.handlers.lock().unwrap();
        h.insert(
            methods::INITIALIZE.into(),
            Box::new(|_| initialize_result()),
        );
        let well = well_formed_schema();
        let big = oversized_schema();
        let with_ref = ref_schema();
        h.insert(
            methods::TOOLS_LIST.into(),
            Box::new(move |_| {
                tools_list_value(&[
                    ("good", well.clone()),
                    ("too_big", big.clone()),
                    ("uses_ref", with_ref.clone()),
                ])
            }),
        );
        h.insert(
            methods::TOOLS_CALL.into(),
            Box::new(|_| text_call_result("ok")),
        );
    }

    let cfg = enabled_config();
    let sc = server_config();
    mcp_clients::testing::install_session("audit", &sc, &cfg, Box::new(transport))
        .await
        .expect("install");

    // ---- dispatch a tools/call so MCP_TOOL_EXECUTED lands -------------
    let out = mcp_clients::try_execute_tool("mcp_audit_good", &json!({"q": "hello"}))
        .await
        .expect("dispatch");
    assert!(out.is_some(), "dispatch must return a result");

    // ---- inspect the persisted audit chain ----------------------------
    let rows = dbref
        .lock()
        .await
        .list_audit_events()
        .await
        .expect("list audit");

    let server_started: Vec<_> = rows
        .iter()
        .filter(|r| r.event_type == audit_events::MCP_SERVER_STARTED)
        .collect();
    // `>= 1` rather than `== 1`: a future addition of e.g. a per-`tools/list_changed`
    // emit shouldn't break this assertion. What matters is the row exists
    // and carries the right payload.
    assert!(
        !server_started.is_empty(),
        "at least one MCP_SERVER_STARTED row expected, got types {:?}",
        rows.iter().map(|r| r.event_type.as_str()).collect::<Vec<_>>()
    );
    let payload: Value = serde_json::from_str(&server_started[0].event_json).expect("payload");
    assert_eq!(payload["server"], "audit");
    assert_eq!(payload["transport"], "stdio");
    assert_eq!(payload["registered_count"], 1);
    let registered_arr = payload["registered_tools"]
        .as_array()
        .unwrap_or_else(|| panic!("registered_tools must be an array, payload {payload}"));
    assert_eq!(
        registered_arr.len(),
        1,
        "registered_tools must contain exactly one entry (the good tool), got {registered_arr:?}",
    );
    assert!(
        registered_arr.iter().any(|v| v == "mcp_audit_good"),
        "registered_tools must include the good tool, payload was {payload}",
    );

    let registered: Vec<_> = rows
        .iter()
        .filter(|r| r.event_type == audit_events::MCP_TOOL_REGISTERED)
        .collect();
    assert_eq!(
        registered.len(),
        1,
        "exactly one MCP_TOOL_REGISTERED row (the good tool) expected, got {registered:?}",
    );
    let reg_payload: Value = serde_json::from_str(&registered[0].event_json).expect("payload");
    assert_eq!(reg_payload["tool"], "mcp_audit_good");

    let rejected: Vec<_> = rows
        .iter()
        .filter(|r| r.event_type == audit_events::MCP_SCHEMA_REJECTED)
        .collect();
    assert_eq!(
        rejected.len(),
        2,
        "two MCP_SCHEMA_REJECTED rows expected (too_big + uses_ref), got {rejected:?}",
    );
    let reject_tools: Vec<String> = rejected
        .iter()
        .filter_map(|r| serde_json::from_str::<Value>(&r.event_json).ok())
        .filter_map(|v| v["tool"].as_str().map(str::to_string))
        .collect();
    assert!(reject_tools.contains(&"too_big".to_string()), "{reject_tools:?}");
    assert!(reject_tools.contains(&"uses_ref".to_string()), "{reject_tools:?}");
    for row in &rejected {
        let v: Value = serde_json::from_str(&row.event_json).expect("payload");
        assert!(
            v["reason"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
            "each MCP_SCHEMA_REJECTED row must carry a non-empty reason — payload {v}",
        );
    }

    let executed: Vec<_> = rows
        .iter()
        .filter(|r| r.event_type == audit_events::MCP_TOOL_EXECUTED)
        .collect();
    assert!(
        !executed.is_empty(),
        "at least one MCP_TOOL_EXECUTED row expected after the dispatch, got {executed:?}",
    );
    let exec_payload: Value = serde_json::from_str(&executed[0].event_json).expect("payload");
    assert_eq!(exec_payload["server"], "audit");

    // Hash-chain integrity — the rows we wrote must form a valid chain.
    let outcome = dbref
        .lock()
        .await
        .verify_audit_chain()
        .await
        .expect("verify");
    assert_eq!(
        outcome,
        cortex::db::AuditChainOutcome::Ok,
        "audit chain must remain intact across MCP emits",
    );
}
