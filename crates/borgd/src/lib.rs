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

pub mod daemon;
pub mod grpc;
pub mod paths;
pub mod pidlock;
pub mod session;
