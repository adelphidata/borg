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
/// Sandbox policy definition and command wrapping.
pub mod policy;
/// Script runner with sandboxed subprocess execution.
pub mod runner;
/// macOS Seatbelt profile generation.
pub mod seatbelt;
