//! Capability routing integration test.
//!
//! Locks in the runtime contract that [`CapabilityRouter`] returns
//! `Unavailable` for any registered capability when no client has advertised
//! it. The agent's tool dispatch reads this signal to decide between host
//! fallback (e.g. `terminal.exec` → daemon `run_shell`) and a
//! `CapabilityUnavailable` tool-result error (e.g. `clipboard.read`). The
//! `Unavailable` ↔ fallback-policy mapping itself lives in
//! `borg_proto::capabilities`; this test guards the routing layer's
//! observable behavior.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use borg_proto::capabilities::{CLIPBOARD_READ, TERMINAL_EXEC};
use borgd::grpc::capability::{CapabilityRouter, InvocationResult};

#[tokio::test]
async fn unadvertised_capabilities_return_unavailable() {
    let router = CapabilityRouter::new();
    assert_eq!(router.client_count(), 0);

    for capability in [TERMINAL_EXEC, CLIPBOARD_READ] {
        let result = router.invoke(capability, "{}".into()).await;
        assert!(
            matches!(result, InvocationResult::Unavailable),
            "{capability} with no client must return Unavailable; got {result:?}"
        );
    }
}
