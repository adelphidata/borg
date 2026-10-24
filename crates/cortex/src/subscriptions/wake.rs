//! Process-global signals between the producers (gateway, tools) and the
//! dispatcher in `borgd`.
//!
//! Only a wake-up crosses this boundary, never an event: the row is already
//! in `subscription_events` when [`poke`] runs, so a missed poke costs at most
//! one dispatcher tick, and nothing is lost.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock, RwLock};

use tokio::sync::Notify;

static WAKE: OnceLock<Arc<Notify>> = OnceLock::new();
static TAPPED_CHANNELS: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();

/// Install the dispatcher's wake handle. The first install wins; later calls
/// return the one already installed so every caller shares it.
pub fn install() -> Arc<Notify> {
    WAKE.get_or_init(|| Arc::new(Notify::new())).clone()
}

/// Tell the dispatcher a row is waiting. A no-op before [`install`] (the CLI,
/// tests): the dispatcher's own tick finds the row.
pub fn poke() {
    if let Some(notify) = WAKE.get() {
        notify.notify_one();
    }
}

fn tapped() -> &'static RwLock<HashSet<String>> {
    TAPPED_CHANNELS.get_or_init(|| RwLock::new(HashSet::new()))
}

/// Replace the set of channels that have an active subscription. The
/// dispatcher calls this at boot and after every change, so the channel tap
/// can answer [`is_channel_tapped`] without touching the database on the
/// reply path.
pub fn set_tapped_channels(channels: impl IntoIterator<Item = String>) {
    let next: HashSet<String> = channels.into_iter().collect();
    match tapped().write() {
        Ok(mut guard) => *guard = next,
        Err(poisoned) => *poisoned.into_inner() = next,
    }
}

/// Whether any active subscription listens on `channel`.
pub fn is_channel_tapped(channel: &str) -> bool {
    match tapped().read() {
        Ok(guard) => guard.contains(channel),
        Err(poisoned) => poisoned.into_inner().contains(channel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tapped_channel_set_is_replaced_not_merged() {
        set_tapped_channels(["slack".to_string(), "discord".to_string()]);
        assert!(is_channel_tapped("slack"));
        set_tapped_channels(["telegram".to_string()]);
        assert!(!is_channel_tapped("slack"), "an old channel must drop out");
        assert!(is_channel_tapped("telegram"));
        set_tapped_channels(Vec::<String>::new());
        assert!(!is_channel_tapped("telegram"));
    }

    #[tokio::test]
    async fn a_poke_after_install_wakes_the_waiter() {
        let notify = install();
        let waiter = tokio::spawn({
            let notify = notify.clone();
            async move { notify.notified().await }
        });
        tokio::task::yield_now().await;
        poke();
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("poke must wake the dispatcher")
            .expect("waiter task");
    }
}
