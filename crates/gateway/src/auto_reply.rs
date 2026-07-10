use std::sync::Arc;

use tokio::sync::RwLock;

use cortex::config::AutoReplyConfig;

/// Current auto-reply state.
#[derive(Debug, Clone)]
pub enum AutoReplyState {
    /// Agent is available and processing messages normally.
    Available,
    /// Agent is away with a custom or default message.
    Away(String),
}

impl Default for AutoReplyState {
    fn default() -> Self {
        Self::Available
    }
}

/// Shared auto-reply state handle.
pub type SharedAutoReplyState = Arc<RwLock<AutoReplyState>>;

/// Check if an auto-reply should be sent instead of invoking the agent.
/// Returns `Some(message)` if the agent is away, `None` if available.
pub async fn check_auto_reply(
    state: &SharedAutoReplyState,
    config: &AutoReplyConfig,
) -> Option<String> {
    if !config.enabled {
        return None;
    }
    let current = state.read().await;
    match &*current {
        AutoReplyState::Available => None,
        AutoReplyState::Away(msg) => Some(msg.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_auto_reply_available_returns_none() {
        let state: SharedAutoReplyState = Arc::new(RwLock::new(AutoReplyState::Available));
        let config = AutoReplyConfig::default();
        assert!(check_auto_reply(&state, &config).await.is_none());
    }

    #[tokio::test]
    async fn check_auto_reply_away_returns_message() {
        let msg = "Be back at 5pm".to_string();
        let state: SharedAutoReplyState = Arc::new(RwLock::new(AutoReplyState::Away(msg.clone())));
        let config = AutoReplyConfig::default();
        let result = check_auto_reply(&state, &config).await;
        assert_eq!(result, Some(msg));
    }

    #[tokio::test]
    async fn check_auto_reply_disabled_returns_none() {
        let state: SharedAutoReplyState =
            Arc::new(RwLock::new(AutoReplyState::Away("away".into())));
        let config = AutoReplyConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(check_auto_reply(&state, &config).await.is_none());
    }

    #[tokio::test]
    async fn state_toggle() {
        let state: SharedAutoReplyState = Arc::new(RwLock::new(AutoReplyState::Available));
        let config = AutoReplyConfig::default();

        // Initially available
        assert!(check_auto_reply(&state, &config).await.is_none());

        *state.write().await = AutoReplyState::Away("gone fishing".into());
        assert_eq!(
            check_auto_reply(&state, &config).await,
            Some("gone fishing".to_string())
        );

        *state.write().await = AutoReplyState::Available;
        assert!(check_auto_reply(&state, &config).await.is_none());
    }

    #[tokio::test]
    async fn default_state_does_not_auto_reply_even_when_enabled() {
        // The default state must behave as "available": with auto-reply
        // enabled but no explicit Away set, a fresh agent must process
        // messages normally (None) rather than silently auto-replying.
        let state: SharedAutoReplyState = Arc::new(RwLock::new(AutoReplyState::default()));
        let config = AutoReplyConfig {
            enabled: true,
            ..Default::default()
        };
        assert_eq!(check_auto_reply(&state, &config).await, None);
    }
}
