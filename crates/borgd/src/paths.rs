//! Filesystem path helpers for borgd.
//!
//! Thin re-export of [`cortex::paths`] kept for `borgd::paths::*` callers
//! and for the external test harness (`borgd::paths::tick_lock_path` is
//! used by `tests/scheduler_tick_lock.rs`). New code in `borgd` should
//! prefer `cortex::paths::*` directly.

pub use cortex::paths::{
    pairing_socket_path, pid_path, resolve_borg_home, socket_path, tick_lock_path,
};
