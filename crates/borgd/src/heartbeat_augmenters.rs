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

use crate::maintenance::cooldowns::{meta_key_for, CooldownTable};
use crate::maintenance::MonthlyBudgetTracker;
use cortex::config::Config;
use cortex::db::Database;

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
    /// Predicate. Returns `Some(snippet)` when the nudge should be
    /// considered for this tick; `None` to stay silent. The snippet is an
    /// instruction to the LLM about what to proactively tell the user.
    /// `db` is `Some` when DB-backed state (e.g. the `/doctor` squelch
    /// list) should influence the decision; `None` if the DB failed to
    /// open, in which case the check should fall back to its
    /// no-DB-available behavior.
    pub check: fn(&Config, Option<&Database>) -> Option<String>,
}

/// Ordered list of heartbeat augmenters. Append new entries here to add
/// nudges — see the module-level docs.
pub const AUGMENTERS: &[Augmenter] = &[
    Augmenter {
        id: "no_channels_configured",
        cooldown_secs: 24 * 60 * 60,
        check: check_no_channels,
    },
    Augmenter {
        id: "critical_host_security",
        cooldown_secs: 48 * 60 * 60,
        check: check_critical_host_security,
    },
    Augmenter {
        id: "task_health",
        cooldown_secs: 6 * 60 * 60,
        check: check_task_health,
    },
    Augmenter {
        id: "budget_threshold",
        cooldown_secs: 6 * 60 * 60,
        check: check_budget_threshold,
    },
    Augmenter {
        id: "identity_unpersonalized",
        cooldown_secs: 7 * 24 * 60 * 60,
        check: check_identity_unpersonalized,
    },
    Augmenter {
        id: "gateway_unrecoverable",
        cooldown_secs: 24 * 60 * 60,
        check: check_gateway_unrecoverable,
    },
];

const SESSION_START_AUGMENTER_IDS: &[&str] = &["no_channels_configured", "identity_unpersonalized"];

/// Run every augmenter in [`AUGMENTERS`] and return the snippets that both
/// fire and are past their cooldown.
///
/// When `db` is `Some`, fired augmenters have their last-fired timestamp
/// written to the `meta` table so subsequent calls within `cooldown_secs`
/// skip them. When `db` is `None` (e.g. the DB failed to open) cooldown is
/// bypassed and every firing augmenter is returned — we prefer an extra
/// nudge to a silent heartbeat.
pub fn collect(config: &Config, db: Option<&Database>) -> Vec<String> {
    collect_matching(config, db, |_| true)
}

/// Session-start greetings use a smaller proactive set than scheduled
/// heartbeats. Today that means only the no-channels nudge so opening the TUI
/// still teaches the user how to reach Borg proactively on the go.
pub fn collect_session_start(config: &Config, db: Option<&Database>) -> Vec<String> {
    collect_matching(config, db, |aug| {
        SESSION_START_AUGMENTER_IDS.contains(&aug.id)
    })
}

fn collect_matching(
    config: &Config,
    db: Option<&Database>,
    include: impl Fn(&Augmenter) -> bool,
) -> Vec<String> {
    let now = unix_now();
    let cooldowns = db.map(CooldownTable::new);
    let mut out = Vec::new();
    for aug in AUGMENTERS {
        if !include(aug) {
            continue;
        }
        let Some(snippet) = (aug.check)(config, db) else {
            continue;
        };
        if let Some(table) = &cooldowns {
            let cd = table.for_augmenter(aug.id, aug.cooldown_secs);
            if !cd.is_ready(now) {
                // Either the cooldown window hasn't elapsed or the meta
                // read failed — both mean "skip" rather than "spam".
                continue;
            }
            cd.record_fired(now);
        }
        out.push(snippet);
    }
    out
}

