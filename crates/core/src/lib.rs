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
/// Agent bond/relationship tracking.
pub mod bond;
/// Headless Chrome automation via CDP.
pub mod browser;
/// Centralized channel name constants and `ChannelName` enum.
pub mod channel_names;
/// Markdown-aware content chunking with code fence preservation.
pub mod chunker;
/// Claude Code CLI subprocess backend for subscription-based access.
pub mod claude_cli;
/// Configuration parsing, defaults, and runtime overrides.
pub mod config;
/// File watcher for live config reloading.
pub mod config_watcher;
/// Memory consolidation pipeline (nightly/weekly scheduled tasks).
pub mod consolidation;
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
/// Embedding API client, cosine similarity, and hybrid search.
pub mod embeddings;
/// Structured error formatting for user-facing messages.
pub mod error_format;
/// Conversation evolution and personality drift.
pub mod evolution;
/// Task-local gateway origin context for tool handlers.
pub mod gateway_context;
/// Git utilities: ghost commits, context enrichment, turn diff tracking.
pub mod git;
/// HMAC-SHA256 chain for tamper-proof event logs.
pub mod hmac_chain;
/// Lifecycle hook system for intercepting agent loop events.
pub mod hooks;
/// Host security audit checks.
pub mod host_audit;
/// IDENTITY.md load/save for agent personality.
pub mod identity;
/// AI image generation (OpenAI DALL-E, Fal).
pub mod image_gen;
/// Installation integrity verification.
pub mod integrity;
/// Real-time `<internal>` tag stripping from streamed output.
pub mod internal_tag_filter;
/// Multi-provider streaming SSE client.
pub mod llm;
/// LLM error classification and retry logic.
pub mod llm_error;
/// Structured logging setup.
pub mod logging;
/// Daily self-healing maintenance runner (doctor sweep, log/activity
/// pruning, persistent-warning surfacing). Seeded as a scheduled task.
pub mod maintenance;
/// Media file handling and type detection.
pub mod media;
/// Media understanding (image/audio analysis via LLM).
pub mod media_understanding;
/// Memory loading with token budget and semantic search.
pub mod memory;
/// Client-side `@path` mention expansion for the TUI composer.
pub mod mentions;
/// Migration utilities for importing from other assistants.
pub mod migrate;
/// MMR diversity re-ranking (Jaccard similarity, greedy selection).
pub mod mmr;
/// Static metadata for known LLM models — context windows and capability flags.
pub mod model_registry;
/// Multi-agent orchestration.
pub mod multi_agent;
/// Sender pairing and access control for gateway channels.
pub mod pairing;
/// Execution policy for collaboration modes.
pub mod policy;
/// Token pricing per provider and model.
pub mod pricing;
/// Project doc discovery (AGENTS.md / CLAUDE.md) for system prompt.
pub mod project_doc;
/// LLM provider enum, auto-detection, and API headers.
pub mod provider;
/// Per-session rate limiting for tool calls and actions.
pub mod rate_guard;
/// Retry utilities with exponential backoff.
pub mod retry;
/// Prompt injection detection and content sanitization.
pub mod sanitize;
/// Script management for user-created tools.
pub mod scripts;
/// Secret detection and redaction in tool outputs.
pub mod secrets;
/// Credential resolution from env, file, exec, or keychain.
pub mod secrets_resolve;
/// Session management and persistence.
pub mod session;
/// Session transcript indexing for searchable conversations.
pub mod session_indexer;
/// Settings resolver: DB → TOML → compiled defaults.
pub mod settings;
/// Short-term (working) memory for the current session.
pub mod short_term_memory;
/// Security validation for user-defined skills.
pub mod skill_security;
/// Skills loading, parsing, and progressive token budgeting.
pub mod skills;
/// macOS sleep inhibitor to keep daemon alive.
pub mod sleep_inhibitor;
/// Shared SSE stream processing for LLM providers.
pub mod sse;
/// Scheduled task management (prompt and command jobs).
pub mod tasks;
/// Anonymous telemetry collection.
pub mod telemetry;
/// System prompt template assembly.
pub mod template;
/// Token estimation via tiktoken (cl100k_base BPE).
pub mod tokenizer;
/// Tool catalog for dynamic tool registration.
pub mod tool_catalog;
/// Core tool definitions sent to the LLM.
pub mod tool_definitions;
/// Tool dispatch helpers (write_memory effects, multi-agent routing).
pub(crate) mod tool_dispatch;
/// Tool-call effect classification and concurrent-grouping planner.
pub mod tool_effects;
/// Tool execution dispatch and result handling.
pub mod tool_handlers;
/// Centralized tool name constants and dispatch macro.
pub mod tool_names;
/// Tool access policy (allow/deny lists, profiles).
pub mod tool_policy;
/// Text truncation with head+tail preservation.
pub mod truncate;
/// Text-to-speech integration.
pub mod tts;
/// Core types: Message, ToolCall, ToolDefinition, Role.
pub mod types;
/// Self-update mechanism for the borg binary.
pub mod update;
/// Vitals system: passive agent health tracking via hooks.
pub mod vitals;
/// Web fetching and search capabilities.
pub mod web;
/// Workflow engine — durable multi-step task orchestration for weaker models.
pub mod workflow;
/// XML utility functions for structured content parsing.
pub mod xml_util;
