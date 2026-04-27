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

pub mod approval_greeting;
/// Hourly budget reset tick spawned at daemon startup.
pub mod budget_tick;
pub mod channel_delivery;
pub mod daemon;
pub mod grpc;
pub mod heartbeat;
pub mod heartbeat_augmenters;
pub mod orchestrator;
pub mod paths;
pub mod pidlock;
pub mod scheduler;
pub mod session;
pub mod task_runner;