/// No messaging channels configured — nudge the user to run `borg add`.
fn check_no_channels(config: &Config, _db: Option<&Database>) -> Option<String> {
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
up so you can message them on the go. Tell them to run `/plugins` in the \
TUI (or `borg plugins` from the command line) to browse and install \
integrations. Native options: telegram, slack, discord, teams, \
google-chat, twilio. Keep it to 2-3 sentences — don't lecture, and don't \
repeat yourself if you've already raised this recently.";

/// Critical host security issues — runs a subset of `borg doctor` checks
/// (firewall, risky ports, SSH config) and nudges only when issues are found.
/// Respects the `security.host_audit` config toggle. Findings the user has
/// squelched via `/doctor` are filtered out here so they don't fire the
/// nudge; the squelch is the user's "I see it, stop reminding me" signal.
fn check_critical_host_security(config: &Config, db: Option<&Database>) -> Option<String> {
    if !config.security.host_audit {
        return None;
    }
    let mut checks = Vec::new();
    cortex::host_audit::check_firewall(&mut checks);
    cortex::host_audit::check_listening_ports(&mut checks);
    cortex::host_audit::check_ssh_config(&mut checks);

    if let Some(db) = db {
        let now = unix_now() as i64;
        if let Ok(squelches) = db.doctor_squelch_list_active(now) {
            let active: std::collections::HashSet<(String, String)> = squelches
                .into_iter()
                .map(|r| (r.category, r.name))
                .collect();
            checks.retain(|c| !active.contains(&(c.category.to_string(), c.name.clone())));
        }
    }

    build_security_nudge(&checks)
}

/// Build an LLM directive from diagnostic checks. Returns `None` when
/// all checks pass. Extracted so it can be tested with synthetic data.
fn build_security_nudge(checks: &[cortex::doctor::DiagnosticCheck]) -> Option<String> {
    let issues: Vec<&str> = checks
        .iter()
        .filter_map(|c| match &c.status {
            cortex::doctor::CheckStatus::Warn(msg) => Some(msg.as_str()),
            cortex::doctor::CheckStatus::Fail(msg) => Some(msg.as_str()),
            cortex::doctor::CheckStatus::Pass => None,
        })
        .collect();

    if issues.is_empty() {
        return None;
    }

    let mut nudge = String::from(
        "Critical host security issues detected on this machine. \
         Alert the user clearly but calmly about the following:\n",
    );
    for issue in &issues {
        nudge.push_str("  - ");
        nudge.push_str(issue);
        nudge.push('\n');
    }
    nudge.push_str(
        "Each item above already includes the concrete next step (e.g. a \
         settings path or shell command) — relay it to the user verbatim, \
         don't paraphrase or omit it. Also mention they can squelch a \
         finding from `/doctor` if it's intentional. Keep it brief.",
    );
    Some(nudge)
}

/// Scheduled-task health — flag tasks carrying `last_error` and recent failed
/// `task_runs`, with names and counts baked into the directive so the LLM
/// cannot freelance numbers.
///
/// Regression context: the heartbeat once produced "5 failed task runs sitting
/// in the queue" with zero DB grounding. Because the LLM had no source-of-truth
/// for the count, follow-up questions like "what are the tasks?" found the
/// agent grep'ing source code for TODOs. This check pins the heartbeat's
/// failure-talk to actual rows in `scheduled_tasks` / `task_runs`.
///
/// Window for `task_runs` failures is the last 24h — long enough to surface
/// overnight breakage on the morning tick, short enough that the same failure
/// stops being mentioned once it ages out.
fn check_task_health(_config: &Config, db: Option<&Database>) -> Option<String> {
    // Without a DB we cannot ground anything — stay silent rather than
    // hallucinate. The heartbeat-grounding regression is the whole point.
    let db = db?;
    build_task_health_nudge(db, unix_now() as i64)
}

const TASK_HEALTH_WINDOW_SECS: i64 = 24 * 60 * 60;
const TASK_HEALTH_MAX_LISTED: usize = 5;

fn build_task_health_nudge(db: &Database, now: i64) -> Option<String> {
    let failing_tasks = match db.tasks_with_last_error() {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("task_health: tasks_with_last_error failed: {e}");
            return None;
        }
    };
    let since = now - TASK_HEALTH_WINDOW_SECS;
    let recent_failed = match db.count_failed_runs_since(since) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("task_health: count_failed_runs_since failed: {e}");
            return None;
        }
    };

    if failing_tasks.is_empty() && recent_failed == 0 {
        return None;
    }

    // Pull windowed failures grouped by task so the nudge can name task IDs
    // even when `last_error` was already cleared by a later success. Without
    // this, the agent saw a count and asked the user for IDs the system
    // already knows.
    let recent_groups = if recent_failed > 0 {
        match db.failed_runs_grouped_since(since, TASK_HEALTH_MAX_LISTED) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("task_health: failed_runs_grouped_since failed: {e}");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let mut nudge = String::from(
        "Scheduled task health check — these are real DB rows, not estimates. \
         Mention them concisely; do not fabricate counts beyond what's listed:\n",
    );

    let listed_ids: std::collections::HashSet<String> = failing_tasks
        .iter()
        .take(TASK_HEALTH_MAX_LISTED)
        .map(|t| t.id.clone())
        .collect();

    if !failing_tasks.is_empty() {
        nudge.push_str(&format!(
            "  {} task(s) carry a last_error:\n",
            failing_tasks.len()
        ));
        for task in failing_tasks.iter().take(TASK_HEALTH_MAX_LISTED) {
            let err = task
                .last_error
                .as_deref()
                .unwrap_or("(unknown)")
                .replace('\n', " ");
            // Cap the per-line error so a single noisy task can't blow the
            // proactive_nudges block past sane sizes.
            let err = if err.len() > 120 { &err[..120] } else { &err };
            nudge.push_str(&format!(
                "    - {} (id={}, last_error: \"{}\")\n",
                task.name, task.id, err
            ));
        }
        if failing_tasks.len() > TASK_HEALTH_MAX_LISTED {
            nudge.push_str(&format!(
                "    … and {} more (truncated)\n",
                failing_tasks.len() - TASK_HEALTH_MAX_LISTED
            ));
        }
    }

    if recent_failed > 0 {
        nudge.push_str(&format!(
            "  {recent_failed} task_run(s) failed in the last 24h:\n"
        ));
        let mut listed = 0usize;
        for group in &recent_groups {
            // Skip tasks already named in the last_error block — same task,
            // duplicated lines just add noise.
            if listed_ids.contains(&group.task_id) {
                continue;
            }
            let err = group
                .latest_error
                .as_deref()
                .unwrap_or("(no error captured)")
                .replace('\n', " ");
            let err = if err.len() > 120 { &err[..120] } else { &err };
            nudge.push_str(&format!(
                "    - {} (id={}, {} failure(s), last_error: \"{}\")\n",
                group.task_name, group.task_id, group.failures, err
            ));
            listed += 1;
        }
        if listed == 0 && !recent_groups.is_empty() {
            // Every windowed failure was already covered above — leave a
            // marker so the agent doesn't think the second count is unmoored.
            nudge.push_str("    (all already listed above)\n");
        }
    }

    nudge.push_str(
        "The task IDs above are everything you need — do NOT ask the user for IDs. \
         Mention the failed tasks by name; if the user wants a deeper dive, call \
         `schedule` with action='runs' and one of the IDs listed. Keep it to 2–3 sentences.",
    );
    Some(nudge)
}

/// Monthly token budget approaching its cap — fires at 75/90/100%.
///
/// Stays silent when the limit is unlimited (`0`). State is per-month
/// (key includes `YYYY-MM`), so each crossed threshold fires exactly
/// once per calendar month — re-firing for a *higher* threshold is
/// allowed (75 → 90 → 100), but the same bucket won't repeat. Numbers
/// in the directive are pre-bucketed by `monthly_budget_pct` so the
/// LLM can't fabricate values between thresholds.
///
/// Coexists with `budget.warning_threshold` / `BudgetEngine` — that's
/// the system-event side; this is the conversational side.
fn check_budget_threshold(config: &Config, db: Option<&Database>) -> Option<String> {
    let db = db?;
    let limit = config.budget.monthly_token_limit;
    let bucket = match db.monthly_budget_pct(limit) {
        Ok(Some(b)) => b,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!("budget_threshold: monthly_budget_pct failed: {e}");
            return None;
        }
    };

    let total = db.monthly_token_total().ok()?;
    // Typed handle replaces the inline meta-string bookkeeping. The
    // tracker owns the per-month key and the "is this bucket new?"
    // bookkeeping; the augmenter just decides whether to emit text.
    let tracker = MonthlyBudgetTracker::new(db);
    if !tracker.should_announce(chrono::Utc::now(), bucket) {
        return None;
    }
    Some(build_budget_nudge(bucket, total, limit))
}

fn build_budget_nudge(bucket: u8, total: u64, limit: u64) -> String {
    format!(
        "Monthly token budget check — these are real DB rows, not estimates. \
         You're at {bucket}% of this month's token budget ({total} of {limit} tokens). \
         Mention this concisely; do not fabricate dollar figures or percentages \
         beyond what's listed. If you have a concrete suggestion for reducing \
         usage (smaller model for routine work, fewer tool calls per turn, \
         summarizing long context), share at most one. Keep it to 2 sentences \
         and don't nag — this nudge is rate-limited but the user still notices."
    )
}

