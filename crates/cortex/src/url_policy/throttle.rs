//! Per-host throttle tracker. Keeps a process-global
//! `HashMap<key, last_seen>` and returns `Throttle` when a matching
//! rule's `min_interval_ms` has not yet elapsed since the previous
//! request bucketed under that key.
//!
//! Process-global (not config-scoped) so that re-loading config does not
//! reset the throttle and let a tight loop burn through it. The map is
//! pruned of entries older than 1 hour on each insert to keep it bounded
//! when an agent walks many distinct hosts.
//!
//! The bucket key is `host:port` (constructed in `super::classify`), so
//! two services on the same host stay isolated — including parallel
//! wiremock servers in integration tests.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::config::ThrottleRule;

use super::rules::{matches_rule, normalize_rule};

const PRUNE_OLDER_THAN: Duration = Duration::from_secs(3600);

static LAST_SEEN: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Check if `match_host` is throttled by any rule, and if not, record
/// `now` as the new `last_seen` for `key` so subsequent calls within the
/// window will be throttled.
///
/// `key` is the per-bucket identity (e.g. `example.com:443`) — same host
/// on different ports gets isolated buckets. `match_host` is the bare
/// host used for rule pattern matching.
///
/// Returns `Some((rule_pattern, retry_after_ms))` when throttled, `None`
/// when permitted (and `last_seen[key]` was updated).
pub fn check_and_record(
    key: &str,
    match_host: &str,
    rules: &[ThrottleRule],
) -> Option<(String, u64)> {
    let matched = first_matching_rule(match_host, rules)?;
    let min_interval = Duration::from_millis(matched.min_interval_ms);

    let mut map = LAST_SEEN.lock().unwrap_or_else(|e| {
        tracing::warn!("throttle mutex poisoned, recovering: {e}");
        e.into_inner()
    });
    let now = Instant::now();
    prune(&mut map, now);

    if let Some(prev) = map.get(key) {
        // `Instant` is monotonic on all supported platforms; saturate
        // defensively in case a backend ever violates that.
        let elapsed = now.saturating_duration_since(*prev);
        if elapsed < min_interval {
            let remaining = min_interval - elapsed;
            return Some((matched.pattern.clone(), remaining.as_millis() as u64));
        }
    }
    map.insert(key.to_string(), now);
    None
}

fn first_matching_rule<'a>(
    match_host: &str,
    rules: &'a [ThrottleRule],
) -> Option<&'a ThrottleRule> {
    rules.iter().find(|r| {
        normalize_rule(&r.pattern)
            .map(|pat| matches_rule(match_host, &pat))
            .unwrap_or(false)
    })
}

fn prune(map: &mut HashMap<String, Instant>, now: Instant) {
    map.retain(|_, last| now.saturating_duration_since(*last) < PRUNE_OLDER_THAN);
}

/// Wipe global throttle state. Exposed (not `cfg(test)`-gated) so
/// integration tests in other crate compilation units can reset between
/// runs — the unit-test sibling otherwise can't reach the global from a
/// `tests/` integration binary.
#[doc(hidden)]
pub fn reset_for_tests() {
    LAST_SEEN
        .lock()
        .unwrap_or_else(|e| {
            tracing::warn!("throttle mutex poisoned during reset, recovering: {e}");
            e.into_inner()
        })
        .clear();
}
