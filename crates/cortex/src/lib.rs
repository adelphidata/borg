//! Core library for the Borg AI personal assistant.
//!
//! Provides the agent loop, multi-provider LLM client, memory system, identity,
//! configuration, skills, tools, and all supporting infrastructure.

// Test code is held to looser standards than production: unwrap/expect failures
// are loud test signals, and stylistic lints add noise without catching bugs.
#![cfg_attr(
    test,
    allow(
        clippy::approx_constant,
        clippy::assertions_on_constants,
        clippy::const_is_empty,
        clippy::expect_used,
        clippy::field_reassign_with_default,
        clippy::identity_op,
        clippy::items_after_test_module,
        clippy::len_zero,
        clippy::manual_range_contains,
        clippy::needless_borrow,
        clippy::needless_collect,
        clippy::redundant_clone,
        clippy::redundant_closure_for_method_calls,
        clippy::uninlined_format_args,
        clippy::unnecessary_cast,
        clippy::unnecessary_map_or,
        clippy::unwrap_used,
        clippy::useless_format,
        clippy::useless_vec
    )
)]

/// Activity logging for session history.
pub mod activity_log;
/// Agent conversation loop and tool dispatch.
pub mod agent;
/// Authentication primitives for non-mTLS ingress surfaces — bearer
/// tokens for OpenAI-compat HTTP and MCP HTTP.
pub mod auth;
/// `BORG_HOME` backup unit: tar.zst create/verify/restore.
pub mod backup;
/// Agent bond/relationship tracking.
pub mod bond;
/// Headless Chrome automation via CDP.
pub mod browser;
/// USD budget engine with versioned rate cards and pre/post-flight enforcement.
pub mod budgets;
/// Centralized channel name constants and `ChannelName` enum.
pub mod channel_names;
/// Claude Code CLI subprocess backend for subscription-based access.
pub mod claude_cli;
/// Opt-in codex `app-server` runtime: JSON-RPC client + event projection.
pub mod codex_app_server;
/// Configuration parsing, defaults, and runtime overrides.
pub mod config;
/// File watcher for live config reloading.
pub mod config_watcher;
/// Global constants (token limits, timeouts, etc.).
pub mod constants;
/// Conversation context management and compaction.
pub mod conversation;
/// Daily summary generation from memory flush.
pub mod daily_summary;
/// SQLite database with versioned migrations.
pub mod db;
/// Diagnostic checks for `borg doctor`.
pub mod doctor;
/// Global emergency stop (`borg pause` / `borg resume`) gate helpers.
pub mod emergency_pause;
/// Typed `Error` umbrella for the crate's public API.
pub mod error;
/// Structured error formatting for user-facing messages.
pub mod error_format;
/// Conversation evolution and personality drift.
pub mod evolution;
/// Per-session workspace execution context and path resolution helpers.
pub mod execution_context;
/// Session export (JSON / CSV / TXT) shared by CLI `borg export` and TUI `/export`.
pub mod export;
/// Task-local gateway origin context for tool handlers.
pub mod gateway_context;
/// Git utilities: ghost commits, context enrichment, turn diff tracking.
pub mod git;
/// Per-session goals: types, validators, prompt builders, runtime accounting.
pub mod goals;
/// Heartbeat task constants and helpers (seeded heartbeat row).
pub mod heartbeat_task;
/// HMAC-SHA256 chain for tamper-proof event logs.
pub mod hmac_chain;
/// Lifecycle hook system for intercepting agent loop events.
pub mod hooks;
/// Host security audit checks.
pub mod host_audit;
/// IDENTITY.md load/save for agent personality.
pub mod identity;
/// Normalized ingress message schema and pre-dispatch gate (rate-limit,
/// sanitize, audit). Shared by every ingress surface — native channels,
/// OpenAI-compat HTTP, generic webhook, MCP server.
pub mod ingress;
/// Installation integrity verification.
pub mod integrity;
/// Real-time `<internal>` tag stripping from streamed output.
pub mod internal_tag_filter;
/// Multi-provider streaming SSE client.
pub mod llm;
/// Structured logging setup.
pub mod logging;
/// Daily self-healing maintenance runner (doctor sweep, log/activity
/// pruning, persistent-warning surfacing). Seeded as a scheduled task.
pub mod maintenance;
/// MCP (Model Context Protocol) client adapter. Mirrors
/// [`wasm_plugins`]'s shape: opt-in via `mcp.enabled`, contributes
/// tool definitions to the agent loop, additive when disabled.
pub mod mcp_clients;
/// Multimodal I/O: parsing/compression (core), image gen, TTS, understanding.
pub mod media;
/// Memory loading with token budget and semantic search.
pub mod memory;
/// Client-side `@path` mention expansion for the TUI composer.
pub mod mentions;
/// Migration utilities for importing from other assistants.
pub mod migrate;
/// Multi-agent orchestration.
pub mod multi_agent;
/// Output-side redaction policy applied to channel deliveries, TUI
/// transcript, and log writes. Pairs with the input-side detector in
/// [`security::secrets`].
pub mod output_redaction;
/// Sender pairing and access control for gateway channels.
pub mod pairing;
/// Canonical filesystem paths under `$BORG_HOME`. Single source of truth
/// for `~/.borg` resolution, subdirectory enumeration, and well-known
/// filenames across every crate.
pub mod paths;
/// Execution policy for collaboration modes.
pub mod policy;
/// Token pricing per provider and model.
pub mod pricing;
/// Central registry of agent-initiated subprocesses (PID, kind, age,
/// pin state) surfaced via `borg status` and folded into self-healing.
pub mod process_registry;
/// Project doc discovery (AGENTS.md / CLAUDE.md) for system prompt.
pub mod project_doc;
/// Provider trait + registry — uniform interface over all LLM transports.
pub mod providers;
/// Per-session rate limiting for tool calls and actions.
pub mod rate_guard;
/// Shared token-bucket rate limiter (per-MCP-server, per-WASM-plugin throttles).
pub mod rate_limit;
/// Script management for user-created tools.
pub mod scripts;
/// Trust-boundary cluster: sanitize, secrets (detection + resolution),
/// skill audit, input validation.
pub mod security;
/// Session management and persistence.
pub mod session;
/// Session transcript indexing for searchable conversations.
pub mod session_indexer;
/// Shared per-turn payload types crossing the gateway ↔ daemon boundary.
pub mod session_types;
/// Settings resolver: DB → TOML → compiled defaults.
pub mod settings;
/// Persistent interactive shell sessions for `run_shell` (start/write/poll/kill).
pub mod shell_session;
/// Skills loading, parsing, and progressive token budgeting.
pub mod skills;
/// Shared SSE stream processing for LLM providers.
pub mod sse;
/// Fixed UUIDs for built-in seeded `scheduled_tasks` rows.
pub mod task_ids;
/// Scheduled task management (prompt and command jobs).
pub mod tasks;
/// Anonymous telemetry collection.
pub mod telemetry;
/// System prompt template assembly.
pub mod template;
/// Typed tool dispatch: closed `enum Tool` + per-variant `Args`. Also hosts
/// catalog/definitions/effects/names/policy submodules (metadata siblings).
pub mod tool;
/// Tool execution dispatch and result handling.
pub mod tool_handlers;
/// Core types: Message, ToolCall, ToolDefinition, Role.
pub mod types;
/// Self-update mechanism for the borg binary.
pub mod update;
/// Pre-flight URL classifier and per-site policy (allow/deny, transform,
/// throttle, SSRF defense) for `web_fetch` and `browser.navigate`.
pub mod url_policy;
/// `/usage` formatter — pre-renders the multi-section text view (token
/// spend, cost, model usage, tool calls, channel activity) over a rolling
/// time window. The TUI popup renders the returned string verbatim.
pub mod usage;
/// Shared low-level primitives: hex, retry, panic-utils, truncate, unix-ext.
pub mod util;
/// Vitals system: passive agent health tracking via hooks.
pub mod vitals;
pub mod wasm_plugins;
/// Web fetching and search capabilities.
pub mod web;

pub use error::{Error, Result};
