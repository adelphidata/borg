//! Process-global signals between the producers (gateway, tools) and the
//! dispatcher in `borgd`.
//!
//! Only a wake-up crosses this boundary, never an event: the row is already
//! in `subscription_events` when [`poke`] runs, so a missed poke costs at most
//! one dispatcher tick, and nothing is lost.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use tokio::sync::Notify;

use crate::config::SubscriptionsConfig;

static WAKE: OnceLock<Arc<Notify>> = OnceLock::new();
static TAPPED_CHANNELS: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();
static STORM_NOTICES: OnceLock<Mutex<Vec<StormNotice>>> = OnceLock::new();
static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static LIVE_SETTINGS: OnceLock<RwLock<Option<SubscriptionsConfig>>> = OnceLock::new();

/// Distinct subscriptions remembered as "seen" between two drains.
const MAX_SEEN: usize = 256;

/// Notices kept while nobody drains them; a storm is rare, so this only
/// bounds a process with no dispatcher (the CLI).
const MAX_STORM_NOTICES: usize = 64;

/// A subscription just paused itself after a burst. The producer that saw
/// the breaker trip reports it; the dispatcher tells the user once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StormNotice {
    /// The subscription that paused itself.
    pub subscription_id: String,
    /// Events seen in the window that tripped it.
    pub events_in_window: i64,
}

fn storm_notices() -> &'static Mutex<Vec<StormNotice>> {
    STORM_NOTICES.get_or_init(|| Mutex::new(Vec::new()))
}

/// Report that a subscription's storm breaker tripped, and wake the
/// dispatcher so the user hears about it now rather than on the next tick.
pub fn report_storm(subscription_id: &str, events_in_window: i64) {
    let notice = StormNotice {
        subscription_id: subscription_id.to_string(),
        events_in_window,
    };
    {
        let mut guard = match storm_notices().lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.len() < MAX_STORM_NOTICES {
            guard.push(notice);
        }
    }
    poke();
}

/// Take every pending storm notice.
pub fn take_storm_notices() -> Vec<StormNotice> {
    let mut guard = match storm_notices().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    std::mem::take(&mut *guard)
}

fn seen() -> &'static Mutex<HashSet<String>> {
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Report that an authenticated request reached a subscription, whether or
/// not it was stored. The dispatcher turns it into a live "event received"
/// for the app, which is how the setup card learns the sender is wired up
/// while the subscription is still paused. A set, so a burst costs one entry.
pub fn report_seen(subscription_id: &str) {
    {
        let mut guard = match seen().lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.len() < MAX_SEEN || guard.contains(subscription_id) {
            guard.insert(subscription_id.to_string());
        }
    }
    poke();
}

/// Take every subscription reported as seen since the last call.
pub fn take_seen() -> Vec<String> {
    let mut guard = match seen().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.drain().collect()
}

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

fn live() -> &'static RwLock<Option<SubscriptionsConfig>> {
    LIVE_SETTINGS.get_or_init(|| RwLock::new(None))
}

/// Publish the daemon's current `subscriptions.*` settings. The gateway is
/// built once from the settings it started with and a settings change does
/// not rebuild it, so the endpoint and the channel tap read these instead.
/// The dispatcher publishes on every pass and on every settings change.
pub fn set_live_settings(settings: &SubscriptionsConfig) {
    let next = Some(settings.clone());
    match live().write() {
        Ok(mut guard) => *guard = next,
        Err(poisoned) => *poisoned.into_inner() = next,
    }
}

/// The settings last published by [`set_live_settings`]. `None` before the
/// dispatcher runs (a gateway on its own, tests): the caller keeps its own.
pub fn live_settings() -> Option<SubscriptionsConfig> {
    match live().read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_settings_are_the_last_ones_published() {
        let mut settings = SubscriptionsConfig {
            enabled: false,
            ingest_burst: 7,
            ..Default::default()
        };
        set_live_settings(&settings);
        let live = live_settings().expect("published");
        assert_eq!((live.enabled, live.ingest_burst), (false, 7));
        settings.enabled = true;
        set_live_settings(&settings);
        assert!(
            live_settings().expect("published").enabled,
            "replaced, not kept"
        );
    }

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

    #[test]
    fn storm_notices_are_taken_once() {
        report_storm("sub_storm_test", 301);
        let taken: Vec<_> = take_storm_notices()
            .into_iter()
            .filter(|n| n.subscription_id == "sub_storm_test")
            .collect();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].events_in_window, 301);
        assert!(take_storm_notices()
            .iter()
            .all(|n| n.subscription_id != "sub_storm_test"));
    }

    #[test]
    fn a_burst_of_seen_reports_is_one_entry() {
        for _ in 0..1000 {
            report_seen("sub_seen_test");
        }
        let taken: Vec<_> = take_seen()
            .into_iter()
            .filter(|id| id == "sub_seen_test")
            .collect();
        assert_eq!(taken.len(), 1);
        assert!(!take_seen().contains(&"sub_seen_test".to_string()));
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
