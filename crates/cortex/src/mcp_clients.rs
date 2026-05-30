//! Runtime bridge between cortex and the `mcp` crate.
//!
//! Mirrors the shape of [`crate::wasm_plugins`] line-for-line: an opt-in
//! subsystem that contributes [`crate::types::ToolDefinition`]s to the
//! agent loop and handles dispatch when the LLM picks one of them. When
//! `mcp.enabled` is `false` (the default), this module is a no-op — borg
//! behaves exactly as it did before MCP existed.
//!
//! ## Trust posture
//!
//! Every server is an *untrusted* counterparty. Tool output passes through
//! three pipelines before reaching the LLM:
//!
//! 1. [`crate::security::secrets::redact_with_audit`] — strip leaked secrets.
//! 2. [`crate::security::sanitize::scan_for_injection`] — flag prompt-injection
//!    payloads.
//! 3. Wrap in `<mcp_tool_result server="…" tool="…" trust="untrusted">…</…>`
//!    so downstream prompt assembly treats the payload as data, not
//!    instructions. The wrapper string is asserted by a source-grep guard
//!    test below — without it, MCP outputs would silently bypass the
//!    untrusted-input pipeline.
//!
//! ## Lifecycle (Slice 2)
//!
//! [`initialize`] opens a per-server [`mcp::session::SessionActor`] on a
//! plain `tokio::spawn`. Slice 3 will lift this into `borgd`'s
//! `spawn_supervised` wrapper without changing the cortex-facing API.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::config::{Config, McpServerConfig};
use crate::types::ToolDefinition;
use mcp::manifest::ServerManifest;
use mcp::registry::{McpRegistry, RegisteredTool, ToolRegistrationOutcome};
use mcp::session::SessionActor;
use mcp::transport::Transport;

/// Process-global registry. `OnceLock` — the *cell* is fixed at first
/// initialize, but [`McpRegistry`]'s internals are already
/// `Arc<RwLock<…>>`, so we can re-run `replace_server_tools` and
/// `drop_server` against the same instance across reconnects and live
/// `tools/list_changed` refreshes.
static REGISTRY: OnceLock<McpRegistry> = OnceLock::new();

/// Per-server mpsc channel into the session actor. Calls go through here;
/// the actor enforces single-writer ordering on the underlying transport.
static SESSION_TX: OnceLock<Mutex<HashMap<String, mpsc::Sender<SessionCmd>>>> = OnceLock::new();

/// Per-server token buckets for rate limiting. Bucket keys are server
/// names (not tool names) so the operator-facing
/// `rate_limit_per_minute` knob caps each server as a whole.
static BUCKETS: OnceLock<Mutex<HashMap<String, TokenBucket>>> = OnceLock::new();

/// Per-server limits captured at session-install time. Re-reading
/// `Config::load_from_db` on every call would be racy and forces tests to
/// own the DB; pinning the limits here makes dispatch hermetic and Slice 3
/// can hot-reload by reseating the map.
static SERVER_LIMITS: OnceLock<Mutex<HashMap<String, ServerLimits>>> = OnceLock::new();

/// Global per-response byte cap. Read on every dispatch and written by
/// [`initialize`] / [`reload_from_config`]. Atomic because dispatch is on
/// the hot path and contending on a Mutex for a single `usize` would hurt.
static MAX_RESPONSE_BYTES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(1024 * 1024);

/// Floor on `mcp.max_response_bytes`. A value of 0 in config is treated as
/// "use the default" rather than "no cap" — the latter would defeat the
/// whole point of the truncation guard. Mirrors the rate-limit
/// fail-closed-on-zero convention at [`try_consume_rate_token`].
const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Resolve the operator-configured `max_response_bytes` to the value the
/// dispatch path should actually use. Centralises the
/// "0 means default, not unbounded" rule so every `MAX_RESPONSE_BYTES.store`
/// call site applies it uniformly.
fn resolve_max_response_bytes(configured: usize) -> usize {
    if configured == 0 {
        tracing::warn!(
            "mcp.max_response_bytes=0 is invalid (would disable the cap); \
             falling back to default {DEFAULT_MAX_RESPONSE_BYTES}",
        );
        DEFAULT_MAX_RESPONSE_BYTES
    } else {
        configured
    }
}

#[derive(Clone, Copy)]
struct ServerLimits {
    rate_limit_per_minute: u32,
    rate_limit_burst: u32,
    call_timeout: Duration,
}

/// Commands the session actor accepts.
enum SessionCmd {
    Call {
        original_name: String,
        args: Value,
        reply: oneshot::Sender<Result<String>>,
    },
}

/// Built-in tool names the registry must avoid colliding with. Hard-coded
/// here to avoid pulling `wasm_plugins`'s private helper across module
/// boundaries; the list mirrors the one in
/// `crate::wasm_plugins::builtin_tool_names`.
fn builtin_tool_names() -> Vec<&'static str> {
    use crate::tool::names;
    vec![
        names::WRITE_MEMORY,
        names::READ_MEMORY,
        names::MEMORY_SEARCH,
        names::LIST,
        names::APPLY_PATCH,
        names::RUN_SHELL,
        names::READ_FILE,
        names::LIST_DIR,
        names::WEB_FETCH,
        names::WEB_SEARCH,
        names::SCHEDULE,
        names::MANAGE_TASKS,
        names::MANAGE_CRON,
        names::BROWSER,
        names::GENERATE_IMAGE,
        names::TEXT_TO_SPEECH,
        names::REQUEST_USER_INPUT,
        names::APPLY_SKILL_PATCH,
        names::CREATE_CHANNEL,
        names::LIST_SKILLS,
        names::LIST_CHANNELS,
        names::LIST_AGENTS,
        names::SPAWN_AGENT,
        names::SEND_TO_AGENT,
        names::WAIT_FOR_AGENT,
        names::CLOSE_AGENT,
        names::MANAGE_ROLES,
        names::GMAIL,
        names::GOOGLE_CALENDAR,
        names::NOTION,
        names::LINEAR,
    ]
}

use crate::rate_limit::TokenBucket;

fn buckets() -> &'static Mutex<HashMap<String, TokenBucket>> {
    BUCKETS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn try_consume_rate_token(server: &str, per_minute: u32, burst: u32) -> Option<Duration> {
    let mut map = match buckets().lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    let bucket = map
        .entry(server.to_string())
        .or_insert_with(|| TokenBucket::new(f64::from(burst)));
    bucket.try_consume(per_minute, burst)
}

fn session_map() -> &'static Mutex<HashMap<String, mpsc::Sender<SessionCmd>>> {
    SESSION_TX.get_or_init(|| Mutex::new(HashMap::new()))
}

