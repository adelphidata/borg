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
];

const SESSION_START_AUGMENTER_IDS: &[&str] = &["no_channels_configured"];

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
    let mut out = Vec::new();
    for aug in AUGMENTERS {
        if !include(aug) {
            continue;
        }
        let Some(snippet) = (aug.check)(config, db) else {
            continue;
        };
        if let Some(db) = db {
            let key = meta_key(aug.id);
            let last = match db.get_meta(&key) {
                Ok(Some(s)) => s.parse::<u64>().unwrap_or(0),
                Ok(None) => 0,
                Err(e) => {
                    tracing::warn!(
                        "heartbeat_augmenters: failed to read cooldown for '{}': {e}",
                        aug.id
                    );
                    continue;
                }
            };
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
    let recent_failed = match db.count_failed_runs_since(now - TASK_HEALTH_WINDOW_SECS) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("task_health: count_failed_runs_since failed: {e}");
            return None;
        }
    };

    if failing_tasks.is_empty() && recent_failed == 0 {
        return None;
    }

    let mut nudge = String::from(
        "Scheduled task health check — these are real DB rows, not estimates. \
         Mention them concisely; do not fabricate counts beyond what's listed:\n",
    );

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
            nudge.push_str(&format!("    - {} (last_error: \"{}\")\n", task.name, err));
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
            "  {recent_failed} task_run(s) have status='failed' in the last 24h.\n"
        ));
    }
    nudge.push_str(
        "Offer to investigate (run `schedule` with action='runs' for a specific task ID, \
         or look at recent task_runs). Keep it to 2–3 sentences and use the names above verbatim.",
    );
    Some(nudge)
}

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

    #[test]
    fn collect_fires_once_then_respects_cooldown() {
        let _guard = EnvGuard::clear_native();
        let cfg = channel_only_config();
        // Skip if OS keychain has real channel credentials
        if cfg.has_any_native_channel() {
            return;
        }
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
        let cfg = channel_only_config();
        // Skip if OS keychain has real channel credentials
        if cfg.has_any_native_channel() {
            return;
        }
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
        let cfg = channel_only_config();
        // Skip if OS keychain has real channel credentials
        if cfg.has_any_native_channel() {
            return;
        }
        // No DB → no cooldown enforcement; every call returns the snippet.
        assert_eq!(collect(&cfg, None).len(), 1);
        assert_eq!(collect(&cfg, None).len(), 1);
    }

    #[test]
    fn collect_silent_when_channel_configured() {
        let _guard = EnvGuard::clear_native();
        std::env::set_var("SLACK_BOT_TOKEN", "xoxb-test");
        let cfg = channel_only_config();
        let db = in_memory_db();
        assert!(collect(&cfg, Some(&db)).is_empty());
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
            nudge.contains("2 task_run(s) have status='failed' in the last 24h"),
            "expected 24h failed-run count = 2, got: {nudge}"
        );
        // Negative: never claim "5" or any other fabricated number.
        assert!(!nudge.contains(" 5 "), "fabricated count leaked: {nudge}");
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
        db.set_meta(&meta_key("critical_host_security"), &stale.to_string())
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
}
