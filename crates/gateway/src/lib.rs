//! Webhook gateway for messaging channel integrations.

// The webhook handlers nest deep async stacks (axum → reqwest → hyper), and
// auto-trait solving for those future types exceeds the default limit.
#![recursion_limit = "256"]
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

/// Fire-and-forget gateway activity logging shared across handler/server/channels.
pub mod activity;
pub mod auto_reply;
/// Ephemeral `/btw` side-question dispatcher shared across native channels.
pub mod btw_dispatch;
/// Shared channel infrastructure: trait, init, registry, routing.
pub mod channel;
pub mod commands;
pub mod constants;
pub mod crypto;
pub mod discord;
/// Typed error surface for the crate's public API.
pub mod error;
pub mod executor;
pub mod format_common;
/// Generic signed-payload webhook surface — `POST /v1/webhook/{name}`
/// with HMAC-SHA256 verification. Lets users wire arbitrary upstream
/// JSON events into the agent loop via per-webhook JSON dot-paths.
pub mod generic_webhook;
/// Native Google Chat integration.
pub mod google_chat;
pub mod handler;
pub mod health;
#[cfg(target_os = "macos")]
pub mod imessage;
pub mod manifest;
/// MCP server — exposes Borg's read-only tools to MCP clients
/// (Claude Desktop, MCP Inspector) over stdio + HTTP transports.
pub mod mcp;
/// Message-content transforms (chunking, dedup, link understanding, plain-text).
pub mod message_processing;
/// OpenAI-compatible HTTP surface — `/v1/chat/completions`, `/v1/models`.
/// Bearer auth via `cortex::auth::tokens`. Lets ChatBox / Open
/// WebUI / the official `openai` SDK point at borg as if it were
/// OpenAI's API.
pub mod openai_compat;
/// Fault-tolerance primitives: backoff, retry, circuit breaker, rate
/// limit, HTTP retry, challenge throttling.
pub mod resilience;
pub mod server;
pub mod session_executor;
pub mod session_queue;
pub mod signal;
pub mod slack;
/// Panic-isolating respawn wrapper for the long-running channel loops.
pub mod supervised_loop;
pub mod teams;
pub mod telegram;
/// Public-internet tunnel providers (cloudflared, ngrok, tailscale, custom, none).
/// Lets borg's gateway expose itself beyond localhost without the operator
/// running their own reverse proxy.
pub mod tunnel;
pub mod twilio;
pub(crate) mod typing_keepalive;
pub mod verify_common;
pub mod voice;

pub use channel::registry::ChannelRegistry;
pub use error::{Error, Result};
pub use server::GatewayServer;
pub use session_executor::{
    InboundTurn, InlineImage, SessionExecutor, SessionExecutorHandle, TurnKind,
};

/// Future returned by [`DbOpener`]: an async DB open that yields a `Database`.
/// Boxed so each HTTP-surface state struct can carry a uniform factory type.
pub type DbOpenFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = anyhow::Result<cortex::db::Database>> + Send>,
>;

/// Per-request async DB factory used by the v1 HTTP surfaces (OpenAI-compat,
/// generic webhook, MCP). Per-handler opens go through
/// `Database::open_with_timeout`, the right entry point for the gateway
/// busy-timeout profile.
pub type DbOpener = std::sync::Arc<dyn Fn() -> DbOpenFuture + Send + Sync>;
