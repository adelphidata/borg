//! Sandboxing for script execution — macOS Seatbelt and Linux Bubblewrap.
//!
//! Provides platform-specific process isolation with deny-all defaults
//! and explicit permission grants for filesystem, network, and IPC access.
#![warn(missing_docs)]
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

/// Linux Bubblewrap (`bwrap`) sandboxing with namespace isolation.
pub mod bubblewrap;
/// Shared security-critical constants (env allowlist, exec paths, sensitive paths).
pub mod constants;
/// Bounded output draining once a process leader exits ahead of its descendants.
pub mod drain;
/// Path utilities shared across sandbox and channel code.
pub mod paths;
/// Sandbox policy definition and command wrapping.
pub mod policy;
/// Signalling a child's whole process group, with a per-member macOS fallback.
pub mod process_group;
/// Script runner with sandboxed subprocess execution.
pub mod runner;
/// macOS Seatbelt profile generation.
pub mod seatbelt;
/// Normalized sandbox denial events (classification only).
pub mod violation;
/// Windows sandbox backend (trampoline arg construction, restricted-token
/// and elevated-IPC backends). Trampoline arg construction is
/// platform-neutral so it builds and tests on all hosts; the actual
/// Windows API calls live under `cfg(target_os = "windows")` submodules.
pub mod windows;

/// Test-only serialisation of process-wide environment mutation. Every
/// test that mutates the environment spawns a unix child, so the lock is
/// unix-only too.
#[cfg(all(test, unix))]
pub(crate) mod test_support {
    /// Held by every test that changes `PATH` (or reads it expecting a
    /// stable value across an await), since the test binary runs tests on
    /// parallel threads and the environment is process-global. An async
    /// mutex so an async test can hold it across the await that spawns the
    /// child; sync tests take it with [`env_lock`].
    pub(crate) static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Lock [`ENV_LOCK`] from a synchronous test (no runtime on this thread).
    pub(crate) fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        ENV_LOCK.blocking_lock()
    }
}