/// IDENTITY.md hasn't been personalized — direct the agent to ask the
/// user for the missing pieces and write a real identity file.
///
/// Uses `cortex::identity::is_personalized` as the single source of
/// truth so this check matches what the agent loop actually loads. The
/// directive mixes structured `request_user_input` choices with
/// free-form chat follow-up because `request_user_input` is
/// multiple-choice only (max 3 questions, 2-4 options each).
fn check_identity_unpersonalized(_config: &Config, _db: Option<&Database>) -> Option<String> {
    if cortex::identity::is_personalized() {
        return None;
    }
    Some(IDENTITY_NUDGE.to_string())
}

const IDENTITY_NUDGE: &str = "\
Your IDENTITY.md is still the unfilled scaffold (~/.borg/IDENTITY.md), \
so you have no personality, no owner name, and no sense of what \
matters to this user. In your reply, briefly say why personalization \
helps (one sentence), then call `request_user_input` with up to 3 \
multiple-choice questions: communication style (Terse / Warm / \
Professional / Playful), top priority area (Work / Personal / Both), \
and preferred check-in window (Morning / Midday / Evening / Quiet). \
After they answer, ask one open follow-up in chat for their name and \
a one-line personality sketch, then call `write_memory` with \
scope=`global` name=`IDENTITY` to persist a personalized identity \
filled from the template structure. Don't re-raise this if you've \
already asked recently.";

/// Gateway has been abandoned by the orchestrator's respawn supervisor
/// AND the daily self-healing recovery step also failed. Last-resort
/// nudge — outbound channel sends still work (native SDKs bypass the
/// gateway), but inbound webhooks are dead, so the user can't reply
/// through chat.
///
/// Two guards: meta flag must be set, AND it must have been set at
/// least `GATEWAY_NUDGE_GRACE_SECS` ago, so we don't race the daily
/// 02:00 maintenance pass.
pub(crate) fn check_gateway_unrecoverable(
    _config: &Config,
    db: Option<&Database>,
) -> Option<String> {
    let db = db?;
    let flag = db.get_meta(GATEWAY_UNRECOVERABLE_KEY).ok().flatten()?;
    if flag != "1" {
        return None;
    }
    // Fall back to `now` (not 0) when the timestamp is missing or
    // unparseable — a `flag="1"` with no `_at` means we caught the
    // orchestrator mid-write or somebody hand-edited the meta table.
    // Either way, treating it as "set just now" makes the grace window
    // re-arm cleanly instead of firing instantly.
    let now = unix_now() as i64;
    let set_at: i64 = db
        .get_meta(GATEWAY_UNRECOVERABLE_AT_KEY)
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(now);
    if now.saturating_sub(set_at) < GATEWAY_NUDGE_GRACE_SECS {
        return None;
    }
    Some(GATEWAY_NUDGE.to_string())
}

pub(crate) const GATEWAY_UNRECOVERABLE_KEY: &str = "gateway_unrecoverable";
pub(crate) const GATEWAY_UNRECOVERABLE_AT_KEY: &str = "gateway_unrecoverable_at";
const GATEWAY_NUDGE_GRACE_SECS: i64 = 60 * 60;

