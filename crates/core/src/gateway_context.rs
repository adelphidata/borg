//! Task-local gateway origin context.
//!
//! When the gateway handler invokes the agent to respond to an inbound
//! message, it wraps the call in [`scope`] so tool handlers (notably
//! `schedule`) can recover the originating channel, sender, and thread. This
//! lets scheduled tasks created mid-conversation default to replying back to
//! the same thread they were spawned from (`delivery_channel = "origin"`).
//!
//! Reading the context outside of a gateway-initiated turn yields `None`.

use serde::{Deserialize, Serialize};

/// The gateway-side context of the message that triggered the current agent turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayOriginContext {
    /// Native channel name, e.g. `"slack"`, `"telegram"`, `"discord"`.
    pub channel_name: String,
    /// Platform sender identifier (Slack user ID, Telegram chat ID, etc.).
    pub sender_id: String,
    /// Thread identifier, if the inbound message was part of a thread.
    pub thread_id: Option<String>,
}

tokio::task_local! {
    static ORIGIN: GatewayOriginContext;
}

/// Run a future with the given gateway origin context available to descendant
/// tool calls via [`current`].
pub async fn scope<F, T>(ctx: GatewayOriginContext, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    ORIGIN.scope(ctx, fut).await
}

/// Return the current task's gateway origin context, if any.
pub fn current() -> Option<GatewayOriginContext> {
    ORIGIN.try_with(|ctx| ctx.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn current_is_none_outside_scope() {
        assert!(current().is_none());
    }

    #[tokio::test]
    async fn current_returns_context_inside_scope() {
        let ctx = GatewayOriginContext {
            channel_name: "slack".into(),
            sender_id: "U123".into(),
            thread_id: Some("1700000000.000100".into()),
        };
        scope(ctx.clone(), async {
            let got = current().expect("context present");
            assert_eq!(got.channel_name, "slack");
            assert_eq!(got.sender_id, "U123");
            assert_eq!(got.thread_id.as_deref(), Some("1700000000.000100"));
        })
        .await;
        assert!(current().is_none(), "context leaks outside scope");
    }
}
