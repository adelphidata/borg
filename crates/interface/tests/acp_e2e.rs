//! End-to-end ACP integration tests.
//!
//! Runs the real `borg acp` subprocess against an in-process `borgd` (full
//! tonic services bound to a per-test UDS, with the `ScriptedBackend` from
//! `borgd::session::scripted` providing canned `AgentEvent` sequences). The
//! tests assert on the JSON-RPC frames exchanged on stdio — every line item
//! in `ACP_IMPLEMENTATION_PLAN.md` §Tests / §Acceptance Criteria has at
//! least one case here.
//!
//! Submodules under `tests/acp_e2e/` are pulled in via `#[path]` because
//! Cargo's module-discovery rules forbid a sibling `acp_e2e/mod.rs` next to
//! the entry file (`acp_e2e.rs`). Keeping everything in one test binary
//! lets all cases share the harness's `OnceLock<TempDir>` for
//! `BORG_HOME` and serialise via a single `tokio::sync::Mutex`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "acp_e2e/harness.rs"]
pub mod harness;

#[path = "acp_e2e/fs_bridge.rs"]
mod fs_bridge;
#[path = "acp_e2e/lifecycle.rs"]
mod lifecycle;
#[path = "acp_e2e/protocol.rs"]
mod protocol;
#[path = "acp_e2e/spec_conformance.rs"]
mod spec_conformance;
#[path = "acp_e2e/streaming.rs"]
mod streaming;
#[path = "acp_e2e/workspace.rs"]
mod workspace;