const GATEWAY_NUDGE: &str = "\
Gateway has been unrecoverable for over an hour — the supervisor \
abandoned respawning after repeated crashes, and the daily \
self-healing recovery step couldn't fix it either. Inbound channel \
messages (Telegram, Slack, Discord webhooks) are not reaching you; \
outbound replies still work via the native SDKs. Tell the user \
clearly and calmly, point them at `~/.borg/logs/daemon.err` for the \
panic, and suggest restarting the daemon (`borg service restart`, or \
`borg daemon` if running it directly) once they've captured the log. \
Keep it to 2-3 sentences — they can't message you back through chat \
right now.";

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
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
        prev_data_dir: Option<String>,
        _tmp: Option<tempfile::TempDir>,
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
            Self {
                saved,
                prev_data_dir: None,
                _tmp: None,
                _lock: lock,
            }
        }

        /// Variant that ALSO repoints `BORG_DATA_DIR` at a fresh tempdir,
        /// so file-based checks (e.g. `cortex::identity::is_personalized`)
        /// don't see the developer's real `~/.borg`. One lock acquisition
        /// covers both — calling `clear_native()` then a separate
        /// data-dir guard would re-enter the env_lock and deadlock.
        fn clear_native_with_isolated_data_dir() -> Self {
            let mut guard = Self::clear_native();
            let tmp = tempfile::tempdir().expect("tempdir");
            guard.prev_data_dir = std::env::var("BORG_DATA_DIR").ok();
            std::env::set_var("BORG_DATA_DIR", tmp.path());
            guard._tmp = Some(tmp);
            guard
        }
    }

    #[test]
    fn collect_session_start_includes_no_channels_nudge() {
        let _env = EnvGuard::clear_native();
        let config = Config::default();
        let nudges = collect_session_start(&config, None);
        assert!(
            nudges
                .iter()
                .any(|n| n.contains("No messaging channels are configured yet")),
            "expected no-channels nudge, got: {nudges:?}"
        );
    }

    #[test]
    fn collect_session_start_excludes_security_nudge() {
        let _env = EnvGuard::clear_native();
        let mut config = Config::default();
        config.security.host_audit = true;
        let nudges = collect_session_start(&config, None);
        assert!(
            nudges
                .iter()
                .all(|n| !n.contains("Critical host security issues")),
            "session-start should stay focused, got: {nudges:?}"
        );
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
            if self._tmp.is_some() {
                match &self.prev_data_dir {
                    Some(v) => std::env::set_var("BORG_DATA_DIR", v),
                    None => std::env::remove_var("BORG_DATA_DIR"),
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
        assert!(check_no_channels(&cfg, None).is_none());
        std::env::remove_var("TELEGRAM_BOT_TOKEN");
    }

    #[test]
    fn check_no_channels_returns_some_when_no_channels() {
        let _guard = EnvGuard::clear_native();
        let cfg = Config::default();
        // Skip if the OS keychain has real channel credentials (dev machine)
        if cfg.has_any_native_channel() {
            return;
        }
        let snippet = check_no_channels(&cfg, None).expect("nudge should fire");
        assert!(snippet.contains("/plugins"));
        assert!(snippet.contains("borg plugins"));
    }

    /// Build a Config with host_audit disabled so only the channel
    /// augmenter fires — isolates channel tests from host state.
    fn channel_only_config() -> Config {
        let mut cfg = Config::default();
        cfg.security.host_audit = false;
        cfg
    }

    /// Count nudges from a single augmenter by matching a stable substring
    /// in its directive. Used by collect-level tests so they stay narrowly
    /// scoped to the augmenter under test even when other augmenters
    /// happen to fire (e.g. `identity_unpersonalized` on a test box with
    /// no IDENTITY.md).
    fn count_no_channels(nudges: &[String]) -> usize {
        nudges
            .iter()
            .filter(|n| n.contains("No messaging channels are configured yet"))
            .count()
    }

    #[test]
    fn collect_fires_once_then_respects_cooldown() {
        let _guard = EnvGuard::clear_native_with_isolated_data_dir();
        let cfg = channel_only_config();
        // Skip if OS keychain has real channel credentials
        if cfg.has_any_native_channel() {
            return;
        }
        let db = in_memory_db();

        let first = collect(&cfg, Some(&db));
        assert_eq!(
            count_no_channels(&first),
            1,
            "expected no-channels nudge on first call, got: {first:?}"
        );

        let second = collect(&cfg, Some(&db));
        assert_eq!(
            count_no_channels(&second),
            0,
            "expected cooldown to suppress no-channels nudge on second call, got: {second:?}"
        );
    }

    #[test]
    fn collect_fires_again_after_cooldown_elapses() {
        let _guard = EnvGuard::clear_native_with_isolated_data_dir();
        let cfg = channel_only_config();
        // Skip if OS keychain has real channel credentials
        if cfg.has_any_native_channel() {
            return;
        }
        let db = in_memory_db();

        // First firing persists a timestamp.
        assert_eq!(count_no_channels(&collect(&cfg, Some(&db))), 1);

        // Rewind the stored timestamp to simulate cooldown elapsing.
        let stale = unix_now().saturating_sub(25 * 60 * 60);
        db.set_meta(&meta_key_for("no_channels_configured"), &stale.to_string())
            .unwrap();

        let again = collect(&cfg, Some(&db));
        assert_eq!(
            count_no_channels(&again),
            1,
            "expected no-channels nudge to re-fire after cooldown, got: {again:?}"
        );
    }

    #[test]
    fn collect_without_db_always_returns_firing_checks() {
        let _guard = EnvGuard::clear_native_with_isolated_data_dir();
        let cfg = channel_only_config();
        // Skip if OS keychain has real channel credentials
        if cfg.has_any_native_channel() {
            return;
        }
        // No DB → no cooldown enforcement; the no-channels snippet is
        // returned on every call.
        assert_eq!(count_no_channels(&collect(&cfg, None)), 1);
        assert_eq!(count_no_channels(&collect(&cfg, None)), 1);
    }

    #[test]
    fn collect_silent_when_channel_configured() {
        let _guard = EnvGuard::clear_native_with_isolated_data_dir();
        std::env::set_var("SLACK_BOT_TOKEN", "xoxb-test");
        let cfg = channel_only_config();
        let db = in_memory_db();
        assert_eq!(count_no_channels(&collect(&cfg, Some(&db))), 0);
        std::env::remove_var("SLACK_BOT_TOKEN");
    }

    #[test]
    fn collect_skips_augmenter_on_db_read_error() {
        let _guard = EnvGuard::clear_native();
        let cfg = channel_only_config();
        let db = in_memory_db();

        // Drop the meta table to make get_meta fail
        db.conn()
            .execute_batch("DROP TABLE IF EXISTS meta")
            .unwrap();

        // Should skip augmenters (not spam) rather than treat as never-fired
        let result = collect(&cfg, Some(&db));
        assert!(
            result.is_empty(),
            "expected no augmenters to fire when DB reads fail, got {result:?}"
        );
    }

    // ── Host security augmenter tests ──

    use cortex::doctor::DiagnosticCheck;

    #[test]
    fn check_critical_host_security_respects_config_toggle() {
        let _guard = EnvGuard::clear_native();
        let mut cfg = Config::default();
        cfg.security.host_audit = false;
        assert!(
            check_critical_host_security(&cfg, None).is_none(),
            "should return None when host_audit is disabled"
        );
    }

    #[test]
    fn check_critical_host_security_runs_when_enabled() {
        let _guard = EnvGuard::clear_native();
        let mut cfg = Config::default();
        cfg.security.host_audit = true;
        let result = check_critical_host_security(&cfg, None);
        if let Some(ref snippet) = result {
            assert!(snippet.contains("Critical host security"));
            // Trailer must point the user at the actionable hint baked
            // into each finding, and at the squelch escape hatch — losing
            // either drops the user back into vague-reminder territory.
            assert!(snippet.contains("next step"));
            assert!(snippet.contains("squelch"));
        }
    }

    // ── build_security_nudge unit tests (synthetic DiagnosticCheck data) ──

    #[test]
    fn build_security_nudge_returns_none_when_all_pass() {
        let checks = vec![
            DiagnosticCheck::pass("Host Security", "Application Firewall"),
            DiagnosticCheck::pass("Host Security", "risky listening ports"),
            DiagnosticCheck::pass("Host Security", "SSH daemon config"),
        ];
        assert!(build_security_nudge(&checks).is_none());
    }

    #[test]
    fn build_security_nudge_returns_none_on_empty_checks() {
        assert!(build_security_nudge(&[]).is_none());
    }

    #[test]
    fn build_security_nudge_includes_warn_issues() {
        let checks = vec![
            DiagnosticCheck::pass("Host Security", "SSH daemon config"),
            DiagnosticCheck::warn(
                "Host Security",
                "Application Firewall",
                "Application Firewall is disabled",
            ),
            DiagnosticCheck::warn(
                "Host Security",
                "port 23 (Telnet) listening",
                "risky port open on all interfaces",
            ),
        ];
        let nudge = build_security_nudge(&checks).expect("should fire with warnings");
        assert!(nudge.contains("Critical host security issues detected"));
        assert!(nudge.contains("Application Firewall is disabled"));
        assert!(nudge.contains("risky port open on all interfaces"));
        assert!(nudge.contains("squelch"));
        // Pass items should NOT appear in the nudge text
        assert!(!nudge.contains("SSH daemon config"));
    }

    #[test]
    fn build_security_nudge_includes_fail_issues() {
        let checks = vec![DiagnosticCheck::fail(
            "Host Security",
            "firewall",
            "no firewall detected",
        )];
        let nudge = build_security_nudge(&checks).expect("should fire with failures");
        assert!(nudge.contains("no firewall detected"));
        assert!(nudge.contains("next step"));
    }

    #[test]
    fn build_security_nudge_mixed_pass_warn_fail() {
        let checks = vec![
            DiagnosticCheck::pass("Host Security", "risky listening ports"),
            DiagnosticCheck::warn(
                "Host Security",
                "SSH: PasswordAuthentication",
                "PasswordAuthentication is enabled",
            ),
            DiagnosticCheck::fail("Host Security", "PF packet filter", "PF is disabled"),
        ];
        let nudge = build_security_nudge(&checks).expect("should fire");
        assert!(nudge.contains("PasswordAuthentication is enabled"));
        assert!(nudge.contains("PF is disabled"));
        assert_eq!(nudge.matches("  - ").count(), 2);
    }

    #[test]
    fn build_security_nudge_single_issue_format() {
        let checks = vec![DiagnosticCheck::warn(
            "Host Security",
            "FileVault disk encryption",
            "FileVault is disabled",
        )];
        let nudge = build_security_nudge(&checks).expect("should fire");
        assert!(nudge.starts_with("Critical host security issues detected"));
        assert!(nudge.contains("  - FileVault is disabled\n"));
        assert!(nudge.ends_with("Keep it brief."));
    }

    #[test]
    fn squelching_alf_drops_it_from_the_security_nudge() {
        // Regression: a squelched (category, name) pair must not surface
        // through `check_critical_host_security` even if the underlying
        // host check still flags it. We can't force ALF off in tests, but
        // we can pre-populate the squelch table so any ALF finding is
        // filtered, then verify the augmenter's nudge doesn't mention it.
        let _guard = EnvGuard::clear_native();
        let mut cfg = Config::default();
        cfg.security.host_audit = true;
        let db = in_memory_db();

        let now = unix_now() as i64;
        db.doctor_squelch_set(
            "Host Security",
            "Application Firewall",
            now,
            now + 60_000,
            Some("test-ack"),
        )
        .unwrap();

        if let Some(snippet) = check_critical_host_security(&cfg, Some(&db)) {
            assert!(
                !snippet.contains("Application Firewall"),
                "squelched finding leaked into nudge: {snippet}"
            );
        }
    }

    #[test]
    fn host_security_augmenter_cooldown_works() {
        let _guard = EnvGuard::clear_native();
        let mut cfg = Config::default();
        cfg.security.host_audit = true;
        let db = in_memory_db();

        let first = collect(&cfg, Some(&db));
        let has_security_nudge = first.iter().any(|s| s.contains("Critical host security"));

        if has_security_nudge {
            let second = collect(&cfg, Some(&db));
            assert!(
                !second.iter().any(|s| s.contains("Critical host security")),
                "security nudge should be suppressed by cooldown on second call"
            );
        }
    }

    // ── task_health augmenter tests ──
    //
    // Regression context: heartbeat once produced "5 failed task runs sitting
    // in the queue" with zero DB grounding. These tests pin the augmenter to
    // real DB state (positive: surfaces failures with their names; negative:
    // stays silent when there are none; structural: no fabricated numbers).

    use cortex::db::NewTask;

    fn make_task(db: &Database, id: &str, name: &str) {
        db.create_task(&NewTask {
            id,
            name,
            prompt: "noop",
            schedule_type: "interval",
            schedule_expr: "30m",
            timezone: "UTC",
            next_run: Some(0),
            max_retries: Some(3),
            timeout_ms: None,
            delivery_channel: None,
            delivery_target: None,
            allowed_tools: None,
            task_type: "prompt",
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
        })
        .expect("create task");
    }

    #[test]
    fn task_health_returns_none_when_no_failures() {
        let db = in_memory_db();
        make_task(&db, "t1", "healthy");
        // Default seeded tasks (Daily Summary, etc.) have no last_error
        // and no run history — augmenter must stay silent.
        assert!(
            build_task_health_nudge(&db, 1_000_000).is_none(),
            "augmenter must not fire on a clean DB"
        );
    }

    #[test]
    fn task_health_returns_none_without_db() {
        // No DB → cannot ground claims → must stay silent. This is the core
        // regression guard against the original "5 failed task runs"
        // hallucination: we never fire without source-of-truth.
        let cfg = Config::default();
        assert!(check_task_health(&cfg, None).is_none());
    }

    #[test]
    fn task_health_surfaces_tasks_with_last_error() {
        let db = in_memory_db();
        make_task(&db, "t1", "daily_summary");
        make_task(&db, "t2", "weekly_report");
        db.set_task_retry("t1", 1, "openrouter 429", 9999)
            .expect("set t1 error");
        db.set_task_retry("t2", 1, "timeout", 9999)
            .expect("set t2 error");

        let nudge =
            build_task_health_nudge(&db, 1_000_000).expect("must fire when tasks carry last_error");
        // Names verbatim (so the LLM can't freelance) — both must appear.
        assert!(
            nudge.contains("daily_summary"),
            "missing daily_summary in nudge: {nudge}"
        );
        assert!(
            nudge.contains("weekly_report"),
            "missing weekly_report in nudge: {nudge}"
        );
        // Errors verbatim, so the agent can answer "what went wrong?".
        assert!(nudge.contains("openrouter 429"), "missing error: {nudge}");
        assert!(nudge.contains("timeout"), "missing error: {nudge}");
        // Real-count line.
        assert!(
            nudge.contains("2 task(s) carry a last_error"),
            "expected count line, got: {nudge}"
        );
        // Anti-hallucination: directive forbids fabricated counts.
        assert!(
            nudge.contains("do not fabricate"),
            "directive must instruct LLM not to fabricate: {nudge}"
        );
    }

    #[test]
    fn task_health_counts_failed_runs_in_window() {
        let db = in_memory_db();
        make_task(&db, "t1", "task1");

        let now = 1_000_000_i64;
        // Within window (last 24h)
        db.record_task_run("t1", now - 60, 100, None, Some("e1"))
            .expect("record");
        db.record_task_run("t1", now - 120, 100, None, Some("e2"))
            .expect("record");
        // Outside window
        db.record_task_run("t1", now - (48 * 60 * 60), 100, None, Some("ancient"))
            .expect("record");
        // Successes — must not be counted as failures
        db.record_task_run("t1", now - 30, 100, Some("ok"), None)
            .expect("record");

        let nudge = build_task_health_nudge(&db, now).expect("must fire");
        assert!(
            nudge.contains("2 task_run(s) failed in the last 24h"),
            "expected 24h failed-run count = 2, got: {nudge}"
        );
        // The whole point of the fix: the windowed block must name the task
        // by name + id so the agent has concrete IDs to investigate, instead
        // of asking the user "what are the task IDs?".
        assert!(
            nudge.contains("task1") && nudge.contains("id=t1"),
            "windowed block must name the task and include its id: {nudge}"
        );
        // Negative: never claim "5" or any other fabricated number.
        assert!(!nudge.contains(" 5 "), "fabricated count leaked: {nudge}");
    }

    #[test]
    fn task_health_lists_windowed_failures_when_last_error_already_cleared() {
        // Regression for the bug behind this commit: the heartbeat once
        // produced "2 task runs have failed in the last 24 hours. Want me to
        // investigate? ... if you share the task IDs". The agent had no IDs
        // because `tasks_with_last_error` was empty (failures cleared by a
        // later success) and the recent-failed branch was just a count.
        // After the fix, the nudge must list task IDs even in that case.
        let db = in_memory_db();
        make_task(&db, "tsk_alpha", "daily_summary");
        make_task(&db, "tsk_beta", "weekly_report");

        let now = 1_000_000_i64;
        // Two failures inside the window for daily_summary, one for weekly_report.
        db.record_task_run("tsk_alpha", now - 60, 100, None, Some("openrouter 429"))
            .expect("record");
        db.record_task_run("tsk_alpha", now - 120, 100, None, Some("upstream timeout"))
            .expect("record");
        db.record_task_run("tsk_beta", now - 90, 100, None, Some("rate limited"))
            .expect("record");
        // Successes after — important: simulates last_error being cleared.
        db.record_task_run("tsk_alpha", now - 30, 100, Some("ok"), None)
            .expect("record");
        // No `set_task_retry` — `tasks_with_last_error()` is empty.

        let nudge = build_task_health_nudge(&db, now).expect("must fire");
        assert!(
            !nudge.contains("task(s) carry a last_error"),
            "first block should be skipped when no task carries last_error: {nudge}"
        );
        // Both task IDs must appear so the agent doesn't ask the user for them.
        assert!(
            nudge.contains("id=tsk_alpha"),
            "missing tsk_alpha id: {nudge}"
        );
        assert!(
            nudge.contains("id=tsk_beta"),
            "missing tsk_beta id: {nudge}"
        );
        // Per-task failure counts are visible.
        assert!(
            nudge.contains("daily_summary") && nudge.contains("2 failure(s)"),
            "expected daily_summary listed with 2 failures: {nudge}"
        );
        assert!(
            nudge.contains("weekly_report") && nudge.contains("1 failure(s)"),
            "expected weekly_report listed with 1 failure: {nudge}"
        );
        // Trailer explicitly tells the LLM not to ask the user for IDs.
        assert!(
            nudge.contains("do NOT ask the user for IDs"),
            "trailer must forbid asking the user for IDs: {nudge}"
        );
    }

    #[test]
    fn task_health_dedupes_tasks_appearing_in_both_blocks() {
        // A task that currently carries `last_error` AND has windowed failed
        // runs should appear once (in the last_error block), not twice.
        let db = in_memory_db();
        make_task(&db, "tsk_dup", "duplicated");

        let now = 1_000_000_i64;
        db.record_task_run("tsk_dup", now - 60, 100, None, Some("boom"))
            .expect("record");
        db.set_task_retry("tsk_dup", 1, "still broken", now + 10_000)
            .expect("set");

        let nudge = build_task_health_nudge(&db, now).expect("must fire");
        assert_eq!(
            nudge.matches("id=tsk_dup").count(),
            1,
            "task should be listed exactly once across both blocks: {nudge}"
        );
        // The windowed block still appears (with marker) so the count line
        // doesn't look unmoored.
        assert!(
            nudge.contains("1 task_run(s) failed in the last 24h"),
            "windowed count line must still appear: {nudge}"
        );
        assert!(
            nudge.contains("(all already listed above)"),
            "marker should explain why the windowed block lists nothing: {nudge}"
        );
    }

    #[test]
    fn task_health_truncates_long_failure_list() {
        let db = in_memory_db();
        for i in 0..7 {
            let id = format!("t{i}");
            let name = format!("task_{i}");
            make_task(&db, &id, &name);
            db.set_task_retry(&id, 1, "boom", 9999).expect("set error");
        }
        let nudge = build_task_health_nudge(&db, 1_000_000).expect("must fire");
        // Cap is 5 listed + a "… and N more (truncated)" line.
        assert!(
            nudge.contains("… and 2 more (truncated)"),
            "expected truncation marker, got: {nudge}"
        );
        // Header must reflect the real total, not the truncated count.
        assert!(
            nudge.contains("7 task(s) carry a last_error"),
            "header should report real total = 7: {nudge}"
        );
    }

    #[test]
    fn task_health_size_stays_bounded_under_worst_case() {
        // Bloat guard: every heartbeat tick that fires this nudge ships its
        // text inside <proactive_nudges> straight to the LLM. Cap = 5 listed
        // tasks at 120-char trimmed errors; everything else truncates. Pin
        // 2KB as the upper bound so a future field addition trips this test.
        let db = in_memory_db();
        for i in 0..20 {
            let id = format!("id-{i}");
            let name = format!("task_with_a_reasonably_long_name_{i}");
            make_task(&db, &id, &name);
            db.set_task_retry(&id, 1, &"x".repeat(500), 9999)
                .expect("set");
        }
        let nudge = build_task_health_nudge(&db, 1_000_000).expect("must fire");
        assert!(
            nudge.len() < 2048,
            "nudge must stay under 2KB even with many long-error tasks; got {} bytes:\n{nudge}",
            nudge.len()
        );
    }

    #[test]
    fn task_health_collect_respects_cooldown() {
        let _guard = EnvGuard::clear_native();
        let cfg = channel_only_config();
        // Skip if OS keychain has real channel credentials (no_channels would also fire)
        if cfg.has_any_native_channel() {
            return;
        }
        let db = in_memory_db();
        make_task(&db, "t1", "broken");
        db.set_task_retry("t1", 1, "boom", 9999).expect("set");

        let first = collect(&cfg, Some(&db));
        assert!(
            first.iter().any(|s| s.contains("broken")),
            "first call should include task_health nudge: {first:?}"
        );
        let second = collect(&cfg, Some(&db));
        assert!(
            !second.iter().any(|s| s.contains("broken")),
            "cooldown should suppress task_health on second call: {second:?}"
        );
    }

    #[test]
    fn host_security_cooldown_resets_after_48h() {
        let _guard = EnvGuard::clear_native();
        let mut cfg = Config::default();
        cfg.security.host_audit = true;
        let db = in_memory_db();

        let _ = collect(&cfg, Some(&db));

        // Simulate 49h elapsed for the security augmenter.
        let stale = unix_now().saturating_sub(49 * 60 * 60);
        db.set_meta(&meta_key_for("critical_host_security"), &stale.to_string())
            .unwrap();

        let result = collect(&cfg, Some(&db));
        let has_security = result.iter().any(|s| s.contains("Critical host security"));

        // If it fired, cooldown must now suppress it again.
        if has_security {
            let again = collect(&cfg, Some(&db));
            assert!(
                !again.iter().any(|s| s.contains("Critical host security")),
                "security nudge should be suppressed after re-firing"
            );
        }
    }

    // ── budget_threshold augmenter tests ──
    //
    // These pin the augmenter to real `token_usage` rows + `meta` per-month
    // bucket state, mirroring the anti-hallucination shape of `task_health`.

    fn budget_only_config(limit: u64) -> Config {
        let mut cfg = Config::default();
        cfg.security.host_audit = false;
        cfg.budget.monthly_token_limit = limit;
        cfg
    }

    #[test]
    fn budget_threshold_returns_none_without_db() {
        // No DB → can't ground numbers → must stay silent. Same regression
        // shape as task_health.
        let cfg = budget_only_config(1000);
        assert!(check_budget_threshold(&cfg, None).is_none());
    }

    #[test]
    fn budget_threshold_silent_when_unlimited() {
        let _guard = EnvGuard::clear_native();
        let cfg = budget_only_config(0);
        let db = in_memory_db();
        db.log_token_usage(10_000_000, 0, 10_000_000, "openai", "gpt-4o", None)
            .unwrap();
        assert!(check_budget_threshold(&cfg, Some(&db)).is_none());
    }

    #[test]
    fn budget_threshold_silent_below_75pct() {
        let _guard = EnvGuard::clear_native();
        let cfg = budget_only_config(1000);
        let db = in_memory_db();
        db.log_token_usage(740, 0, 740, "openai", "gpt-4o", None)
            .unwrap();
        assert!(check_budget_threshold(&cfg, Some(&db)).is_none());
    }

    #[test]
    fn budget_threshold_fires_at_75pct_with_real_numbers() {
        let _guard = EnvGuard::clear_native();
        let cfg = budget_only_config(1000);
        let db = in_memory_db();
        db.log_token_usage(800, 0, 800, "openai", "gpt-4o", None)
            .unwrap();
        let nudge = check_budget_threshold(&cfg, Some(&db)).expect("must fire at 80%");
        // Anti-hallucination: bucketed pct, total, and limit must all be present.
        assert!(nudge.contains("75%"), "missing bucket 75%: {nudge}");
        assert!(nudge.contains("800"), "missing total tokens: {nudge}");
        assert!(nudge.contains("1000"), "missing limit: {nudge}");
        assert!(
            nudge.contains("do not fabricate"),
            "directive must forbid fabrication: {nudge}"
        );
    }

    #[test]
    fn budget_threshold_does_not_refire_at_same_bucket_within_month() {
        let _guard = EnvGuard::clear_native();
        let cfg = budget_only_config(1000);
        let db = in_memory_db();
        db.log_token_usage(800, 0, 800, "openai", "gpt-4o", None)
            .unwrap();
        assert!(check_budget_threshold(&cfg, Some(&db)).is_some());
        // Same bucket on the next tick → silent. The persisted month-bucket
        // is the regression guard: without it, the user sees the same 75%
        // nag every cooldown window.
        assert!(check_budget_threshold(&cfg, Some(&db)).is_none());
    }

    #[test]
    fn budget_threshold_refires_when_bucket_increases() {
        // Crossing 75 → 90 must be allowed even within the same month —
        // otherwise the user only ever hears about the first threshold and
        // misses the more urgent ones.
        let _guard = EnvGuard::clear_native();
        let cfg = budget_only_config(1000);
        let db = in_memory_db();
        db.log_token_usage(800, 0, 800, "openai", "gpt-4o", None)
            .unwrap();
        let first = check_budget_threshold(&cfg, Some(&db)).expect("75% must fire");
        assert!(first.contains("75%"));

        db.log_token_usage(150, 0, 150, "openai", "gpt-4o", None)
            .unwrap();
        let second = check_budget_threshold(&cfg, Some(&db)).expect("90% must fire");
        assert!(second.contains("90%"), "expected 90% bucket: {second}");

        db.log_token_usage(100, 0, 100, "openai", "gpt-4o", None)
            .unwrap();
        let third = check_budget_threshold(&cfg, Some(&db)).expect("100% must fire");
        assert!(third.contains("100%"), "expected 100% bucket: {third}");
    }

    #[test]
    fn budget_threshold_resets_on_new_month_key() {
        // Simulate a previous month's bucket persisted under last month's
        // key. A fresh month sees an empty key and fires from scratch.
        let _guard = EnvGuard::clear_native();
        let cfg = budget_only_config(1000);
        let db = in_memory_db();
        db.set_meta("heartbeat_budget_last_bucket_2099-01", "100")
            .unwrap();
        db.log_token_usage(800, 0, 800, "openai", "gpt-4o", None)
            .unwrap();
        let nudge =
            check_budget_threshold(&cfg, Some(&db)).expect("must fire — this month's key is empty");
        assert!(nudge.contains("75%"));
    }

    #[test]
    fn budget_nudge_size_stays_bounded() {
        // The directive ships verbatim into <proactive_nudges>. Bake in an
        // upper bound so a future field addition trips this test rather
        // than silently bloating every heartbeat tick.
        let nudge = build_budget_nudge(100, u64::MAX, u64::MAX);
        assert!(
            nudge.len() < 1024,
            "budget nudge should stay under 1KB; got {} bytes:\n{nudge}",
            nudge.len()
        );
    }

    #[test]
    fn budget_collect_respects_cooldown() {
        let _guard = EnvGuard::clear_native();
        let mut cfg = budget_only_config(1000);
        // Skip if OS keychain has real channel credentials (no_channels would also fire)
        if cfg.has_any_native_channel() {
            return;
        }
        cfg.security.host_audit = false;
        let db = in_memory_db();
        db.log_token_usage(800, 0, 800, "openai", "gpt-4o", None)
            .unwrap();

        let first = collect(&cfg, Some(&db));
        assert!(
            first.iter().any(|s| s.contains("75%")),
            "first call should include budget nudge: {first:?}"
        );
        let second = collect(&cfg, Some(&db));
        assert!(
            !second.iter().any(|s| s.contains("75%")),
            "cooldown should suppress budget nudge on second call: {second:?}"
        );
    }

    // ── identity_unpersonalized augmenter tests ──
    //
    // These exercise `is_personalized_content` indirectly via the augmenter.
    // The pure helper is also unit-tested in cortex/src/identity.rs.

    #[test]
    fn identity_directive_mentions_required_tools() {
        // Regression guard: the LLM follow-up depends on these exact tool
        // names. If a future refactor renames or drops them from the
        // directive the agent will silently stop completing the flow.
        assert!(IDENTITY_NUDGE.contains("request_user_input"));
        assert!(IDENTITY_NUDGE.contains("write_memory"));
        assert!(IDENTITY_NUDGE.contains("IDENTITY"));
    }

    #[test]
    fn identity_directive_offers_structured_questions() {
        // The `request_user_input` tool is multiple-choice only. The
        // directive must seed concrete option labels so the LLM produces
        // a valid tool call instead of inventing free-form prompts.
        assert!(IDENTITY_NUDGE.contains("Terse"));
        assert!(IDENTITY_NUDGE.contains("Warm"));
        assert!(IDENTITY_NUDGE.contains("Work"));
    }

    #[test]
    fn identity_nudge_size_stays_bounded() {
        assert!(
            IDENTITY_NUDGE.len() < 1024,
            "identity nudge should stay under 1KB; got {} bytes",
            IDENTITY_NUDGE.len()
        );
    }

    #[test]
    fn identity_unpersonalized_fires_when_file_missing() {
        let _g = EnvGuard::clear_native_with_isolated_data_dir();
        // No file at all → unpersonalized.
        let cfg = Config::default();
        let nudge = check_identity_unpersonalized(&cfg, None)
            .expect("must fire when IDENTITY.md is missing");
        assert!(nudge.contains("IDENTITY.md"));
    }

    #[test]
    fn identity_unpersonalized_fires_on_raw_template() {
        let _g = EnvGuard::clear_native_with_isolated_data_dir();
        let template = include_str!("../../interface/templates/IDENTITY.md");
        cortex::identity::save_identity(template).expect("write template");
        let cfg = Config::default();
        assert!(check_identity_unpersonalized(&cfg, None).is_some());
    }

    #[test]
    fn identity_unpersonalized_silent_when_personalized() {
        let _g = EnvGuard::clear_native_with_isolated_data_dir();
        cortex::identity::save_identity(
            "# Borg\n\nYou are Borg, an AI personal assistant belonging to Mike.\n\n## Personality\nWry, direct.\n\n## Communication Style\nTerse.\n\n## What Matters\nShipping.\n",
        )
        .expect("write filled");
        let cfg = Config::default();
        assert!(check_identity_unpersonalized(&cfg, None).is_none());
    }

    // ── gateway_unrecoverable augmenter tests ──

    #[test]
    fn gateway_silent_without_db() {
        let cfg = Config::default();
        assert!(check_gateway_unrecoverable(&cfg, None).is_none());
    }

    #[test]
    fn gateway_silent_when_meta_unset() {
        let cfg = Config::default();
        let db = in_memory_db();
        assert!(check_gateway_unrecoverable(&cfg, Some(&db)).is_none());
    }

    #[test]
    fn gateway_silent_within_one_hour_grace() {
        // The grace window is the whole point of layering this augmenter
        // *behind* the daily 02:00 self-healing pass — without it, the
        // augmenter would race maintenance and nag during the recovery
        // attempt itself.
        let cfg = Config::default();
        let db = in_memory_db();
        let now = unix_now() as i64;
        db.set_meta(GATEWAY_UNRECOVERABLE_KEY, "1").unwrap();
        db.set_meta(GATEWAY_UNRECOVERABLE_AT_KEY, &(now - 30).to_string())
            .unwrap();
        assert!(check_gateway_unrecoverable(&cfg, Some(&db)).is_none());
    }

    #[test]
    fn gateway_fires_after_grace() {
        let cfg = Config::default();
        let db = in_memory_db();
        let now = unix_now() as i64;
        db.set_meta(GATEWAY_UNRECOVERABLE_KEY, "1").unwrap();
        db.set_meta(
            GATEWAY_UNRECOVERABLE_AT_KEY,
            &(now - 2 * 60 * 60).to_string(),
        )
        .unwrap();
        let nudge =
            check_gateway_unrecoverable(&cfg, Some(&db)).expect("must fire after grace elapsed");
        // Directive must mention the log path + restart command — those
        // are the user's actual escape hatches when chat is one-way.
        assert!(nudge.contains("daemon.err"), "missing log path: {nudge}");
        assert!(nudge.contains("restart"), "missing restart hint: {nudge}");
        assert!(
            nudge.contains("outbound"),
            "must explain the channel asymmetry: {nudge}"
        );
    }

    #[test]
    fn gateway_silent_when_flag_cleared() {
        // A successful recovery clears the flag — the augmenter must
        // observe this immediately, even if `_at` is still set.
        let cfg = Config::default();
        let db = in_memory_db();
        let now = unix_now() as i64;
        db.set_meta(GATEWAY_UNRECOVERABLE_KEY, "0").unwrap();
        db.set_meta(
            GATEWAY_UNRECOVERABLE_AT_KEY,
            &(now - 2 * 60 * 60).to_string(),
        )
        .unwrap();
        assert!(check_gateway_unrecoverable(&cfg, Some(&db)).is_none());
    }

    #[test]
    fn gateway_collect_respects_cooldown() {
        let _guard = EnvGuard::clear_native();
        let mut cfg = Config::default();
        cfg.security.host_audit = false;
        if cfg.has_any_native_channel() {
            return;
        }
        let db = in_memory_db();
        let now = unix_now() as i64;
        db.set_meta(GATEWAY_UNRECOVERABLE_KEY, "1").unwrap();
        db.set_meta(
            GATEWAY_UNRECOVERABLE_AT_KEY,
            &(now - 2 * 60 * 60).to_string(),
        )
        .unwrap();
        let first = collect(&cfg, Some(&db));
        assert!(
            first.iter().any(|s| s.contains("Gateway")),
            "first tick should include gateway nudge: {first:?}"
        );
        let second = collect(&cfg, Some(&db));
        assert!(
            !second.iter().any(|s| s.contains("Gateway")),
            "cooldown should suppress on second tick: {second:?}"
        );
    }

    #[test]
    fn session_start_includes_identity_nudge() {
        // Opening the TUI with no IDENTITY.md should prompt the user to
        // personalize — that's how new users discover the flow.
        let _g = EnvGuard::clear_native_with_isolated_data_dir();
        let cfg = Config::default();
        let nudges = collect_session_start(&cfg, None);
        assert!(
            nudges.iter().any(|n| n.contains("IDENTITY.md")),
            "expected identity nudge in session-start, got: {nudges:?}"
        );
    }

    #[test]
    fn session_start_excludes_budget_and_gateway_nudges() {
        // The session-start subset must stay focused — the heavy
        // operational nudges only fire on scheduled ticks.
        let _g = EnvGuard::clear_native_with_isolated_data_dir();
        let mut cfg = budget_only_config(1000);
        cfg.security.host_audit = false;
        let db = in_memory_db();
        db.log_token_usage(800, 0, 800, "openai", "gpt-4o", None)
            .unwrap();
        let now = unix_now() as i64;
        db.set_meta(GATEWAY_UNRECOVERABLE_KEY, "1").unwrap();
        db.set_meta(
            GATEWAY_UNRECOVERABLE_AT_KEY,
            &(now - 2 * 60 * 60).to_string(),
        )
        .unwrap();
        let nudges = collect_session_start(&cfg, Some(&db));
        assert!(
            nudges
                .iter()
                .all(|n| !n.contains("75%") && !n.contains("Gateway")),
            "session-start must exclude budget/gateway, got: {nudges:?}"
        );
    }
}
