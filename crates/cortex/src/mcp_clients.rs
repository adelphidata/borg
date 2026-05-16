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

use std::collections::HashMap;
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

/// Per-server token bucket, copied verbatim from
/// [`crate::wasm_plugins`]'s implementation (same math, same
/// fail-closed-on-zero behaviour, same return shape).
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(initial: f64) -> Self {
        Self {
            tokens: initial,
            last_refill: Instant::now(),
        }
    }

    /// Returns `None` on success (one token consumed) or
    /// `Some(retry_after)` on refusal.
    fn try_consume(&mut self, per_minute: u32, burst: u32) -> Option<Duration> {
        if per_minute == 0 || burst == 0 {
            // Misconfigured-off must fail closed, never silently mean
            // "unlimited" — that would turn a clumsy operator into a
            // denial-of-budget.
            return Some(Duration::from_secs(60));
        }
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        let refill = elapsed * (f64::from(per_minute) / 60.0);
        self.tokens = (self.tokens + refill).min(f64::from(burst));
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            None
        } else {
            let deficit = 1.0 - self.tokens;
            let seconds_to_one = deficit / (f64::from(per_minute) / 60.0);
            Some(Duration::from_secs_f64(seconds_to_one.max(0.001)))
        }
    }
}

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

/// True iff `name` belongs to an MCP tool the operator has marked
/// read-only via per-server `tools.read_only`. Default-on-miss is
/// `false`, so an uninitialized registry or unknown tool is treated as
/// mutating — Plan mode blocks it.
pub fn is_read_only_mcp_tool(name: &str) -> bool {
    REGISTRY
        .get()
        .and_then(|reg| reg.lookup(name))
        .map(|t| t.read_only)
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
            crate::db::try_emit_global(
                crate::db::audit_events::MCP_TOOL_EXECUTED,
                &tool.server,
                &serde_json::json!({
                    "server": &tool.server,
                    "tool": &tool.full_name,
                    "success": false,
                    "duration_ms": start.elapsed().as_millis() as u64,
                    "error": e.to_string(),
                }),
            )
            .await;
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
                crate::db::audit_events::MCP_SCHEMA_REJECTED,
                server_name,
                &serde_json::json!({
                    "server": server_name,
                    "reason": format!("start_server failed: {e}"),
                }),
            )
            .await;
        }
    }
    Ok(())
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
    let transport = open_transport(&manifest, policy).await?;
    let mut session = SessionActor::new(&manifest, transport);

    tokio::time::timeout(manifest.connect_timeout, session.handshake())
        .await
        .map_err(|_| anyhow!("handshake timed out for server '{server_name}'"))?
        .map_err(|e| anyhow!("handshake failed for server '{server_name}': {e}"))?;

    let discovered = session
        .list_tools()
        .await
        .map_err(|e| anyhow!("tools/list failed for server '{server_name}': {e}"))?;

    let outcomes =
        reg.replace_server_tools(server_name, &discovered.tools, &manifest.tools, builtin);
    audit_rejections(server_name, &outcomes).await;

    record_limits(server_name, server_cfg);

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
    // TODO(slice3): adopt `borgd::supervisor::spawn_supervised` so panics
    // and disconnects retry with backoff and surface to the daemon's
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

async fn audit_rejections(server_name: &str, outcomes: &[ToolRegistrationOutcome]) {
    for o in outcomes {
        match o {
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
            ToolRegistrationOutcome::FilteredOut { .. }
            | ToolRegistrationOutcome::Registered { .. } => {}
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
                let result = session
                    .call_tool(&original_name, args)
                    .await
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
                            audit_rejections(&server_name, &outcomes).await;
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
            }
        }
    }
    // Receiver closed — likely shutdown. Drop the session; its Drop will
    // close the transport.
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

        let builtin = builtin_tool_names();
        let builtin_refs: Vec<&str> = builtin.to_vec();
        let outcomes = reg.replace_server_tools(
            server_name,
            &discovered.tools,
            &manifest.tools,
            &builtin_refs,
        );
        audit_rejections(server_name, &outcomes).await;
        record_limits(server_name, server_cfg);

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

    #[test]
    fn rate_bucket_refuses_after_burst() {
        // Spec invariant: with per_minute=60 and burst=2, the third call
        // within ~0s must be refused. Locks the math against an off-by-one
        // refill regression.
        let mut b = TokenBucket::new(2.0);
        assert!(b.try_consume(60, 2).is_none());
        assert!(b.try_consume(60, 2).is_none());
        assert!(b.try_consume(60, 2).is_some());
    }

    #[test]
    fn rate_bucket_zero_fails_closed() {
        // Spec invariant: per_minute=0 must NOT mean unlimited.
        let mut b = TokenBucket::new(10.0);
        assert!(b.try_consume(0, 10).is_some());
        let mut b = TokenBucket::new(10.0);
        assert!(b.try_consume(60, 0).is_some());
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
