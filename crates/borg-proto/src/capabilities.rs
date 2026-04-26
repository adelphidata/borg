//! Single source of truth for capability names exchanged over the
//! `borg.capability.v1` bidi-stream. The agent's tool layer references these
//! constants when deciding whether a tool call should route to a client or
//! fall back to daemon-host execution.

/// Run a shell command on the daemon's host. Always available; never routed.
pub const HOST_RUN_SHELL: &str = "host.run_shell";

/// Read a file on the daemon's host. Always available; never routed.
pub const HOST_READ_FILE: &str = "host.read_file";

/// Run a shell command on the *client's* host (e.g. user's laptop while
/// SSHed into the daemon's box). Falls back to `HOST_RUN_SHELL` if no client
/// advertises it.
pub const TERMINAL_EXEC: &str = "terminal.exec";

/// Read the client's clipboard. No fallback — returns CapabilityUnavailable.
pub const CLIPBOARD_READ: &str = "clipboard.read";

/// Write the client's clipboard. No fallback.
pub const CLIPBOARD_WRITE: &str = "clipboard.write";

/// Pick a file from the client's filesystem. No fallback.
pub const FILE_PICK: &str = "file.pick";

/// Show a desktop/terminal notification on the client. No fallback.
pub const NOTIFY: &str = "notify";

/// All capability names known to v1.
pub const ALL: &[&str] = &[
    HOST_RUN_SHELL,
    HOST_READ_FILE,
    TERMINAL_EXEC,
    CLIPBOARD_READ,
    CLIPBOARD_WRITE,
    FILE_PICK,
    NOTIFY,
];

/// Routing classification used by the capability bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routing {
    /// Daemon-host capability — always executed locally, never routed.
    HostOnly,
    /// Client-preferred; if no client advertises it, fall back to daemon host.
    /// Used for `terminal.exec` → `host.run_shell`.
    ClientWithHostFallback,
    /// Client-only; if no client advertises it, return CapabilityUnavailable.
    ClientRequired,
}

/// Returns the routing rule for a capability, or `None` if unknown.
pub fn routing(name: &str) -> Option<Routing> {
    match name {
        HOST_RUN_SHELL | HOST_READ_FILE => Some(Routing::HostOnly),
        TERMINAL_EXEC => Some(Routing::ClientWithHostFallback),
        CLIPBOARD_READ | CLIPBOARD_WRITE | FILE_PICK | NOTIFY => Some(Routing::ClientRequired),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_capability_in_all_has_a_routing_rule() {
        // Real failure mode: someone adds a new capability constant and
        // forgets to wire it into `routing()`. The bridge would silently
        // treat it as unknown and the agent's tool call would fail with
        // a confusing error instead of a clean fallback or rejection.
        for name in ALL {
            assert!(
                routing(name).is_some(),
                "capability {name} has no routing rule"
            );
        }
    }

    #[test]
    fn terminal_exec_falls_back_to_host_but_clipboard_does_not() {
        // Documents the asymmetric rule from architecture-redesign.md §Layer 3.
        // If anyone changes the rule, this test forces them to update the doc.
        assert_eq!(
            routing(TERMINAL_EXEC),
            Some(Routing::ClientWithHostFallback)
        );
        assert_eq!(routing(CLIPBOARD_READ), Some(Routing::ClientRequired));
        assert_eq!(routing(FILE_PICK), Some(Routing::ClientRequired));
    }
}
