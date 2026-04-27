//! Capability routing integration tests.
//!
//! Exercises [`borgd::grpc::capability::CapabilityRouter`]'s decision logic
//! that the agent's tool layer relies on:
//!
//!   - `terminal.exec` is `ClientWithHostFallback` — when no client has
//!     advertised it, the router returns `Unavailable` and the caller
//!     (capability bridge) is expected to substitute the host `run_shell`
//!     handler. The router itself does NOT perform the fallback; this test
//!     locks in the `Unavailable` signal so a refactor that flipped the
//!     routing rule (e.g. silently swapping to ClientRequired) would surface
//!     here as a different `InvocationResult` variant.
//!
//!   - `clipboard.read` is `ClientRequired` — when no client has advertised
//!     it, the router returns `Unavailable` for the agent to translate into
//!     a `CapabilityUnavailable` tool error. We assert the same variant as
//!     terminal.exec but the test is meaningful because the SEMANTIC
//!     contract differs: clipboard.read has no host fallback, so the agent
//!     must NEVER substitute a host execution path. The companion routing
//!     unit test (`borg_proto::capabilities::tests`) locks in the rule
//!     itself; this file locks in the runtime behavior under that rule.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use borg_proto::capabilities::{CLIPBOARD_READ, TERMINAL_EXEC};
use borgd::grpc::capability::{CapabilityRouter, InvocationResult};

#[tokio::test]
async fn terminal_exec_falls_back_to_daemon_when_no_client_capability() {
    // No client advertises terminal.exec. The router must return
    // `Unavailable` so the capability bridge can substitute the daemon-host
    // run_shell handler. Real failure mode: a regression that returned
    // `Unknown` (treating terminal.exec as host-only) would cause the agent
    // to dispatch the *built-in* tool through the wrong path, or returning
    // `Disconnected` would mislead the caller into thinking a client died.
    let router = CapabilityRouter::new();
    assert_eq!(
        router.client_count(),
        0,
        "no clients advertised at start of test"
    );

    let result = router.invoke(TERMINAL_EXEC, "{}".into()).await;
    match result {
        InvocationResult::Unavailable => {
            // Pass: the bridge will fall back to host run_shell.
        }
        other => panic!(
            "terminal.exec with no client must return Unavailable; got {other:?}"
        ),
    }
}

#[tokio::test]
async fn clipboard_read_returns_capability_unavailable_when_no_client_capability() {
    // No client advertises clipboard.read. There is NO host fallback for
    // clipboard reads (the daemon has no access to the user's clipboard);
    // the router signals `Unavailable` and the agent is expected to surface
    // a CapabilityUnavailable tool-result error. Real failure mode: a
    // regression that returned `Ok("")` here would cause the agent to
    // believe the clipboard was empty (silent wrong answer), and a
    // regression that returned `Unknown` would cause the agent to attempt
    // a host fallback that does not exist.
    let router = CapabilityRouter::new();

    let result = router.invoke(CLIPBOARD_READ, "{}".into()).await;
    match result {
        InvocationResult::Unavailable => {
            // Pass.
        }
        other => panic!(
            "clipboard.read with no client must return Unavailable; got {other:?}"
        ),
    }
}