fn limits_map() -> &'static Mutex<HashMap<String, ServerLimits>> {
    SERVER_LIMITS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_limits(server: &str, cfg: &McpServerConfig) {
    let limits = ServerLimits {
        rate_limit_per_minute: cfg.rate_limit_per_minute,
        rate_limit_burst: cfg.rate_limit_burst,
        call_timeout: Duration::from_millis(cfg.call_timeout_ms),
    };
    let mut map = match limits_map().lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    map.insert(server.to_string(), limits);
    // Drop any pre-existing bucket so a burst/per-minute change reseeds
    // the bucket on the next call rather than carrying stale capacity.
    if let Ok(mut b) = buckets().lock() {
        b.remove(server);
    }
}

/// Process-global snapshot of `mcp.disabled_servers`, refreshed at
/// `initialize` and consulted on every read/dispatch. Defense in depth:
/// the init loop already skips disabled servers, but if a session was
/// installed before a name was added to the list, this filter keeps the
/// LLM from seeing or calling its tools until the daemon tears the
/// session down. Empty by default → no filtering.
static DISABLED_SERVERS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn disabled_set() -> &'static Mutex<Vec<String>> {
    DISABLED_SERVERS.get_or_init(|| Mutex::new(Vec::new()))
}

fn is_server_disabled(server: &str) -> bool {
    let guard = match disabled_set().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    guard.iter().any(|s| s == server)
}

fn set_disabled_servers(list: &[String]) {
    let mut guard = match disabled_set().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    *guard = list.to_vec();
}

fn lookup_limits(server: &str) -> Option<ServerLimits> {
    let map = match limits_map().lock() {
        Ok(m) => m,
        Err(p) => p.into_inner(),
    };
    map.get(server).copied()
}

/// Return MCP tool definitions for the agent loop. Empty when MCP is
/// disabled or the registry has not been populated. Tools belonging to
/// servers in `mcp.disabled_servers` are filtered live — the init loop
/// already skips those servers, but a session installed before the name
/// was added must not keep leaking its tools to the LLM.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    let Some(reg) = REGISTRY.get() else {
        return Vec::new();
    };
    reg.tools()
        .into_iter()
        .filter(|t: &RegisteredTool| !is_server_disabled(&t.server))
        .map(|t: RegisteredTool| {
            ToolDefinition::new(
                t.full_name,
                t.description.unwrap_or_default(),
                t.input_schema,
            )
        })
        .collect()
}

/// One server's runtime view, joining the operator-side config (is it
/// enabled? what transport?) with the live registry (is it healthy? how
/// many tools landed?). Returned by [`server_status_snapshot`] for
/// operator-facing UIs (`/mcp`, `/plugins`, `borg mcp list`).
#[derive(Debug, Clone)]
pub struct McpServerSnapshot {
    /// Operator-chosen server name.
    pub name: String,
    /// Transport kind label: `"stdio"`, `"http"`, or `"sse"`.
    pub transport: &'static str,
    /// Operator-configured `enabled` flag (per-server).
    pub config_enabled: bool,
    /// True if the operator has the server in `mcp.disabled_servers` —
    /// hides tools live without daemon restart.
    pub live_disabled: bool,
    /// Live registry health, if any. `None` means no session has been
    /// installed yet (e.g. mcp.enabled=false, or daemon never started).
    pub health: Option<mcp::registry::ServerHealth>,
    /// Tools currently registered from this server (post-gauntlet).
    pub tool_count: usize,
    /// Number of tools that hit the schema gauntlet and were rejected.
    pub rejected_count: usize,
}

impl McpServerSnapshot {
    /// Single source of truth for the lifecycle label rendered in both
    /// the `/mcp` popup and the `MCP SERVERS` section of `/plugins`.
    /// Centralising avoids the two views drifting apart — a divergence
    /// would surface as "the popup says connected but the unified view
    /// says reconnecting" with no clue which is right.
    ///
    /// Precedence: `live_disabled` (operator's runtime kill switch) >
    /// `!config_enabled` (per-server opt-out) > live registry health.
    pub fn state_label(&self) -> &'static str {
        use mcp::registry::ServerHealth;
        if self.live_disabled {
            return "live-disabled";
        }
        if !self.config_enabled {
            return "disabled";
        }
        match self.health.as_ref() {
            Some(ServerHealth::Healthy) => "connected",
            Some(ServerHealth::Reconnecting { .. }) => "reconnecting",
            Some(ServerHealth::AuthFailed { .. }) => "auth-failed",
            Some(ServerHealth::Degraded) => "degraded",
            None => "not-started",
        }
    }
}

