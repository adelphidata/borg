//! Tool registry: owns the merged tool catalog across all MCP servers.
//!
//! The registry is what the cortex adapter queries each turn. It's
//! deliberately a *snapshot* type: the supervisor task updates it after
//! handshake / `tools/list` / `tools/list_changed`, and readers (the agent
//! loop) lock-and-clone briefly.
//!
//! Security responsibilities owned here:
//!
//! - Name sanitization (`mcp_<server>_<tool>`) with collision rejection.
//! - Schema gauntlet (size, root-type, `$ref`, property count).
//! - Per-server `tools.max` and global `mcp.max_tools_total` caps.
//! - Plan-mode classification (every tool defaults to mutating; operator
//!   opts in via `tools.read_only`).
//!
//! Supervisor lifecycle (spawn / reconnect / supervise) is intentionally
//! *not* in this module — it lives in `cortex::mcp_clients` so it can use
//! the cortex-side audit writer and `spawn_supervised`.

use crate::errors::SchemaRejectionReason;
use crate::manifest::ToolFilter;
use crate::protocol::McpToolSchema;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Acquire a write lock, logging once if the lock was poisoned and
/// recovering the inner state. A poison event means another thread
/// panicked while holding the lock — we surface that in the trace stream
/// so the operator can correlate it with whatever else broke (rather than
/// silently swallowing it).
fn write_recovering<'a, T>(lock: &'a RwLock<T>, name: &'static str) -> RwLockWriteGuard<'a, T> {
    match lock.write() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::warn!(lock = name, "mcp registry write lock poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

/// Read-lock variant. Same recovery story as [`write_recovering`].
fn read_recovering<'a, T>(lock: &'a RwLock<T>, name: &'static str) -> RwLockReadGuard<'a, T> {
    match lock.read() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::warn!(lock = name, "mcp registry read lock poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

/// Cap on a single tool's serialized schema. Above this, refuse to register.
///
/// Each tool's schema rides in the system prompt every turn — a 32 KB cap
/// keeps a single hostile server from monopolizing the context window.
pub const MAX_SCHEMA_BYTES: usize = 32 * 1024;

/// Cap on property count in a single tool's input schema. Defends against
/// adversarial schemas that try to balloon prompt size.
pub const MAX_PROPERTIES: usize = 64;

/// Global cap on the total number of MCP tools across all servers. Sized as
/// roughly four servers at the per-server [`ToolFilter::DEFAULT_MAX`] of 32;
/// past that, MCP schemas crowd out borg's own tools in the system prompt.
///
/// [`ToolFilter::DEFAULT_MAX`]: crate::manifest::ToolFilter::DEFAULT_MAX
pub const DEFAULT_GLOBAL_TOOL_CAP: usize = 128;

/// One registered MCP tool, ready for the agent loop to dispatch.
#[derive(Debug, Clone)]
pub struct RegisteredTool {
    /// Namespaced name the LLM sees (`mcp_<server>_<tool>`).
    pub full_name: String,
    /// Server this tool belongs to.
    pub server: String,
    /// Original (pre-sanitization) tool name to forward to the server.
    pub original_name: String,
    /// Description forwarded to the LLM.
    pub description: Option<String>,
    /// Validated JSON schema for `inputSchema`.
    pub input_schema: Value,
    /// True when the tool is parallel-safe to fan out with other read-only
    /// calls. Set by the operator via per-server `tools.read_only` config,
    /// OR — additively — by the server itself via
    /// `annotations.readOnlyHint: true`. Use this for batching decisions
    /// only. Plan-mode gating MUST use [`Self::operator_read_only`]
    /// because the hint is untrusted and Plan mode is a hard trust
    /// boundary.
    pub read_only: bool,
    /// True only when the operator explicitly opted this tool in via
    /// per-server `tools.read_only` config. Used as the Plan-mode trust
    /// gate so a malicious server cannot promote its mutating tool into
    /// Plan-mode visibility by self-reporting `readOnlyHint`.
    pub operator_read_only: bool,
}

/// Registry status for `borg mcp status`.
#[derive(Debug, Clone, Default)]
pub struct RegistryStatus {
    /// Per-server status entries.
    pub servers: BTreeMap<String, ServerStatus>,
    /// Total tools currently registered.
    pub total_tools: usize,
}

/// Per-server status.
#[derive(Debug, Clone)]
pub struct ServerStatus {
    /// Health snapshot.
    pub health: ServerHealth,
    /// Tools currently registered from this server.
    pub tool_count: usize,
    /// Reasons we rejected individual tools, for operator visibility.
    pub rejected_tools: Vec<(String, String)>,
    /// Last observed serverInfo line (`name version`).
    pub server_info: Option<String>,
}

/// Coarse health classification surfaced in `borg mcp status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerHealth {
    /// Connected and operating normally.
    Healthy,
    /// Tools are registered from a cached catalog while the live connection is
    /// still being established. Tool *calls* are refused until the session is
    /// live — the catalog is a hint about what will be available, not proof
    /// that it currently is.
    Starting,
    /// Connect failed or session dropped; supervisor is retrying.
    Reconnecting {
        /// Number of consecutive failures so far.
        attempts: u32,
    },
    /// Authentication failed — no further retries until the operator
    /// updates credentials.
    AuthFailed {
        /// Human-readable reason from the server.
        message: String,
    },
    /// Degraded: too many restarts in the window. Won't auto-retry.
    Degraded,
}

/// Shared registry state. Wrap in `Arc` for the cortex adapter; clone is
/// cheap.
#[derive(Clone, Default)]
pub struct McpRegistry {
    inner: Arc<RwLock<RegistryInner>>,
    /// Global cap. `Arc<RwLock>` keeps it adjustable at runtime without
    /// re-creating the registry.
    global_cap: Arc<RwLock<usize>>,
}

#[derive(Default)]
struct RegistryInner {
    /// All registered tools, keyed by namespaced name.
    tools: BTreeMap<String, RegisteredTool>,
    /// Per-server tool names (for collision detection and bulk-removal on
    /// disconnect / refresh).
    by_server: BTreeMap<String, Vec<String>>,
    /// Per-server status snapshots.
    status: BTreeMap<String, ServerStatus>,
}

impl McpRegistry {
    /// New empty registry with the default global tool cap.
    pub fn new() -> Self {
        Self::with_global_cap(DEFAULT_GLOBAL_TOOL_CAP)
    }

    /// New empty registry with a custom global cap.
    pub fn with_global_cap(cap: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(RegistryInner::default())),
            global_cap: Arc::new(RwLock::new(cap)),
        }
    }

    /// Replace the tools registered for `server` with the result of running
    /// `discovered` through the gauntlet. Returns the per-tool outcomes so
    /// the supervisor can audit rejections.
    pub fn replace_server_tools(
        &self,
        server: &str,
        discovered: &[McpToolSchema],
        filter: &ToolFilter,
        builtin_names: &[&str],
    ) -> Vec<ToolRegistrationOutcome> {
        let mut outcomes = Vec::new();
        let global_cap = *read_recovering(&self.global_cap, "registry.global_cap");
        let mut inner = write_recovering(&self.inner, "registry.inner");

        // Drop existing entries for this server first — `tools/list_changed`
        // is a full re-declaration, not a diff.
        if let Some(existing) = inner.by_server.remove(server) {
            for name in existing {
                inner.tools.remove(&name);
            }
        }

        // Track how many we've already accepted for this server in this call.
        let mut accepted_for_server: usize = 0;
        let mut registered_names: Vec<String> = Vec::new();

        for tool in discovered {
            if !filter.admits(&tool.name) {
                outcomes.push(ToolRegistrationOutcome::FilteredOut {
                    original: tool.name.clone(),
                });
                continue;
            }

            if accepted_for_server >= filter.max {
                outcomes.push(ToolRegistrationOutcome::Rejected {
                    original: tool.name.clone(),
                    reason: SchemaRejectionReason::PerServerCapReached,
                });
                continue;
            }

            let current_total: usize = inner.tools.len();
            if current_total + 1 > global_cap {
                outcomes.push(ToolRegistrationOutcome::Rejected {
                    original: tool.name.clone(),
                    reason: SchemaRejectionReason::GlobalCapReached,
                });
                continue;
            }

            let full_name = match sanitize_full_name(server, &tool.name) {
                Some(n) => n,
                None => {
                    outcomes.push(ToolRegistrationOutcome::Rejected {
                        original: tool.name.clone(),
                        reason: SchemaRejectionReason::EmptyName,
                    });
                    continue;
                }
            };

            if builtin_names.iter().any(|b| *b == full_name) || inner.tools.contains_key(&full_name)
            {
                outcomes.push(ToolRegistrationOutcome::Collision {
                    original: tool.name.clone(),
                    full_name: full_name.clone(),
                });
                continue;
            }

            if let Err(reason) = validate_schema(&tool.input_schema) {
                outcomes.push(ToolRegistrationOutcome::Rejected {
                    original: tool.name.clone(),
                    reason,
                });
                continue;
            }

            // Read-only is true when either the operator opted in
            // (`tools.read_only`) OR the server advertised
            // `annotations.readOnlyHint: true`. The latter is untrusted,
            // but it only widens parallel-safe + Plan-mode-visible sets —
            // it never grants additional privileges, so trusting the
            // server's self-report here is acceptable. Operator config
            // remains the canonical opt-in for ambient tools that don't
            // ship the hint.
            let operator_read_only = filter.is_read_only(&tool.name);
            let advertised_read_only = tool
                .annotations
                .as_ref()
                .and_then(|a| a.read_only_hint)
                .unwrap_or(false);
            let read_only = operator_read_only || advertised_read_only;
            let registered = RegisteredTool {
                full_name: full_name.clone(),
                server: server.to_string(),
                original_name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
                read_only,
                operator_read_only,
            };
            inner.tools.insert(full_name.clone(), registered);
            registered_names.push(full_name.clone());
            accepted_for_server += 1;
            outcomes.push(ToolRegistrationOutcome::Registered { full_name });
        }

        inner.by_server.insert(server.to_string(), registered_names);

        let rejected_tools = outcomes
            .iter()
            .filter_map(|o| match o {
                ToolRegistrationOutcome::Rejected { original, reason } => {
                    Some((original.clone(), reason.to_string()))
                }
                ToolRegistrationOutcome::Collision {
                    original,
                    full_name,
                } => Some((original.clone(), format!("collision with {full_name}"))),
                ToolRegistrationOutcome::FilteredOut { .. }
                | ToolRegistrationOutcome::Registered { .. } => None,
            })
            .collect();

        let preserved_server_info = inner.status.get(server).and_then(|s| s.server_info.clone());
        inner.status.insert(
            server.to_string(),
            ServerStatus {
                health: ServerHealth::Healthy,
                tool_count: accepted_for_server,
                rejected_tools,
                server_info: preserved_server_info,
            },
        );

        outcomes
    }

    /// Remove all tools belonging to `server` (e.g. on disconnect /
    /// disable). Idempotent.
    pub fn drop_server(&self, server: &str) {
        let mut inner = write_recovering(&self.inner, "registry.inner");
        if let Some(names) = inner.by_server.remove(server) {
            for n in names {
                inner.tools.remove(&n);
            }
        }
        inner.status.remove(server);
    }

    /// Mark a server's health (without touching its tools — used when the
    /// session is reconnecting and we want to keep the catalog warm).
    pub fn set_health(&self, server: &str, health: ServerHealth) {
        let mut inner = write_recovering(&self.inner, "registry.inner");
        let entry = inner
            .status
            .entry(server.to_string())
            .or_insert_with(|| ServerStatus {
                health: health.clone(),
                tool_count: 0,
                rejected_tools: Vec::new(),
                server_info: None,
            });
        entry.health = health;
        // When a server is no longer Healthy, drop its tools — keeping stale
        // schemas in the LLM tool list would invite confusing failures.
        //
        // `Starting` is the exception: its tools come from a cached catalog and
        // are deliberately advertised before the session is live. Dropping them
        // here would defeat the cache entirely — the caller registers the
        // catalog and *then* marks the server starting.
        let drop_tools = !matches!(entry.health, ServerHealth::Healthy | ServerHealth::Starting);
        if drop_tools {
            if let Some(names) = inner.by_server.remove(server) {
                for n in names {
                    inner.tools.remove(&n);
                }
            }
            if let Some(s) = inner.status.get_mut(server) {
                s.tool_count = 0;
            }
        }
    }

    /// A single server's health, or `None` if no session has touched it.
    /// Cheaper than [`Self::status`] for the dispatch path, which asks about
    /// one server and would otherwise clone every server's status.
    pub fn health(&self, server: &str) -> Option<ServerHealth> {
        let inner = read_recovering(&self.inner, "registry.inner");
        inner.status.get(server).map(|s| s.health.clone())
    }

    /// Record server-info from a successful handshake.
    pub fn set_server_info(&self, server: &str, info: Option<String>) {
        let mut inner = write_recovering(&self.inner, "registry.inner");
        let entry = inner
            .status
            .entry(server.to_string())
            .or_insert_with(|| ServerStatus {
                health: ServerHealth::Healthy,
                tool_count: 0,
                rejected_tools: Vec::new(),
                server_info: None,
            });
        entry.server_info = info;
    }

    /// Snapshot of all currently-registered tools.
    pub fn tools(&self) -> Vec<RegisteredTool> {
        let inner = read_recovering(&self.inner, "registry.inner");
        inner.tools.values().cloned().collect()
    }

    /// Look up a tool by its namespaced name.
    pub fn lookup(&self, full_name: &str) -> Option<RegisteredTool> {
        let inner = read_recovering(&self.inner, "registry.inner");
        inner.tools.get(full_name).cloned()
    }

    /// Status snapshot for `borg mcp status`.
    pub fn status(&self) -> RegistryStatus {
        let inner = read_recovering(&self.inner, "registry.inner");
        RegistryStatus {
            servers: inner.status.clone(),
            total_tools: inner.tools.len(),
        }
    }

    /// Adjust the global cap at runtime.
    pub fn set_global_cap(&self, cap: usize) {
        *write_recovering(&self.global_cap, "registry.global_cap") = cap;
    }
}

/// Per-tool outcome of [`McpRegistry::replace_server_tools`].
#[derive(Debug, Clone)]
pub enum ToolRegistrationOutcome {
    /// Tool was registered.
    Registered {
        /// Namespaced name.
        full_name: String,
    },
    /// Tool was filtered out by `include`/`exclude` config.
    FilteredOut {
        /// Original (pre-sanitization) tool name.
        original: String,
    },
    /// Tool was rejected by the schema/cap gauntlet.
    Rejected {
        /// Original tool name.
        original: String,
        /// Why it was rejected.
        reason: SchemaRejectionReason,
    },
    /// Tool's namespaced name collided with a builtin or another MCP tool.
    Collision {
        /// Original tool name.
        original: String,
        /// Namespaced name that collided.
        full_name: String,
    },
}

/// Build the namespaced tool name from a server name + MCP tool name.
///
/// Sanitization rules:
/// - Lowercase.
/// - Any non-`[a-z0-9_]` byte → `_`.
/// - Trailing/leading underscores in the *components* are preserved (we
///   only collapse runs of underscores within the joined name to keep
///   names readable but unambiguously decodable).
///
/// Returns `None` if the resulting name has empty server *or* tool
/// component.
pub fn sanitize_full_name(server: &str, tool: &str) -> Option<String> {
    let s = sanitize_component(server);
    let t = sanitize_component(tool);
    if s.is_empty() || t.is_empty() {
        return None;
    }
    Some(format!("mcp_{s}_{t}"))
}

fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

/// Run an MCP tool's `inputSchema` through the validation gauntlet.
///
/// Returns `Err` with a [`SchemaRejectionReason`] if the schema is rejected;
/// `Ok(())` if it passes.
pub fn validate_schema(schema: &Value) -> std::result::Result<(), SchemaRejectionReason> {
    // `serde_json::to_string(&Value)` is infallible on the current `Value`
    // type — re-serialization can't fail. If that ever changes (e.g. a swap
    // to `RawValue`), treat the failure as "too large to size-check safely"
    // and refuse rather than waving the schema through with bytes=0.
    let serialized = match serde_json::to_string(schema) {
        Ok(s) => s,
        Err(_) => {
            return Err(SchemaRejectionReason::TooLarge { bytes: usize::MAX });
        }
    };
    if serialized.len() > MAX_SCHEMA_BYTES {
        return Err(SchemaRejectionReason::TooLarge {
            bytes: serialized.len(),
        });
    }
    if contains_ref(schema) {
        return Err(SchemaRejectionReason::ContainsRef);
    }
    let obj = match schema {
        Value::Object(o) => o,
        _ => return Err(SchemaRejectionReason::NotObjectRoot),
    };
    match obj.get("type") {
        Some(Value::String(t)) if t == "object" => {}
        _ => return Err(SchemaRejectionReason::NotObjectRoot),
    }
    if let Some(Value::Object(props)) = obj.get("properties") {
        if props.len() > MAX_PROPERTIES {
            return Err(SchemaRejectionReason::TooManyProperties { count: props.len() });
        }
    }
    Ok(())
}

fn contains_ref(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            if map.contains_key("$ref") {
                return true;
            }
            map.values().any(contains_ref)
        }
        Value::Array(items) => items.iter().any(contains_ref),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema_with_props(n: usize) -> Value {
        let mut props = serde_json::Map::new();
        for i in 0..n {
            props.insert(format!("p{i}"), json!({"type": "string"}));
        }
        json!({"type": "object", "properties": props})
    }

    #[test]
    fn sanitize_namespaces_with_mcp_prefix() {
        assert_eq!(
            sanitize_full_name("github", "create_issue").as_deref(),
            Some("mcp_github_create_issue")
        );
        // Mixed case + punctuation gets normalized but information preserved.
        assert_eq!(
            sanitize_full_name("My-API", "list.items.v2").as_deref(),
            Some("mcp_my_api_list_items_v2")
        );
    }

    #[test]
    fn sanitize_rejects_empty_components() {
        assert!(sanitize_full_name("", "tool").is_none());
        assert!(sanitize_full_name("server", "").is_none());
        assert!(sanitize_full_name("---", "tool").is_none());
    }

    #[test]
    fn schema_gauntlet_rejects_non_object_root() {
        // Spec: every MCP tool input must be a JSON object at the root.
        // Anything else is rejected.
        assert!(matches!(
            validate_schema(&json!("not-an-object")).unwrap_err(),
            SchemaRejectionReason::NotObjectRoot
        ));
        assert!(matches!(
            validate_schema(&json!({"type": "string"})).unwrap_err(),
            SchemaRejectionReason::NotObjectRoot
        ));
    }

    #[test]
    fn schema_gauntlet_rejects_ref_at_any_depth() {
        // Spec: we never resolve `$ref` (external-ref attack surface).
        let nested = json!({
            "type": "object",
            "properties": {
                "inner": { "$ref": "#/definitions/foo" }
            }
        });
        assert!(matches!(
            validate_schema(&nested).unwrap_err(),
            SchemaRejectionReason::ContainsRef
        ));
    }

    #[test]
    fn schema_gauntlet_rejects_too_many_properties() {
        let s = schema_with_props(MAX_PROPERTIES + 1);
        let err = validate_schema(&s).unwrap_err();
        assert!(matches!(
            err,
            SchemaRejectionReason::TooManyProperties { count } if count == MAX_PROPERTIES + 1
        ));
    }

    #[test]
    fn schema_gauntlet_rejects_oversize() {
        // A schema with one giant description string crosses the byte cap.
        let s = json!({
            "type": "object",
            "description": "x".repeat(MAX_SCHEMA_BYTES + 1)
        });
        let err = validate_schema(&s).unwrap_err();
        assert!(matches!(err, SchemaRejectionReason::TooLarge { .. }));
    }

    /// Regression for the silent-swallow bug: `validate_schema` used to
    /// call `serde_json::to_string(...).unwrap_or_default()`, which
    /// would collapse any serialization failure to an empty string and
    /// then *pass* the size check on `bytes == 0`. `serde_json::Value`
    /// can't actually fail to re-serialize, so the bug was latent — but
    /// the gauntlet must still report the correct size for a *real*
    /// oversize schema rather than masking it. This test pins that the
    /// reported `bytes` reflects the actual serialized length, not the
    /// fallback default. A regression that re-introduces the swallow
    /// would either return `bytes == 0` (gauntlet bypassed) or skip the
    /// reject entirely.
    #[test]
    fn schema_gauntlet_reports_real_byte_count_on_oversize() {
        let payload = "x".repeat(MAX_SCHEMA_BYTES + 100);
        let s = json!({ "type": "object", "description": payload });
        let err = validate_schema(&s).unwrap_err();
        match err {
            SchemaRejectionReason::TooLarge { bytes } => {
                assert!(
                    bytes > MAX_SCHEMA_BYTES,
                    "reported {bytes} bytes must exceed cap {MAX_SCHEMA_BYTES}; \
                     a bytes==0 result would mean the size check was bypassed"
                );
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn schema_gauntlet_admits_normal_tool() {
        let s = json!({
            "type": "object",
            "properties": { "query": { "type": "string" } },
            "required": ["query"]
        });
        validate_schema(&s).unwrap();
    }

    /// `Starting` means "tools known from cache, session not live yet". Health
    /// changes drop a server's tools precisely so stale schemas can't linger —
    /// but applying that to `Starting` would erase the catalog the cache just
    /// registered, silently defeating it. Every other non-healthy state must
    /// still drop.
    #[test]
    fn set_health_keeps_tools_only_for_healthy_and_starting() {
        let tools = vec![McpToolSchema {
            name: "read".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            annotations: None,
        }];

        for (health, expect_kept) in [
            (ServerHealth::Healthy, true),
            (ServerHealth::Starting, true),
            (ServerHealth::Reconnecting { attempts: 1 }, false),
            (
                ServerHealth::AuthFailed {
                    message: "401".into(),
                },
                false,
            ),
            (ServerHealth::Degraded, false),
        ] {
            let reg = McpRegistry::new();
            reg.replace_server_tools("files", &tools, &ToolFilter::default(), &[]);
            assert!(!reg.tools().is_empty(), "precondition: tool registered");

            reg.set_health("files", health.clone());

            assert_eq!(
                !reg.tools().is_empty(),
                expect_kept,
                "{health:?}: tools kept should be {expect_kept}"
            );
            assert_eq!(reg.health("files"), Some(health));
        }
    }

    #[test]
    fn replace_server_tools_namespaces_and_filters() {
        let reg = McpRegistry::new();
        let tools = vec![
            McpToolSchema {
                name: "search".into(),
                description: Some("search".into()),
                input_schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
                annotations: None,
            },
            McpToolSchema {
                name: "delete".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                annotations: None,
            },
        ];
        let filter = ToolFilter {
            exclude: vec!["delete".into()],
            read_only: vec!["search".into()],
            ..Default::default()
        };
        let outcomes = reg.replace_server_tools("github", &tools, &filter, &[]);

        // search: registered as mcp_github_search, read_only=true.
        let names: Vec<_> = reg.tools().into_iter().map(|t| t.full_name).collect();
        assert_eq!(names, vec!["mcp_github_search"]);
        let entry = reg.lookup("mcp_github_search").unwrap();
        assert!(entry.read_only, "operator-marked read-only must propagate");
        // delete: filtered out.
        assert!(outcomes
            .iter()
            .any(|o| matches!(o, ToolRegistrationOutcome::FilteredOut { original } if original == "delete")));
    }

    #[test]
    fn collision_with_builtin_is_rejected() {
        let reg = McpRegistry::new();
        let tools = vec![McpToolSchema {
            name: "fetch".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            annotations: None,
        }];
        let outcomes =
            reg.replace_server_tools("web", &tools, &ToolFilter::default(), &["mcp_web_fetch"]);
        assert!(outcomes
            .iter()
            .any(|o| matches!(o, ToolRegistrationOutcome::Collision { .. })));
        assert!(reg.tools().is_empty());
    }

    #[test]
    fn replace_full_drops_previous() {
        // tools/list_changed semantics: a fresh list fully replaces the
        // previous declaration. Stale tools must not linger in the catalog.
        let reg = McpRegistry::new();
        reg.replace_server_tools(
            "x",
            &[McpToolSchema {
                name: "a".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                annotations: None,
            }],
            &ToolFilter::default(),
            &[],
        );
        reg.replace_server_tools(
            "x",
            &[McpToolSchema {
                name: "b".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                annotations: None,
            }],
            &ToolFilter::default(),
            &[],
        );
        let names: Vec<_> = reg.tools().into_iter().map(|t| t.full_name).collect();
        assert_eq!(names, vec!["mcp_x_b"]);
    }

    #[test]
    fn per_server_cap_enforced() {
        let reg = McpRegistry::new();
        let tools: Vec<_> = (0..5)
            .map(|i| McpToolSchema {
                name: format!("t{i}"),
                description: None,
                input_schema: json!({"type": "object"}),
                annotations: None,
            })
            .collect();
        let filter = ToolFilter {
            max: 3,
            ..Default::default()
        };
        let outcomes = reg.replace_server_tools("s", &tools, &filter, &[]);
        let accepted = outcomes
            .iter()
            .filter(|o| matches!(o, ToolRegistrationOutcome::Registered { .. }))
            .count();
        let cap_rejections = outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o,
                    ToolRegistrationOutcome::Rejected {
                        reason: SchemaRejectionReason::PerServerCapReached,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(accepted, 3);
        assert_eq!(cap_rejections, 2);
    }

    #[test]
    fn unhealthy_server_drops_its_tools() {
        // Stale schemas in the LLM tool list lead to confusing failures.
        // Spec: any non-Healthy state purges the tools.
        let reg = McpRegistry::new();
        reg.replace_server_tools(
            "x",
            &[McpToolSchema {
                name: "t".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                annotations: None,
            }],
            &ToolFilter::default(),
            &[],
        );
        assert_eq!(reg.tools().len(), 1);
        reg.set_health("x", ServerHealth::Reconnecting { attempts: 1 });
        assert!(reg.tools().is_empty());
    }

    #[test]
    fn server_advertised_read_only_hint_widens_parallel_only_not_plan_mode() {
        // Spec: a server's `readOnlyHint: true` flips `read_only` (the
        // batching-safe flag) but MUST NOT flip `operator_read_only` (the
        // Plan-mode gate). The hint is untrusted; Plan mode requires
        // explicit operator opt-in via per-server `tools.read_only`.
        use crate::protocol::ToolAnnotations;
        let reg = McpRegistry::new();
        let tools = vec![McpToolSchema {
            name: "search".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            annotations: Some(ToolAnnotations {
                read_only_hint: Some(true),
            }),
        }];
        reg.replace_server_tools("srv", &tools, &ToolFilter::default(), &[]);
        let entry = reg.lookup("mcp_srv_search").unwrap();
        assert!(
            entry.read_only,
            "server-advertised readOnlyHint must widen parallel-safe set"
        );
        assert!(
            !entry.operator_read_only,
            "untrusted hint must NOT widen the Plan-mode trust boundary"
        );
    }

    #[test]
    fn operator_opt_in_sets_both_read_only_flags() {
        // Inverse spec: explicit operator config flips both flags so the
        // tool is parallel-safe AND Plan-mode-visible.
        let reg = McpRegistry::new();
        let tools = vec![McpToolSchema {
            name: "search".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            annotations: None,
        }];
        let filter = ToolFilter {
            read_only: vec!["search".into()],
            ..Default::default()
        };
        reg.replace_server_tools("srv", &tools, &filter, &[]);
        let entry = reg.lookup("mcp_srv_search").unwrap();
        assert!(entry.read_only, "operator opt-in must set read_only");
        assert!(
            entry.operator_read_only,
            "operator opt-in must set operator_read_only"
        );
    }

    #[test]
    fn missing_read_only_hint_does_not_flip_read_only() {
        // Negative guard: an unannotated tool stays mutating-by-default.
        // Without this, the planner would batch unknown MCP calls and a
        // misbehaving server's mutating tool would run concurrently with
        // peers.
        let reg = McpRegistry::new();
        let tools = vec![McpToolSchema {
            name: "delete".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            annotations: None,
        }];
        reg.replace_server_tools("srv", &tools, &ToolFilter::default(), &[]);
        let entry = reg.lookup("mcp_srv_delete").unwrap();
        assert!(
            !entry.read_only,
            "tool without readOnlyHint must remain mutating-by-default"
        );
    }
}
