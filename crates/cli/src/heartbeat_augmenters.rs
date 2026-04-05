//! Proactive nudge injections for heartbeat turns.
//!
//! During a heartbeat tick, each [`Augmenter`] in [`AUGMENTERS`] inspects
//! the current [`Config`] (and an optional [`Database`] for per-id cooldown
//! state) and may return a short snippet. Snippets that fire are appended
//! to the heartbeat user message inside a `<proactive_nudges>` block so the
//! agent naturally weaves them into its response.
//!
//! ## Design goals
//!
//! - **Heartbeat-scoped**: augmenters are only invoked from
//!   `execute_heartbeat_turn` — regular chat turns are untouched.
//! - **Token-efficient**: a check returning `None` costs nothing; a firing
//!   check adds ~100 tokens to exactly one turn.
//! - **Rate-limited**: each augmenter declares a `cooldown_secs`, tracked
//!   in the existing `meta` SQLite table under
//!   `heartbeat_augmenter_last_fired_{id}`. Within the cooldown window the
//!   augmenter is silently skipped so the agent doesn't nag.
//! - **Extensible**: add a new nudge by appending one entry to
//!   [`AUGMENTERS`] — no other wiring needed.
//!
//! ## Extension points — how to add a new proactive nudge
//!
//! To add another heartbeat nudge later (e.g. "budget 80% used", "no
//! IDENTITY.md personalization", "gateway not running"):
//!
//! 1. Write a pure check function:
//!    ```ignore
//!    fn check_my_condition(config: &Config) -> Option<String> {
//!        if condition_is_ok(config) { return None; }
//!        Some("Directive to the LLM about what to do this tick...".into())
//!    }
//!    ```
//!    The returned string is a *directive to the model*, not a literal
//!    message to the user — the agent rewrites it in its own voice. Keep
//!    it to 2–3 sentences and tell the model when to stay quiet.
//!
//! 2. Append an entry to [`AUGMENTERS`]:
//!    ```ignore
//!    Augmenter {
//!        id: "my_condition",          // stable, unique, used as cooldown key
//!        cooldown_secs: 24 * 60 * 60, // how often it may re-fire
//!        check: check_my_condition,
//!    }
//!    ```
//!
//! 3. Add unit tests alongside the existing ones: one for the positive
//!    case (returns `Some`), one for the negative case (returns `None`),
//!    and — if the check is non-trivial — one exercising [`collect`] to
//!    verify cooldown behavior end-to-end.
//!
//! No changes to `service.rs`, the agent loop, or any registry are
//! required. [`collect`] picks up the new entry automatically on the next
//! heartbeat tick.

use borg_core::config::Config;
use borg_core::db::Database;

/// A single proactive nudge check.
///
/// See the module-level docs for how to add one.
pub struct Augmenter {
    /// Stable identifier. Also used as the suffix of the `meta` table key
    /// that stores this augmenter's last-fired timestamp for cooldown
    /// tracking. Must be unique across all augmenters.
    pub id: &'static str,
    /// Minimum seconds between successive firings of this augmenter.
    /// Enforced by [`collect`] via the `meta` table — the `check` fn
    /// itself is stateless.
    pub cooldown_secs: u64,
    /// Pure predicate. Returns `Some(snippet)` when the nudge should be
    /// considered for this tick; `None` to stay silent. The snippet is an
    /// instruction to the LLM about what to proactively tell the user.
    pub check: fn(&Config) -> Option<String>,
}

/// Ordered list of heartbeat augmenters. Append new entries here to add
/// nudges — see the module-level docs.
pub const AUGMENTERS: &[Augmenter] = &[Augmenter {
    id: "no_channels_configured",
    cooldown_secs: 24 * 60 * 60,
    check: check_no_channels,
}];

/// Run every augmenter in [`AUGMENTERS`] and return the snippets that both
/// fire and are past their cooldown.
///
/// When `db` is `Some`, fired augmenters have their last-fired timestamp
/// written to the `meta` table so subsequent calls within `cooldown_secs`
/// skip them. When `db` is `None` (e.g. the DB failed to open) cooldown is
/// bypassed and every firing augmenter is returned — we prefer an extra
/// nudge to a silent heartbeat.
pub fn collect(config: &Config, db: Option<&Database>) -> Vec<String> {
    let now = unix_now();
    let mut out = Vec::new();
    for aug in AUGMENTERS {
        let Some(snippet) = (aug.check)(config) else {
            continue;
        };
        if let Some(db) = db {
            let key = meta_key(aug.id);
            let last = db
                .get_meta(&key)
                .ok()
                .flatten()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            if now.saturating_sub(last) < aug.cooldown_secs {
                continue;
            }
            if let Err(e) = db.set_meta(&key, &now.to_string()) {
                tracing::warn!(
                    "heartbeat_augmenters: failed to persist cooldown for '{}': {e}",
                    aug.id
                );
            }
        }
        out.push(snippet);
    }
    out
}

