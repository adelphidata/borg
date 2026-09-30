//! Signalling a child's process group, with a per-member fallback.
//!
//! The implementation lives in [`sandbox::process_group`] — the crate that owns
//! process containment — because the sandboxed script runner needs the same
//! macOS `EPERM` fallback that the stdio watchdog does, and `sandbox` sits
//! below `mcp`. This re-export keeps the watchdog's existing spelling.

pub use sandbox::process_group::signal_group_or_members;
/// Unix-only: the watchdog's spawn site is itself inside `#[cfg(unix)]`, so a
/// non-Unix stub would be dead code rather than a portability shim.
#[cfg(unix)]
pub use sandbox::process_group::{close_nonstdio_fds, detach_from_tty};
