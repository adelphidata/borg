//! Borg agent daemon library.
//!
//! Exposes the daemon's lifecycle, gRPC services, and helper modules so
//! integration tests can construct a real daemon (or just its services)
//! against a temp `BORG_HOME` without going through the binary entry point.

#![warn(missing_docs)]
// Tests intentionally use `.expect()` for setup that, if it fails, means the
// test environment itself is broken — the workspace-level lint is for
// production code.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

/// APNs push pipeline — register iOS devices, wake them with silent
/// pushes. Currently surface + stub verifier; see `apns::attest`.
pub mod apns;
pub mod approval_greeting;
pub mod auth;
/// Hourly budget reset tick spawned at daemon startup.
pub mod budget_tick;
pub mod channel_delivery;
pub mod cost_events;
pub mod daemon;
/// LAN/peer discovery — mDNS announcement and Tailscale peer enumeration.
pub mod discovery;
/// Typed `Error` umbrella for the crate's public API.
pub mod error;
pub mod goal_bus;
pub mod grpc;
pub mod heartbeat;
pub mod heartbeat_augmenters;
pub(crate) mod maintenance;
pub mod orchestrator;
pub mod pairing;
pub mod paths;
pub mod pidlock;
pub mod pki_grpc;
/// Daemon tokio runtime construction — enlarged worker stack so the agent
/// loop's poll chain cannot overflow and abort the process.
pub mod runtime;
pub mod scheduler;
pub mod session;
pub mod supervisor;
pub mod task_runner;
pub mod tick_lock;
/// Owner of the optional public-internet tunnel (cloudflared / ngrok /
/// tailscale / custom). Backs the `Admin.TunnelStart/Stop/Status` RPCs.
pub mod tunnel_supervisor;

pub use error::{Error, Result};