/// Snapshot of every configured MCP server, joined with the live
/// registry. Empty when MCP is globally disabled (`mcp.enabled=false`)
/// AND no servers are configured. Cheap to call: clones a small
/// `BTreeMap` of statuses.
///
/// Used by the TUI `/mcp` and `/plugins` popups for read-only operator
/// visibility; the dispatch path uses [`tool_definitions`] and
/// [`is_server_disabled`] directly.
pub fn server_status_snapshot(config: &Config) -> Vec<McpServerSnapshot> {
    let status = REGISTRY.get().map(McpRegistry::status);
    let mut out: Vec<McpServerSnapshot> = config
        .mcp
        .servers
        .iter()
        .map(|(name, srv)| {
            let transport = match &srv.transport {
                crate::config::TransportSpec::Stdio { .. } => "stdio",
                crate::config::TransportSpec::Http { .. } => "http",
                crate::config::TransportSpec::Sse { .. } => "sse",
            };
            let live = status.as_ref().and_then(|s| s.servers.get(name));
            McpServerSnapshot {
                name: name.clone(),
                transport,
                config_enabled: srv.enabled,
                live_disabled: config.mcp.disabled_servers.iter().any(|d| d == name),
                health: live.map(|s| s.health.clone()),
                tool_count: live.map(|s| s.tool_count).unwrap_or(0),
                rejected_count: live.map(|s| s.rejected_tools.len()).unwrap_or(0),
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// True iff `name` belongs to an MCP tool that is safe to fan out in
/// parallel with other read-only calls. Combines operator opt-in
/// (`tools.read_only`) with server-advertised `annotations.readOnlyHint`.
/// Default-on-miss is `false`, so an uninitialized registry or unknown
/// tool is treated as mutating.
///
/// **Do not use this for Plan-mode gating** — the hint is untrusted. Use
/// [`is_operator_read_only_mcp_tool`] for trust-boundary decisions.
pub fn is_read_only_mcp_tool(name: &str) -> bool {
    REGISTRY
        .get()
        .and_then(|reg| reg.lookup(name))
        .map(|t| t.read_only)
        .unwrap_or(false)
}

/// True iff the operator explicitly opted `name` into per-server
/// `tools.read_only`. Untrusted server annotations are NOT considered
/// here, so this is the correct gate for Plan mode and any other path
/// that treats "read-only" as a trust assertion rather than a batching
/// hint. Default-on-miss is `false`.
pub fn is_operator_read_only_mcp_tool(name: &str) -> bool {
    REGISTRY
        .get()
        .and_then(|reg| reg.lookup(name))
        .map(|t| t.operator_read_only)
        .unwrap_or(false)
}

/// Attempt to execute an MCP tool. Returns `Ok(None)` if `name` is not an
/// MCP tool name (so the agent loop's dispatcher can fall through to the
/// next handler); `Ok(Some(text))` on success (text is already wrapped in
/// the trust-boundary tag); `Err` for structured failures (rate limit,
/// transport, schema mismatch). Refusals are never silent.
pub async fn try_execute_tool(name: &str, args: &Value) -> Result<Option<String>> {
    if !name.starts_with("mcp_") {
        return Ok(None);
    }
    let Some(reg) = REGISTRY.get() else {
        return Ok(None);
    };
    let Some(tool) = reg.lookup(name) else {
        return Ok(None);
    };

    // Refuse dispatch to a server the operator has live-disabled, even if
    // the registry still holds its tools because a session was installed
    // before the name landed in `mcp.disabled_servers`. Pair with the
    // `tool_definitions` filter so the LLM neither sees nor reaches it.
    if is_server_disabled(&tool.server) {
        return Err(anyhow!(
            "MCP server '{server}' is operator-disabled",
            server = tool.server
        ));
    }

    // Rate-limit gate. Limits are pinned at session-install time so
    // dispatch stays hermetic — see `lookup_limits`. If the entry is
    // missing the server isn't initialized; refuse rather than silently
    // applying defaults.
    let limits = match lookup_limits(&tool.server) {
        Some(l) => l,
        None => {
            return Err(anyhow!(
                "MCP server '{server}' has no recorded limits; refusing call",
                server = tool.server
            ));
        }
    };
    let per_minute = limits.rate_limit_per_minute;
    let burst = limits.rate_limit_burst;
    let call_timeout = limits.call_timeout;
    if let Some(retry_after) = try_consume_rate_token(&tool.server, per_minute, burst) {
        let retry_secs = retry_after.as_secs_f64().max(0.1);
        crate::db::try_emit_global(
            crate::db::audit_events::MCP_RATE_LIMITED,
            &tool.server,
            &serde_json::json!({
                "server": &tool.server,
                "tool": &tool.full_name,
                "retry_after_secs": retry_secs,
                "limit_per_minute": per_minute,
                "burst": burst,
            }),
        )
        .await;
        tracing::info!(
            target: "borg.mcp",
            event = "rate_limited",
            server = %tool.server,
            tool = %tool.full_name,
            retry_after_secs = retry_secs,
            "mcp tool call rate-limited",
        );
        return Err(anyhow!(
            "MCP server '{server}' is rate-limited; retry in {retry_secs:.1}s",
            server = tool.server
        ));
    }

    let tx = {
        let map = match session_map().lock() {
            Ok(m) => m,
            Err(p) => p.into_inner(),
        };
        match map.get(&tool.server) {
            Some(s) => s.clone(),
            None => {
                return Err(anyhow!(
                    "MCP server '{server}' has no live session",
                    server = tool.server
                ))
            }
        }
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = SessionCmd::Call {
        original_name: tool.original_name.clone(),
        args: args.clone(),
        reply: reply_tx,
    };
    if tx.send(cmd).await.is_err() {
        return Err(anyhow!(
            "MCP server '{server}' session task is gone",
            server = tool.server
        ));
    }

    let start = Instant::now();
    // Outer watchdog so a wedged actor cannot starve the agent loop
    // forever. The session enforces the inner per-call timeout; this
    // covers actor-side deadlock.
    let outer_deadline = call_timeout + Duration::from_secs(5);
    let raw_text = match tokio::time::timeout(outer_deadline, reply_rx).await {
        Ok(Ok(Ok(text))) => text,
        Ok(Ok(Err(e))) => {
            let duration_ms = start.elapsed().as_millis() as u64;
            crate::db::try_emit_global(
                crate::db::audit_events::MCP_TOOL_EXECUTED,
                &tool.server,
                &serde_json::json!({
                    "server": &tool.server,
                    "tool": &tool.full_name,
                    "success": false,
                    "duration_ms": duration_ms,
                    "error": e.to_string(),
                }),
            )
            .await;
            // OTEL-style structured event — `borg.mcp.executes_total{outcome="error"}`
            // plus the error message for correlation with logs.
            tracing::info!(
                target: "borg.mcp",
                event = "execute",
                server = %tool.server,
                tool = %tool.full_name,
                outcome = "error",
                duration_ms,
                error = %e,
                "mcp tool execution failed",
            );
            return Err(e.context(format!("MCP tool '{}' failed", tool.full_name)));
        }
        Ok(Err(_canceled)) => {
            return Err(anyhow!(
                "MCP server '{server}' reply channel dropped",
                server = tool.server
            ));
        }
        Err(_elapsed) => {
            return Err(anyhow!(
                "MCP server '{server}' did not reply within {ms}ms",
                server = tool.server,
                ms = outer_deadline.as_millis()
            ));
        }
    };
    let duration_ms = start.elapsed().as_millis() as u64;

    // Response size cap. Hostile/buggy servers cannot dump an unbounded
    // blob into the LLM context: we truncate before redaction + wrapping
    // and surface the truncation as an audit row so operators can see why
    // an output looks clipped. The cap is read once per call from the
    // global atomic — see `MAX_RESPONSE_BYTES` (set via
    // `resolve_max_response_bytes`, which floors a misconfigured 0 to the
    // default rather than disabling the guard).
    let cap = MAX_RESPONSE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    let raw_text = if raw_text.len() > cap {
        let original_bytes = raw_text.len();
        // UTF-8-safe truncation: walk back to the previous char boundary
        // so we never split a multi-byte sequence and emit invalid UTF-8.
        let mut cut = cap;
        while cut > 0 && !raw_text.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut t = raw_text;
        t.truncate(cut);
        t.push_str("\n…[truncated]");
        crate::db::try_emit_global(
            crate::db::audit_events::MCP_RESPONSE_TRUNCATED,
            &tool.server,
            &serde_json::json!({
                "server": &tool.server,
                "tool": &tool.full_name,
                "original_bytes": original_bytes,
                "truncated_to_bytes": cut,
                "cap_bytes": cap,
            }),
        )
        .await;
        tracing::warn!(
            server = %tool.server,
            tool = %tool.full_name,
            original_bytes,
            cap,
            "MCP response truncated by per-call cap",
        );
        t
    } else {
        raw_text
    };

    // Untrusted-input pipeline. Order matters: redact first so the
    // injection scan and the wrapper both see scrubbed text.
    let redacted = crate::security::secrets::redact_with_audit(&raw_text);
    let redaction_hits = redacted.hits.len();
    if redaction_hits > 0 {
        tracing::warn!(
            server = %tool.server,
            tool = %tool.full_name,
            hits = redaction_hits,
            "MCP tool output contained leaked secrets — redacted before LLM",
        );
    }

    let threat = crate::security::sanitize::scan_for_injection(&redacted.text);
    let (payload, threat_label) = match threat {
        crate::security::sanitize::ThreatLevel::Clean => (redacted.text, "clean"),
        crate::security::sanitize::ThreatLevel::Flagged { score, patterns } => {
            tracing::warn!(
                server = %tool.server,
                tool = %tool.full_name,
                score, patterns = ?patterns,
                "MCP tool output flagged for potential prompt injection",
            );
            (
                crate::security::sanitize::wrap_with_injection_warning(
                    &tool.full_name,
                    &redacted.text,
                ),
                "flagged",
            )
        }
        crate::security::sanitize::ThreatLevel::HighRisk { score, patterns } => {
            tracing::warn!(
                server = %tool.server,
                tool = %tool.full_name,
                score, patterns = ?patterns,
                "MCP tool output classified high-risk for prompt injection",
            );
            (
                crate::security::sanitize::wrap_with_injection_warning(
                    &tool.full_name,
                    &redacted.text,
                ),
                "high_risk",
            )
        }
    };

    // Escape any boundary-closing tags inside the payload so a hostile
    // server cannot smuggle a `</mcp_tool_result>` (or `</untrusted_content>`,
    // covered by the same regex) to break out of the trust envelope and
    // inject sibling tags the LLM would read as system instructions.
    let safe_payload = crate::security::sanitize::sanitize_xml_boundaries(&payload);

    // The trust-boundary wrapper. The literal `<mcp_tool_result` string
    // is asserted by `mcp_tool_result_wrapper_is_present_in_source` below
    // — without it, MCP outputs would silently bypass the untrusted-input
    // pipeline.
    let server_attr = crate::security::sanitize::escape_xml_attr(&tool.server);
    let tool_attr = crate::security::sanitize::escape_xml_attr(&tool.full_name);
    let wrapped = format!(
        "<mcp_tool_result server=\"{server_attr}\" tool=\"{tool_attr}\" trust=\"untrusted\">\n{safe_payload}\n</mcp_tool_result>"
    );

    crate::db::try_emit_global(
        crate::db::audit_events::MCP_TOOL_EXECUTED,
        &tool.server,
        &serde_json::json!({
            "server": &tool.server,
            "tool": &tool.full_name,
            "success": true,
            "duration_ms": duration_ms,
            "threat_level": threat_label,
            "redaction_hits": redaction_hits,
        }),
    )
    .await;

    // OTEL-style structured event. The borgd tracing-opentelemetry layer
    // (if installed) projects this into the `borg.mcp.executes_total` and
    // `borg.mcp.execute_duration_ms` instruments documented in
    // `docs/mcp.md`. Off-daemon contexts (CLI, tests) get the same data via
    // plain log output.
    tracing::info!(
        target: "borg.mcp",
        event = "execute",
        server = %tool.server,
        tool = %tool.full_name,
        outcome = "success",
        duration_ms,
        threat_level = threat_label,
        redaction_hits,
        "mcp tool executed",
    );

    Ok(Some(wrapped))
}

/// Initialize MCP sessions. Returns `Ok(())` immediately if
/// `mcp.enabled` is false. Per-server connect failures are logged and
/// *do not* fail the overall initialize — borg always boots, even when
/// every MCP server is down.
pub async fn initialize(config: &Config) -> Result<()> {
    if !config.mcp.enabled {
        return Ok(());
    }
    let reg = REGISTRY.get_or_init(|| McpRegistry::with_global_cap(config.mcp.max_tools_total));
    reg.set_global_cap(config.mcp.max_tools_total);
    set_disabled_servers(&config.mcp.disabled_servers);
    MAX_RESPONSE_BYTES.store(
        resolve_max_response_bytes(config.mcp.max_response_bytes),
        std::sync::atomic::Ordering::Relaxed,
    );

    let builtin = builtin_tool_names();
    let builtin_refs: Vec<&str> = builtin.to_vec();

    for (server_name, server_cfg) in &config.mcp.servers {
        if !server_cfg.enabled || config.mcp.disabled_servers.contains(server_name) {
            tracing::info!(server = %server_name, "MCP server disabled — skipping");
            continue;
        }
        if let Err(e) = start_server(server_name, server_cfg, config, reg, &builtin_refs).await {
            tracing::warn!(
                server = %server_name,
                error = %e,
                "MCP server failed to start — continuing without it",
            );
            // Surface unreachable / mis-credentialled servers in the audit
            // chain so operators don't have to grep logs to see why their
            // configured servers aren't contributing tools.
            crate::db::try_emit_global(
                crate::db::audit_events::MCP_SERVER_FAILED,
                server_name,
                &serde_json::json!({
                    "server": server_name,
                    "phase": "initialize",
                    "reason": format!("start_server failed: {e}"),
                }),
            )
            .await;
        }
    }
    Ok(())
}

/// Result of a [`reload_from_config`] pass. Designed for the operator-facing
/// summary in the TUI `/reload mcp` command — added/removed name lists drive
/// the diff line, `failed` keeps partial-success visible, and `total_tools`
/// confirms the LLM-visible catalog size after the swap.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReloadReport {
    /// Servers that were not running before this reload and are now connected.
    pub added: Vec<String>,
    /// Servers that were running before this reload and have been torn down
    /// (either removed from config or flipped to disabled).
    pub removed: Vec<String>,
    /// Servers that were already running and remain. Limits are reseated
    /// in-place so rate-limit knob changes take effect without a teardown.
    pub unchanged: usize,
    /// Per-server start failures: `(server_name, error_message)`. Reload is
    /// best-effort per-server — other servers still come up.
    pub failed: Vec<(String, String)>,
    /// Total tool count visible to the LLM after the reload (post-disabled
    /// filtering). Matches the count that `tool_definitions()` would return.
    pub total_tools: usize,
}

/// Diff DB-resident MCP server config against the running session map and
/// apply the delta in place. Surfaces a [`ReloadReport`] the operator can
/// see in the TUI.
///
/// Mid-flight tool calls hold their own `mpsc::Sender` clone obtained at
/// dispatch time, so tearing a server down via this function does not
/// cancel calls already in flight — they finish against the old session
/// and the actor exits when the call replies.
pub async fn reload_from_config(config: &Config) -> Result<ReloadReport> {
    // If MCP is globally disabled, the desired set is empty — tear everything
    // down. The registry stays initialized so a subsequent reload that flips
    // `mcp.enabled` back to true does not need a daemon restart.
    let desired: HashSet<String> = if config.mcp.enabled {
        config
            .mcp
            .servers
            .iter()
            .filter(|(name, srv)| srv.enabled && !config.mcp.disabled_servers.contains(*name))
            .map(|(name, _)| name.clone())
            .collect()
    } else {
        HashSet::new()
    };

    let current: HashSet<String> = {
        let map = match session_map().lock() {
            Ok(m) => m,
            Err(p) => p.into_inner(),
        };
        map.keys().cloned().collect()
    };

    let mut to_remove: Vec<String> = current.difference(&desired).cloned().collect();
    let mut to_add: Vec<String> = desired.difference(&current).cloned().collect();
    let unchanged: Vec<String> = desired.intersection(&current).cloned().collect();
    to_remove.sort();
    to_add.sort();

    let reg = REGISTRY.get_or_init(|| McpRegistry::with_global_cap(config.mcp.max_tools_total));
    reg.set_global_cap(config.mcp.max_tools_total);
    set_disabled_servers(&config.mcp.disabled_servers);
    MAX_RESPONSE_BYTES.store(
        resolve_max_response_bytes(config.mcp.max_response_bytes),
        std::sync::atomic::Ordering::Relaxed,
    );

    for name in &to_remove {
        // Dropping the sender closes the actor's receiver — the task exits
        // and the transport's Drop closes the underlying connection.
        if let Ok(mut map) = session_map().lock() {
            map.remove(name);
        }
        reg.drop_server(name);
        if let Ok(mut b) = buckets().lock() {
            b.remove(name);
        }
        if let Ok(mut l) = limits_map().lock() {
            l.remove(name);
        }
        tracing::info!(server = %name, "MCP server torn down via reload");
    }

    // Reseat limits for unchanged servers so rate-limit / timeout knob
    // changes land without a teardown-and-respawn.
    for name in &unchanged {
        if let Some(cfg) = config.mcp.servers.get(name) {
            record_limits(name, cfg);
        }
    }

    let builtin = builtin_tool_names();
    let builtin_refs: Vec<&str> = builtin.to_vec();
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut added: Vec<String> = Vec::new();

    for name in to_add {
        let Some(cfg) = config.mcp.servers.get(&name) else {
            // Race: server was removed from config between the diff and now.
            continue;
        };
        match start_server(&name, cfg, config, reg, &builtin_refs).await {
            Ok(()) => {
                tracing::info!(server = %name, "MCP server started via reload");
                added.push(name);
            }
            Err(e) => {
                let err_msg = e.to_string();
                tracing::warn!(server = %name, error = %err_msg, "MCP reload: server failed to start");
                crate::db::try_emit_global(
                    crate::db::audit_events::MCP_SERVER_FAILED,
                    &name,
                    &serde_json::json!({
                        "server": name,
                        "phase": "reload",
                        "reason": format!("reload start_server failed: {err_msg}"),
                    }),
                )
                .await;
                failures.push((name, err_msg));
            }
        }
    }

    let total_tools = REGISTRY
        .get()
        .map(|r| {
            r.tools()
                .into_iter()
                .filter(|t| !is_server_disabled(&t.server))
                .count()
        })
        .unwrap_or(0);

    Ok(ReloadReport {
        added,
        removed: to_remove,
        unchanged: unchanged.len(),
        failed: failures,
        total_tools,
    })
}

async fn start_server(
    server_name: &str,
    server_cfg: &McpServerConfig,
    config: &Config,
    reg: &McpRegistry,
    builtin: &[&str],
) -> Result<()> {
    let manifest = server_cfg.resolve_to_manifest(server_name, config)?;
    let policy = mcp::transport::http::HttpPolicy {
        allow_plaintext_http: config.mcp.allow_plaintext_http,
        allow_local_network: config.mcp.allow_local_network,
    };
    let transport = match open_transport(&manifest, policy).await {
        Ok(t) => t,
        Err(e) => {
            // OTEL-style transport_errors_total breakdown: kind labels the
            // class of failure (DNS, TLS, IO, refused). The exact string
            // comes from the underlying McpError Display impl, so the
            // metrics layer can aggregate by the leading word.
            let transport_label = match &manifest.transport {
                mcp::manifest::Transport::Stdio { .. } => "stdio",
                mcp::manifest::Transport::Http { .. } => "streamable-http",
                mcp::manifest::Transport::Sse { .. } => "sse",
            };
            tracing::warn!(
                target: "borg.mcp",
                event = "transport_error",
                server = %server_name,
                transport = transport_label,
                kind = "open",
                error = %e,
                "mcp transport open failed",
            );
            return Err(e);
        }
    };
    let mut session = SessionActor::new(&manifest, transport);

    tokio::time::timeout(manifest.connect_timeout, session.handshake())
        .await
        .map_err(|_| anyhow!("handshake timed out for server '{server_name}'"))?
        .map_err(|e| anyhow!("handshake failed for server '{server_name}': {e}"))?;

    let discovered = session
        .list_tools()
        .await
        .map_err(|e| anyhow!("tools/list failed for server '{server_name}': {e}"))?;

    tracing::info!(
        target: "borg.mcp",
        event = "tools_listed",
        server = %server_name,
        count = discovered.tools.len(),
        "mcp tools/list completed",
    );

    let outcomes =
        reg.replace_server_tools(server_name, &discovered.tools, &manifest.tools, builtin);
    audit_registrations(server_name, &outcomes).await;

    record_limits(server_name, server_cfg);

    // Operator-visible "this server is live" signal — paired with
    // `MCP_SERVER_FAILED` for terminal failures and with per-tool
    // `MCP_TOOL_REGISTERED` rows below in `audit_registrations`.
    let registered_tools: Vec<&str> = outcomes
        .iter()
        .filter_map(|o| match o {
            ToolRegistrationOutcome::Registered { full_name } => Some(full_name.as_str()),
            _ => None,
        })
        .collect();
    crate::db::try_emit_global(
        crate::db::audit_events::MCP_SERVER_STARTED,
        server_name,
        &serde_json::json!({
            "server": server_name,
            "transport": match &manifest.transport {
                mcp::manifest::Transport::Stdio { .. } => "stdio",
                mcp::manifest::Transport::Http { .. } => "streamable-http",
                mcp::manifest::Transport::Sse { .. } => "sse",
            },
            "registered_tools": registered_tools,
            "registered_count": registered_tools.len(),
        }),
    )
    .await;

    let (tx, rx) = mpsc::channel::<SessionCmd>(32);
    {
        let mut map = match session_map().lock() {
            Ok(m) => m,
            Err(p) => p.into_inner(),
        };
        map.insert(server_name.to_string(), tx);
    }

    let registry_clone: McpRegistry = reg.clone();
    let filter = manifest.tools.clone();
    let server_owned = server_name.to_string();
    let builtin_owned: Vec<String> = builtin.iter().map(ToString::to_string).collect();
    // TODO: adopt `borgd::supervisor::spawn_supervised` so panics and
    // disconnects retry with backoff and surface to the daemon's
    // degraded-state machinery.
    tokio::spawn(async move {
        run_session_task(
            session,
            server_owned,
            rx,
            registry_clone,
            filter,
            builtin_owned,
        )
        .await;
    });

    Ok(())
}

async fn open_transport(
    manifest: &ServerManifest,
    policy: mcp::transport::http::HttpPolicy,
) -> Result<Box<dyn Transport>> {
    match &manifest.transport {
        mcp::manifest::Transport::Stdio { command, args, env } => {
            let cfg = mcp::transport::stdio::StdioConfig {
                server_name: manifest.name.clone(),
                command: command.clone(),
                args: args.clone(),
                env: env.clone(),
            };
            let stderr_sink: mcp::transport::stdio::StderrSink =
                Arc::new(|srv: &str, line: &str| {
                    tracing::debug!(server = %srv, line = %line, "mcp stderr");
                });
            let t = mcp::transport::stdio::spawn(cfg, stderr_sink).await?;
            Ok(Box::new(t))
        }
        mcp::manifest::Transport::Http { url, headers, auth } => {
            let mut headers = headers.clone();
            if let mcp::manifest::HttpAuth::Bearer { token } = auth {
                headers.insert("Authorization".to_string(), format!("Bearer {token}"));
            }
            let cfg = mcp::transport::http::HttpConfig {
                server_name: manifest.name.clone(),
                url: url.clone(),
                headers,
                timeout: manifest.call_timeout,
                policy,
            };
            let t = mcp::transport::http::open(cfg).await?;
            Ok(Box::new(t))
        }
        mcp::manifest::Transport::Sse { url, headers, auth } => {
            let mut headers = headers.clone();
            if let mcp::manifest::HttpAuth::Bearer { token } = auth {
                headers.insert("Authorization".to_string(), format!("Bearer {token}"));
            }
            let cfg = mcp::transport::sse::SseConfig {
                server_name: manifest.name.clone(),
                url: url.clone(),
                headers,
                timeout: manifest.call_timeout,
                policy,
            };
            let t = mcp::transport::sse::open(cfg, manifest.connect_timeout).await?;
            Ok(Box::new(t))
        }
    }
}

async fn audit_registrations(server_name: &str, outcomes: &[ToolRegistrationOutcome]) {
    for o in outcomes {
        match o {
            ToolRegistrationOutcome::Registered { full_name } => {
                crate::db::try_emit_global(
                    crate::db::audit_events::MCP_TOOL_REGISTERED,
                    server_name,
                    &serde_json::json!({
                        "server": server_name,
                        "tool": full_name,
                    }),
                )
                .await;
            }
            ToolRegistrationOutcome::Rejected { original, reason } => {
                crate::db::try_emit_global(
                    crate::db::audit_events::MCP_SCHEMA_REJECTED,
                    server_name,
                    &serde_json::json!({
                        "server": server_name,
                        "tool": original,
                        "reason": reason.to_string(),
                    }),
                )
                .await;
                tracing::warn!(
                    server = %server_name,
                    tool = %original,
                    reason = %reason,
                    "MCP tool rejected by schema gauntlet",
                );
            }
            ToolRegistrationOutcome::Collision {
                original,
                full_name,
            } => {
                crate::db::try_emit_global(
                    crate::db::audit_events::MCP_SCHEMA_REJECTED,
                    server_name,
                    &serde_json::json!({
                        "server": server_name,
                        "tool": original,
                        "reason": format!("collision with {full_name}"),
                    }),
                )
                .await;
                tracing::warn!(
                    server = %server_name,
                    tool = %original,
                    full_name = %full_name,
                    "MCP tool name collision — not registered",
                );
            }
            ToolRegistrationOutcome::FilteredOut { .. } => {}
        }
    }
}

async fn run_session_task(
    mut session: SessionActor,
    server_name: String,
    mut rx: mpsc::Receiver<SessionCmd>,
    registry: McpRegistry,
    filter: mcp::manifest::ToolFilter,
    builtin: Vec<String>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            SessionCmd::Call {
                original_name,
                args,
                reply,
            } => {
                let raw_result = session.call_tool(&original_name, args).await;
                // Classify transport-level failures so the session tears down
                // cleanly rather than looping with a wedged transport. The
                // streamable-HTTP transport surfaces 404-on-stale-session-id
                // as `TransportClosed`; without a teardown here the next
                // dispatch would POST without the session id, the server
                // would reject every subsequent `tools/call`, and the LLM
                // would see opaque "no live session" errors until the daemon
                // restarted. Marking the session dead drops it from
                // `SESSION_TX` so future dispatches fail fast and the
                // operator sees one clear error per call.
                let transport_dead =
                    matches!(&raw_result, Err(mcp::errors::McpError::TransportClosed(_)));
                let result = raw_result
                    .map(|(text, _raw)| text)
                    .map_err(|e| anyhow!("{e}"));
                // Best-effort send — caller may have dropped its receiver
                // (e.g. outer-watchdog fired). Per CLAUDE.md, log on the
                // drop path so it isn't silent.
                if let Err(_dropped) = reply.send(result) {
                    tracing::warn!(
                        server = %server_name,
                        tool = %original_name,
                        "MCP reply channel closed before send — caller likely timed out",
                    );
                }

                // Drain refused server-initiated requests (sampling, roots,
                // …). The protocol layer already replied with -32601; we
                // emit one audit row per refusal so an operator can SEE that
                // a server tried to drive the borg LLM. Silence here would
                // hide a determined hostile peer.
                for (method, _id) in session.drain_refused_server_requests() {
                    crate::db::try_emit_global(
                        crate::db::audit_events::MCP_SAMPLING_REFUSED,
                        &server_name,
                        &serde_json::json!({
                            "server": &server_name,
                            "method": method,
                        }),
                    )
                    .await;
                }

                // Drain notifications that arrived during the call. A
                // `tools/list_changed` triggers a re-list so newly
                // advertised tools become callable without a reconnect.
                let pending = session.drain_notifications();
                let mut needs_relist = false;
                for n in pending {
                    if n.method.as_deref() == Some(mcp::protocol::methods::TOOLS_LIST_CHANGED) {
                        needs_relist = true;
                    }
                }
                if needs_relist {
                    match session.list_tools().await {
                        Ok(discovered) => {
                            let builtin_refs: Vec<&str> =
                                builtin.iter().map(String::as_str).collect();
                            let outcomes = registry.replace_server_tools(
                                &server_name,
                                &discovered.tools,
                                &filter,
                                &builtin_refs,
                            );
                            audit_registrations(&server_name, &outcomes).await;
                            tracing::debug!(
                                server = %server_name,
                                count = discovered.tools.len(),
                                "MCP server tool list refreshed after list_changed",
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                server = %server_name,
                                error = %e,
                                "MCP tools/list refresh failed after list_changed",
                            );
                        }
                    }
                }

                // Transport-level failure (e.g. streamable-HTTP 404 on a
                // stale session id). Tear down: remove our sender from the
                // global map and drop the session. Future dispatches will
                // fail fast with "no live session" — an operator-readable
                // error — instead of looping with a wedged transport.
                // Re-init requires the operator to `borg settings set` or
                // restart the daemon; auto-reconnect lives in a follow-up.
                if transport_dead {
                    tracing::warn!(
                        server = %server_name,
                        "MCP transport closed; tearing down session (re-init requires daemon restart)",
                    );
                    crate::db::try_emit_global(
                        crate::db::audit_events::MCP_SERVER_FAILED,
                        &server_name,
                        &serde_json::json!({
                            "server": &server_name,
                            "phase": "runtime",
                            "reason": "transport closed mid-session",
                        }),
                    )
                    .await;
                    if let Ok(mut map) = session_map().lock() {
                        map.remove(&server_name);
                    }
                    registry.drop_server(&server_name);
                    break;
                }
            }
        }
    }
    // Receiver closed (clean shutdown) or transport_dead break above. The
    // session's Drop closes the transport.
    tracing::debug!(server = %server_name, "MCP session task exiting");
}

#[doc(hidden)]
pub mod testing {
    //! Test-only helpers. Production code goes through `initialize()`;
    //! tests need to inject a [`SessionActor`] built around a scripted
    //! transport without spawning real subprocesses or making real HTTP
    //! requests. Always compiled (rather than `#[cfg(test)]`) so the
    //! integration-test crate at `tests/mcp_integration.rs` — which is
    //! built against cortex as if external — can use these helpers.

    use super::*;

    /// Clear every piece of process-global MCP state between tests. The
    /// `OnceLock`s themselves can't be reset, but their *interiors* can:
    /// drop every server's tools from the registry, drop every mpsc
    /// sender so the actor tasks exit, and empty the rate-limit buckets.
    pub fn reset_all() {
        if let Some(reg) = REGISTRY.get() {
            let servers: Vec<String> = reg.status().servers.keys().cloned().collect();
            for s in servers {
                reg.drop_server(&s);
            }
        }
        if let Some(m) = SESSION_TX.get() {
            if let Ok(mut g) = m.lock() {
                g.clear();
            }
        }
        if let Some(b) = BUCKETS.get() {
            if let Ok(mut g) = b.lock() {
                g.clear();
            }
        }
        if let Some(l) = SERVER_LIMITS.get() {
            if let Ok(mut g) = l.lock() {
                g.clear();
            }
        }
        if let Some(d) = DISABLED_SERVERS.get() {
            if let Ok(mut g) = d.lock() {
                g.clear();
            }
        }
    }

    /// Install a session whose transport is the caller's choice (typically
    /// a `ScriptedTransport` for hermetic tests). Runs the same handshake
    /// → list_tools → register flow as `initialize`, then spawns the same
    /// session task.
    pub async fn install_session(
        server_name: &str,
        server_cfg: &McpServerConfig,
        cfg: &Config,
        transport: Box<dyn Transport>,
    ) -> Result<()> {
        let reg = REGISTRY.get_or_init(|| McpRegistry::with_global_cap(cfg.mcp.max_tools_total));
        reg.set_global_cap(cfg.mcp.max_tools_total);
        set_disabled_servers(&cfg.mcp.disabled_servers);
        MAX_RESPONSE_BYTES.store(
            resolve_max_response_bytes(cfg.mcp.max_response_bytes),
            std::sync::atomic::Ordering::Relaxed,
        );

        let manifest = server_cfg.resolve_to_manifest(server_name, cfg)?;
        let mut session = SessionActor::new(&manifest, transport);
        tokio::time::timeout(manifest.connect_timeout, session.handshake())
            .await
            .map_err(|_| anyhow!("handshake timed out for server '{server_name}'"))?
            .map_err(|e| anyhow!("handshake failed for server '{server_name}': {e}"))?;
        let discovered = session
            .list_tools()
            .await
            .map_err(|e| anyhow!("tools/list failed for server '{server_name}': {e}"))?;

        tracing::info!(
            target: "borg.mcp",
            event = "tools_listed",
            server = %server_name,
            count = discovered.tools.len(),
            "mcp tools/list completed",
        );

        let builtin = builtin_tool_names();
        let builtin_refs: Vec<&str> = builtin.to_vec();
        let outcomes = reg.replace_server_tools(
            server_name,
            &discovered.tools,
            &manifest.tools,
            &builtin_refs,
        );
        audit_registrations(server_name, &outcomes).await;
        record_limits(server_name, server_cfg);
        // Mirror the production `start_server` ordering: registrations →
        // limits → SERVER_STARTED. Keeping ordering identical means a
        // future audit emit inside `record_limits` (e.g. limit-snapshot
        // row) lands in the same position in both code paths.
        let registered_tools: Vec<&str> = outcomes
            .iter()
            .filter_map(|o| match o {
                ToolRegistrationOutcome::Registered { full_name } => Some(full_name.as_str()),
                _ => None,
            })
            .collect();
        crate::db::try_emit_global(
            crate::db::audit_events::MCP_SERVER_STARTED,
            server_name,
            &serde_json::json!({
                "server": server_name,
                "transport": match &manifest.transport {
                    mcp::manifest::Transport::Stdio { .. } => "stdio",
                    mcp::manifest::Transport::Http { .. } => "streamable-http",
                    mcp::manifest::Transport::Sse { .. } => "sse",
                },
                "registered_tools": registered_tools,
                "registered_count": registered_tools.len(),
            }),
        )
        .await;

        let (tx, rx) = mpsc::channel::<SessionCmd>(32);
        {
            let mut map = match session_map().lock() {
                Ok(m) => m,
                Err(p) => p.into_inner(),
            };
            map.insert(server_name.to_string(), tx);
        }
        let registry_clone: McpRegistry = reg.clone();
        let filter = manifest.tools.clone();
        let server_owned = server_name.to_string();
        let builtin_owned: Vec<String> = builtin.iter().map(ToString::to_string).collect();
        tokio::spawn(async move {
            run_session_task(
                session,
                server_owned,
                rx,
                registry_clone,
                filter,
                builtin_owned,
            )
            .await;
        });
        Ok(())
    }

    /// Return the per-server rate-limit and call-timeout currently
    /// recorded in `SERVER_LIMITS`. Tests use this to assert that
    /// [`reload_from_config`] reseated limits on an unchanged server when
    /// the operator tightened (or loosened) the knobs.
    pub fn limits_for(server: &str) -> Option<(u32, u32, std::time::Duration)> {
        super::lookup_limits(server)
            .map(|l| (l.rate_limit_per_minute, l.rate_limit_burst, l.call_timeout))
    }

    /// Return registration outcomes for `server` from the live registry's
    /// status. Tests use this to assert that the schema gauntlet recorded
    /// the expected rejections.
    pub fn rejected_for(server: &str) -> Vec<(String, String)> {
        REGISTRY
            .get()
            .map(McpRegistry::status)
            .and_then(|s| s.servers.get(server).cloned())
            .map(|s| s.rejected_tools)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Source-grep guard: the trust-boundary wrapper must be present in
    /// this module's source. CLAUDE.md sanctions source-grep guards for
    /// invariants the type system cannot express; this is one. If the
    /// wrapper string is ever removed or renamed, MCP tool output would
    /// reach the LLM as plain text and bypass the untrusted-input
    /// pipeline downstream.
    #[test]
    fn mcp_tool_result_wrapper_is_present_in_source() {
        let src = include_str!("./mcp_clients.rs");
        assert!(
            src.contains("<mcp_tool_result"),
            "<mcp_tool_result wrapper string missing — MCP outputs would \
             silently bypass the untrusted-input pipeline"
        );
        assert!(
            src.contains("trust=\\\"untrusted\\\""),
            "trust=\"untrusted\" attribute missing — every MCP tool result \
             must declare itself untrusted to the prompt assembler"
        );
    }

    // TokenBucket refill/fail-closed math is covered in `crate::rate_limit`.

    #[test]
    fn builtin_tool_names_includes_dispatcher_critical_names() {
        // Spec: any name in `builtin_tool_names` would silently shadow a
        // first-class tool if an MCP server registered it. Missing one
        // means a hostile (or merely careless) MCP server can shadow
        // `run_shell` and execute arbitrary commands the user thinks are
        // going to the trusted builtin.
        let names = builtin_tool_names();
        for required in [
            "apply_patch",
            "run_shell",
            "read_file",
            "list_dir",
            "web_fetch",
            "web_search",
            "write_memory",
            "read_memory",
            "memory_search",
            "schedule",
        ] {
            assert!(
                names.contains(&required),
                "builtin_tool_names missing '{required}' — an MCP server registering \
                 this name would silently shadow the trusted builtin (full list: {names:?})"
            );
        }
    }

    /// `server_status_snapshot` is the read-only join consumed by the
    /// TUI's `/mcp` and `/plugins` popups. It must:
    ///
    /// - return one row per configured server (regardless of
    ///   live-registry presence — the registry only populates after
    ///   `initialize`, but operators expect to see their config rows
    ///   in `/mcp` immediately);
    /// - sort by server name (deterministic order — operators scan);
    /// - propagate the per-server `enabled` flag AND the global
    ///   `mcp.disabled_servers` live-kill-switch into separate fields
    ///   (the TUI uses both to render distinct lifecycle states);
    /// - tag the transport so the popup can render `stdio`/`http`/`sse`
    ///   without re-walking `TransportSpec` itself.
    #[test]
    fn server_status_snapshot_joins_config_with_registry() {
        use crate::config::{McpServerConfig, TransportSpec};
        use std::collections::HashMap;

        let mut cfg = crate::config::Config::default();
        cfg.mcp.enabled = true;
        cfg.mcp.disabled_servers = vec!["killed".to_string()];
        cfg.mcp.servers.insert(
            "zeta".to_string(),
            McpServerConfig {
                enabled: true,
                transport: TransportSpec::Http {
                    url: "https://example.com/mcp".to_string(),
                    headers: HashMap::new(),
                    auth: Default::default(),
                },
                connect_timeout_ms: 60_000,
                call_timeout_ms: 120_000,
                rate_limit_per_minute: 60,
                rate_limit_burst: 10,
                tools: Default::default(),
            },
        );
        cfg.mcp.servers.insert(
            "alpha".to_string(),
            McpServerConfig {
                enabled: false,
                transport: TransportSpec::Stdio {
                    command: "uvx".to_string(),
                    args: Vec::new(),
                    env: HashMap::new(),
                },
                connect_timeout_ms: 60_000,
                call_timeout_ms: 120_000,
                rate_limit_per_minute: 60,
                rate_limit_burst: 10,
                tools: Default::default(),
            },
        );
        cfg.mcp.servers.insert(
            "killed".to_string(),
            McpServerConfig {
                enabled: true,
                transport: TransportSpec::Sse {
                    url: "https://sse.example.com/".to_string(),
                    headers: HashMap::new(),
                    auth: Default::default(),
                },
                connect_timeout_ms: 60_000,
                call_timeout_ms: 120_000,
                rate_limit_per_minute: 60,
                rate_limit_burst: 10,
                tools: Default::default(),
            },
        );

        let snap = server_status_snapshot(&cfg);
        assert_eq!(snap.len(), 3);

        let names: Vec<&str> = snap.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alpha", "killed", "zeta"],
            "snapshot must be sorted by server name"
        );

        let alpha = &snap[0];
        assert_eq!(alpha.transport, "stdio");
        assert!(!alpha.config_enabled, "per-server disable must propagate");
        assert!(!alpha.live_disabled);

        let killed = &snap[1];
        assert_eq!(killed.transport, "sse");
        assert!(killed.config_enabled);
        assert!(
            killed.live_disabled,
            "mcp.disabled_servers entry must surface as live_disabled — \
             the TUI relies on this to show that a just-set kill switch \
             actually took effect"
        );

        let zeta = &snap[2];
        assert_eq!(zeta.transport, "http");
        assert!(zeta.config_enabled);
        assert!(!zeta.live_disabled);
        // No `initialize()` ran in this test, so `health` is None for all
        // rows; tool_count and rejected_count are 0. That's deliberate —
        // the TUI surfaces `not-started` when health is None.
        assert!(zeta.health.is_none());
        assert_eq!(zeta.tool_count, 0);
        assert_eq!(zeta.rejected_count, 0);
    }

    /// `state_label()` is the single source of truth for the lifecycle
    /// string both popups render. The precedence rules matter: a server
    /// that's enabled in config but live-killed via
    /// `mcp.disabled_servers` MUST render as `live-disabled` (otherwise
    /// the operator's runtime kill switch looks like it didn't take),
    /// and a `Healthy`-but-`!config_enabled` row MUST render as
    /// `disabled` (not `connected`) since registry health lags config
    /// changes by a reconnect cycle.
    #[test]
    fn state_label_precedence() {
        use mcp::registry::ServerHealth;
        let base = McpServerSnapshot {
            name: "s".to_string(),
            transport: "stdio",
            config_enabled: true,
            live_disabled: false,
            health: Some(ServerHealth::Healthy),
            tool_count: 0,
            rejected_count: 0,
        };

        // live_disabled wins over everything else.
        let mut s = base.clone();
        s.live_disabled = true;
        s.config_enabled = true;
        s.health = Some(ServerHealth::Healthy);
        assert_eq!(s.state_label(), "live-disabled");

        // !config_enabled wins over registry health.
        let mut s = base.clone();
        s.config_enabled = false;
        s.health = Some(ServerHealth::Healthy);
        assert_eq!(s.state_label(), "disabled");

        // Health labels map directly when enabled + not live-disabled.
        let mut s = base.clone();
        s.health = Some(ServerHealth::Healthy);
        assert_eq!(s.state_label(), "connected");
        s.health = Some(ServerHealth::Degraded);
        assert_eq!(s.state_label(), "degraded");
        s.health = None;
        assert_eq!(s.state_label(), "not-started");
    }

    #[test]
    fn try_execute_returns_none_for_non_mcp_name() {
        // Spec: a non-mcp_-prefixed name MUST return Ok(None) so the
        // agent dispatcher can fall through. Returning Ok(Some(_)) or
        // Err(_) here would shadow every builtin tool.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let r = rt
            .block_on(super::try_execute_tool("run_shell", &serde_json::json!({})))
            .expect("ok");
        assert!(r.is_none(), "non-mcp prefix must short-circuit to None");
    }
}