fn meta_key(id: &str) -> String {
    format!("heartbeat_augmenter_last_fired_{id}")
}

/// No messaging channels configured — nudge the user to run `borg add`.
fn check_no_channels(config: &Config) -> Option<String> {
    if config.has_any_native_channel() {
        return None;
    }
    // Future: also scan ~/.borg/channels/ via ChannelRegistry for
    // script-based channels (iMessage, SMS, WhatsApp templates). For v1,
    // native coverage is sufficient — those are the recommended defaults.
    Some(NO_CHANNELS_NUDGE.to_string())
}

const NO_CHANNELS_NUDGE: &str = "\
No messaging channels are configured yet, so the user can only reach you \
through the terminal. This tick, briefly and warmly suggest setting one \
up so you can message them on the go. Mention that `borg plugins` lists \
every integration and `borg add <name>` (e.g. `borg add telegram`) walks \
through the credentials. Native options: telegram, slack, discord, teams, \
google-chat, twilio. Keep it to 2-3 sentences — don't lecture, and don't \
repeat yourself if you've already raised this recently.";

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Keys that `has_any_native_channel` checks — cleared before each test
    /// so ambient environment (dev machines, CI secrets) can't flip the
    /// check's result.
    const NATIVE_KEYS: &[&str] = &[
        "TELEGRAM_BOT_TOKEN",
        "SLACK_BOT_TOKEN",
        "DISCORD_BOT_TOKEN",
        "TWILIO_ACCOUNT_SID",
        "TEAMS_APP_ID",
        "GOOGLE_CHAT_SERVICE_TOKEN",
        "SIGNAL_ACCOUNT",
    ];

    /// Tests in this module mutate process-wide environment variables, so
    /// they must run serially — cargo test's default thread pool would
    /// otherwise let them race and observe each other's state.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn clear_native() -> Self {
            let lock = env_lock();
            let saved = NATIVE_KEYS
                .iter()
                .map(|k| (*k, std::env::var(k).ok()))
                .collect();
            for k in NATIVE_KEYS {
                std::env::remove_var(k);
            }
            Self { saved, _lock: lock }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn in_memory_db() -> Database {
        Database::from_connection(Connection::open_in_memory().expect("open in-memory sqlite"))
            .expect("init in-memory db")
    }

    #[test]
    fn check_no_channels_returns_none_when_channel_configured() {
        let _guard = EnvGuard::clear_native();
        std::env::set_var("TELEGRAM_BOT_TOKEN", "test-token");
        let cfg = Config::default();
        assert!(check_no_channels(&cfg).is_none());
        std::env::remove_var("TELEGRAM_BOT_TOKEN");
    }

    #[test]
    fn check_no_channels_returns_some_when_no_channels() {
        let _guard = EnvGuard::clear_native();
        let cfg = Config::default();
        let snippet = check_no_channels(&cfg).expect("nudge should fire");
        assert!(snippet.contains("borg add"));
        assert!(snippet.contains("borg plugins"));
    }

    #[test]
    fn collect_fires_once_then_respects_cooldown() {
        let _guard = EnvGuard::clear_native();
        let cfg = Config::default();
        let db = in_memory_db();

        let first = collect(&cfg, Some(&db));
        assert_eq!(first.len(), 1, "expected nudge on first call");

        let second = collect(&cfg, Some(&db));
        assert!(
            second.is_empty(),
            "expected cooldown to suppress second call, got {second:?}"
        );
    }

    #[test]
    fn collect_fires_again_after_cooldown_elapses() {
        let _guard = EnvGuard::clear_native();
        let cfg = Config::default();
        let db = in_memory_db();

        // First firing persists a timestamp.
        assert_eq!(collect(&cfg, Some(&db)).len(), 1);

        // Rewind the stored timestamp to simulate cooldown elapsing.
        let stale = unix_now().saturating_sub(25 * 60 * 60);
        db.set_meta(&meta_key("no_channels_configured"), &stale.to_string())
            .unwrap();

        let again = collect(&cfg, Some(&db));
        assert_eq!(again.len(), 1, "expected nudge to re-fire after cooldown");
    }

    #[test]
    fn collect_without_db_always_returns_firing_checks() {
        let _guard = EnvGuard::clear_native();
        let cfg = Config::default();
        // No DB → no cooldown enforcement; every call returns the snippet.
        assert_eq!(collect(&cfg, None).len(), 1);
        assert_eq!(collect(&cfg, None).len(), 1);
    }

    #[test]
    fn collect_silent_when_channel_configured() {
        let _guard = EnvGuard::clear_native();
        std::env::set_var("SLACK_BOT_TOKEN", "xoxb-test");
        let cfg = Config::default();
        let db = in_memory_db();
        assert!(collect(&cfg, Some(&db)).is_empty());
        std::env::remove_var("SLACK_BOT_TOKEN");
    }
}
