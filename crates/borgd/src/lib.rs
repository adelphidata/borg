//! Borg agent daemon library.
//!
//! Exposes the daemon's lifecycle, gRPC services, and helper modules so
//! integration tests can construct a real daemon (or just its services)
//! against a temp `BORG_HOME` without going through the binary entry point.

#![warn(missing_docs)]
// The task/gRPC spawn sites nest deep async stacks (tonic → hyper → tokio),
// and auto-trait solving for those future types exceeds the default limit.
#![recursion_limit = "256"]
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
pub mod grpc_web;
pub mod heartbeat;
pub mod heartbeat_augmenters;
pub(crate) mod maintenance;
pub mod orchestrator;
pub mod pairing;
pub mod pidlock;
pub mod pki_grpc;
/// Daemon tokio runtime construction — enlarged worker stack so the agent
/// loop's poll chain cannot overflow and abort the process.
pub mod runtime;
pub mod scheduler;
pub mod session;
pub mod session_activity;
pub mod session_heartbeat;
/// Subscriptions: the dispatcher that turns outside and internal events into
/// agent runs, and the activity bus the app listens to.
pub mod subscriptions;
pub mod supervisor;
pub mod task_preflight;
pub mod task_runner;
pub mod tick_lock;
/// Owner of the optional public-internet tunnel (cloudflared / ngrok /
/// tailscale / custom). Backs the `Admin.TunnelStart/Stop/Status` RPCs.
pub mod tunnel_supervisor;

pub use error::{Error, Result};

/// Serialises tests that mutate process-wide environment variables
/// (`BORG_HOME`, channel tokens). One lock for the whole crate: two modules
/// with private locks still race each other.
#[cfg(test)]
pub(crate) mod test_env {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `BORG_HOME` pointed at a fresh directory for the guard's life, under
    /// [`env_lock`], so `Database::open` and `Config::load_from_db` in the
    /// code under test never touch the real `~/.borg`.
    pub(crate) struct IsolatedHome {
        prev: Option<String>,
        _tmp: tempfile::TempDir,
        _lock: MutexGuard<'static, ()>,
    }

    impl IsolatedHome {
        pub(crate) fn new() -> Self {
            let lock = env_lock();
            let tmp = tempfile::tempdir().expect("tempdir");
            let prev = std::env::var("BORG_HOME").ok();
            std::env::set_var("BORG_HOME", tmp.path());
            Self {
                prev,
                _tmp: tmp,
                _lock: lock,
            }
        }
    }

    impl Drop for IsolatedHome {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("BORG_HOME", v),
                None => std::env::remove_var("BORG_HOME"),
            }
        }
    }
}
