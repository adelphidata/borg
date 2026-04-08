//! Webhook gateway for messaging channel integrations.
//!
//! Receives webhooks from external services (Telegram, Slack, Discord, etc.),
//! routes messages to the agent, and sends responses back through the appropriate channel.

/// Auto-reply state management for channel conversations.
pub mod auto_reply;
/// Exponential backoff with jitter for polling/SSE reconnection.
pub mod backoff;
/// Pairing challenge throttle to prevent spamming unapproved senders.
pub mod challenge_throttle;
/// Channel client initialization helpers.
mod channel_init;
/// Trait abstraction for native channel implementations.
pub mod channel_trait;
/// Message chunking for platforms with length limits.
pub mod chunker;
/// Circuit breaker for failing channel backends.
pub mod circuit_breaker;
/// Slash command parsing and dispatch for channel messages.
pub mod commands;
/// Gateway-wide constants (timeouts, limits, etc.).
pub mod constants;
/// Cryptographic utilities for webhook signature verification.
pub mod crypto;
/// Message deduplication to prevent processing duplicates.
pub mod dedup;
/// Native Discord Bot API integration.
pub mod discord;
/// Error deduplication to reduce noise in channel responses.
pub mod error_dedup;
/// Channel script subprocess execution.
pub mod executor;
/// Native Google Chat integration.
pub mod google_chat;
/// Webhook handler: verify, parse, invoke agent, respond.
pub mod handler;
/// Channel health monitoring and status tracking.
pub mod health;
/// HTTP retry logic for outbound channel API calls.
pub mod http_retry;
/// Native iMessage integration (macOS only).
#[cfg(target_os = "macos")]
pub mod imessage;
/// Per-session in-flight agent turn cancellation registry.
pub mod in_flight;
/// URL preview and link understanding for enriched responses.
pub mod link_understanding;
/// `channel.toml` manifest parsing.
pub mod manifest;
/// Sliding-window rate limiting for inbound messages.
pub mod rate_limit;
/// Channel registry: scan and register user-defined channels.
pub mod registry;
/// Retry utilities for transient channel failures.
pub mod retry;
/// Route resolution: map webhook paths to channel handlers.
pub mod routing;
/// Axum HTTP server with webhook routes and native channel endpoints.
pub mod server;
/// Per-session message queue for serializing concurrent requests.
pub mod session_queue;
/// Native Signal messenger integration via signal-cli.
pub mod signal;
/// Native Slack Bot API integration.
pub mod slack;
/// Native Microsoft Teams integration.
pub mod teams;
/// Native Telegram Bot API integration.
pub mod telegram;
/// Native Twilio integration (WhatsApp + SMS).
pub mod twilio;
pub(crate) mod typing_keepalive;

pub use registry::ChannelRegistry;
pub use server::GatewayServer;
