//! Signalling a child's process group, with a per-member fallback.
//!
//! The implementation lives in [`sandbox::process_group`] — the crate that owns
//! process containment — because the sandboxed script runner needs the same
//! macOS `EPERM` fallback that the stdio watchdog does, and `sandbox` sits
//! below `mcp`. This re-export keeps the watchdog's existing spelling.

pub use sandbox::process_group::signal_group_or_members;
