//! Runtime/daemon-state checks: subprocesses, heartbeat, budget, prompt
//! cache, agents, orphan daemon locks, and scheduled tasks.

use anyhow::Result;

use crate::config::Config;
use crate::constants::SECS_PER_DAY;
use crate::db::Database;
use crate::doctor::{DiagnosticCheck, RemediationKind, DAEMON_LOCK_STALE_SECS};
use crate::process_registry::ProcessRegistry;

/// Subprocess registry health: surfaces `Warn`/`Fail` when one or more
/// non-pinned children have been running past the configured age
/// thresholds. Squelchable via
/// `/doctor squelch Subprocesses:Long-running children`.
pub(in crate::doctor) fn check_subprocesses(
    config: &Config,
    registry: &ProcessRegistry,
    checks: &mut Vec<DiagnosticCheck>,
) {
    let now = chrono::Utc::now().timestamp();
    let class = registry.classify(
        now,
        config.process_registry.orphan_warn_secs as i64,
        config.process_registry.orphan_fail_secs as i64,
    );
    let hint =
        "Inspect: `borg status`. Pin the row in `/status` (press `p`) or kill: `kill <pid>`.";
    if !class.failing.is_empty() {
        let msg = format_subprocess_offenders(&class.failing, now);
        let mut check = DiagnosticCheck::fail("Subprocesses", "Long-running children", msg);
        check.remediation_kind = RemediationKind::Manual;
        check.remediation_key = Some("subprocesses.long_running");
        check.user_hint = Some(hint.to_string());
        checks.push(check);
    } else if !class.warning.is_empty() {
        let msg = format_subprocess_offenders(&class.warning, now);
        let mut check = DiagnosticCheck::warn("Subprocesses", "Long-running children", msg);
        check.remediation_kind = RemediationKind::Manual;
        check.remediation_key = Some("subprocesses.long_running");
        check.user_hint = Some(hint.to_string());
        checks.push(check);
    } else {
        checks.push(DiagnosticCheck::pass(
            "Subprocesses",
            "Long-running children",
        ));
    }
}

/// Render up to 3 offenders by descending age; collapse the rest into
/// "… and N more" so a runaway registry doesn't produce a wall-of-text
/// warning in `borg doctor` or persistent warnings.
fn format_subprocess_offenders(
    entries: &[crate::process_registry::ProcessEntry],
    now: i64,
) -> String {
    // 3 names fit on one terminal line alongside the "and N more" suffix.
    const MAX_NAMED: usize = 3;
    let mut parts: Vec<String> = entries
        .iter()
        .take(MAX_NAMED)
        .map(|e| {
            let age = now.saturating_sub(e.started_at).max(0);
            format!(
                "{} pid={} age={}s ({})",
                e.kind.tag(),
                e.pid,
                age,
                e.command
            )
        })
        .collect();
    if entries.len() > MAX_NAMED {
        parts.push(format!("… and {} more", entries.len() - MAX_NAMED));
    }
    format!(
        "{} long-running child(ren): {}",
        entries.len(),
        parts.join("; ")
    )
}

/// Format a unix timestamp as a human-readable relative time string.
pub fn format_relative_time(timestamp: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let delta = now - timestamp;
    if delta < 0 {
        return "just now".to_string();
    }
    let secs = delta as u64;
    if secs < 60 {
        if secs == 1 {
            "1 second ago".to_string()
        } else {
            format!("{secs} seconds ago")
        }
    } else if secs < 3600 {
        let mins = secs / 60;
        if mins == 1 {
            "1 minute ago".to_string()
        } else {
            format!("{mins} minutes ago")
        }
    } else if secs < SECS_PER_DAY as u64 {
        let hours = secs / 3600;
        if hours == 1 {
            "1 hour ago".to_string()
        } else {
            format!("{hours} hours ago")
        }
    } else {
        let days = secs / SECS_PER_DAY as u64;
        if days == 1 {
            "1 day ago".to_string()
        } else {
            format!("{days} days ago")
        }
    }
}

pub(in crate::doctor) async fn check_heartbeat(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    checks.push(DiagnosticCheck::pass(
        "Heartbeat",
        format!("interval: {}", config.heartbeat.interval),
    ));

    match crate::db::Database::open().await {
        Ok(db) => {
            match db.query_activity(1, Some("info"), Some("heartbeat")).await {
                Ok(entries) if !entries.is_empty() => {
                    let last = &entries[0];
                    let age_secs = chrono::Utc::now().timestamp() - last.created_at;

                    // Stale threshold: 2x configured interval (default 1h for 30m interval)
                    let interval_secs = crate::tasks::parse_interval(&config.heartbeat.interval)
                        .unwrap_or(crate::constants::HEARTBEAT_FALLBACK_INTERVAL)
                        .as_secs() as i64;
                    let stale_threshold = interval_secs * 2;

                    let relative = format_relative_time(last.created_at);
                    if age_secs <= stale_threshold {
                        checks.push(DiagnosticCheck::pass(
                            "Heartbeat",
                            format!("last heartbeat: {relative}"),
                        ));
                    } else {
                        checks.push(DiagnosticCheck::warn(
                            "Heartbeat",
                            "last heartbeat",
                            format!("{relative} — may not be running"),
                        ));
                    }
                }
                Ok(_) => {
                    checks.push(DiagnosticCheck::warn(
                        "Heartbeat",
                        "last heartbeat",
                        "no heartbeat activity recorded",
                    ));
                }
                Err(e) => {
                    checks.push(DiagnosticCheck::fail(
                        "Heartbeat",
                        "activity query",
                        format!("{e}"),
                    ));
                }
            }

            if db.is_daemon_lock_held().await {
                checks.push(DiagnosticCheck::pass(
                    "Heartbeat",
                    "daemon is running (heartbeat active)".to_string(),
                ));
            } else {
                checks.push(DiagnosticCheck::warn(
                    "Heartbeat",
                    "daemon status",
                    "daemon not running — heartbeat only active during TUI sessions",
                ));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::fail(
                "Heartbeat",
                "database",
                format!("cannot check heartbeat status: {e}"),
            ));
        }
    }
}

pub(in crate::doctor) async fn check_budget(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    let limit = config.budget.monthly_token_limit;
    if limit == 0 {
        checks.push(DiagnosticCheck::warn(
            "Budget",
            "monthly limit",
            "unlimited (no budget set)",
        ));
        return;
    }

    match Database::open().await {
        Ok(db) => match db.monthly_token_total().await {
            Ok(used) => {
                let pct = (used as f64 / limit as f64 * 100.0) as u64;
                let name = format!("monthly usage: {used}/{limit} ({pct}%)");
                if used >= limit {
                    checks.push(DiagnosticCheck::fail("Budget", name, "budget exceeded"));
                } else if (used as f64 / limit as f64) >= config.budget.warning_threshold {
                    checks.push(DiagnosticCheck::warn("Budget", name, "approaching limit"));
                } else {
                    checks.push(DiagnosticCheck::pass("Budget", name));
                }
            }
            Err(e) => {
                checks.push(DiagnosticCheck::warn(
                    "Budget",
                    "monthly usage",
                    format!("could not read: {e}"),
                ));
            }
        },
        Err(e) => {
            checks.push(DiagnosticCheck::warn(
                "Budget",
                "monthly usage",
                format!("database unavailable: {e}"),
            ));
        }
    }
}

pub(in crate::doctor) async fn check_prompt_cache(
    config: &Config,
    checks: &mut Vec<DiagnosticCheck>,
) {
    if !config.llm.cache.enabled {
        checks.push(DiagnosticCheck::warn(
            "Prompt Cache",
            "status",
            "disabled (llm.cache.enabled = false)",
        ));
        return;
    }
    checks.push(DiagnosticCheck::pass(
        "Prompt Cache",
        format!(
            "enabled (ttl={}, tools={}, system={}, rolling={})",
            config.llm.cache.ttl,
            config.llm.cache.cache_tools,
            config.llm.cache.cache_system,
            config.llm.cache.rolling_messages_clamped(),
        ),
    ));

    // Hit ratio over the last 24 hours — informational. Providers that do not
    // support caching (Ollama, Groq, Gemini non-cached) will report 0, which
    // we surface as a warning only when the primary provider is known to
    // support caching.
    let since = chrono::Utc::now().timestamp().saturating_sub(24 * 3600);
    let summary_result: Result<(u64, u64, u64)> = match Database::open().await {
        Ok(db) => db.cache_token_summary_since(since).await,
        Err(e) => Err(e),
    };
    match summary_result {
        Ok((prompt, cached, created)) => {
            if prompt == 0 {
                checks.push(DiagnosticCheck::pass(
                    "Prompt Cache",
                    "24h hit ratio: no traffic yet",
                ));
                return;
            }
            let pct = (cached as f64 / prompt as f64 * 100.0) as u64;
            let name =
                format!("24h hit ratio: {pct}% ({cached}/{prompt} cached, {created} created)");
            let supports_cache = provider_supports_cache(&config.llm);
            if supports_cache && pct < 20 && prompt > 10_000 {
                checks.push(DiagnosticCheck::warn(
                    "Prompt Cache",
                    name,
                    "low hit ratio — check system prompt stability",
                ));
            } else {
                checks.push(DiagnosticCheck::pass("Prompt Cache", name));
            }
        }
        Err(e) => {
            checks.push(DiagnosticCheck::warn(
                "Prompt Cache",
                "24h hit ratio",
                format!("could not read: {e}"),
            ));
        }
    }
}

/// Returns true if the configured provider is known to honor prompt caching
/// (either via explicit `cache_control` markers or implicit auto-caching).
fn provider_supports_cache(llm: &crate::config::LlmConfig) -> bool {
    let provider = llm.provider.as_deref().unwrap_or("");
    matches!(
        provider,
        "anthropic" | "openai" | "openrouter" | "deepseek" | "claude-cli"
    )
}

pub(in crate::doctor) async fn check_agents(config: &Config, checks: &mut Vec<DiagnosticCheck>) {
    if !config.agents.enabled {
        checks.push(DiagnosticCheck::warn(
            "Agents",
            "multi-agent system",
            "disabled",
        ));
        return;
    }
    checks.push(DiagnosticCheck::pass(
        "Agents",
        format!(
            "multi-agent enabled (depth={}, children={}, concurrent={})",
            config.agents.max_spawn_depth,
            config.agents.max_children_per_agent,
            config.agents.max_concurrent,
        ),
    ));
    if config.agents.max_spawn_depth > 3 {
        checks.push(DiagnosticCheck::warn(
            "Agents",
            "spawn depth",
            format!(
                "max_spawn_depth={} is high (recommended ≤3)",
                config.agents.max_spawn_depth
            ),
        ));
    }
    let roles = crate::multi_agent::roles::list_all_roles().await;
    checks.push(DiagnosticCheck::pass(
        "Agents",
        format!("{} role(s) available", roles.len()),
    ));
}

/// Build the orphan-lock finding from its idle age. Pulled out so the message
/// and remediation wiring are testable without a DB. The detection itself
/// (dead PID + stale heartbeat) lives in `Database::daemon_lock_orphan_age`.
fn orphan_lock_check(idle_secs: i64) -> DiagnosticCheck {
    DiagnosticCheck::warn(
        "Locks",
        "orphan daemon lock",
        format!("daemon lock held by a dead PID, idle {idle_secs}s — a daemon likely crashed mid-session"),
    )
    .with_remediation(
        RemediationKind::AutoSafe,
        "locks.clear_orphan",
        "run `borg doctor --repair` (or `/heal`) to clear the stale lock",
    )
}

/// Build the previous-unclean-exit finding, or `None` when there is nothing
/// worth saying.
///
/// Pure so the "when do we warn" decision is testable without a DB. Warns only
/// while the unclean stop is the *most recent* shutdown — a later clean stop
/// retires it — and only inside
/// [`DAEMON_UNCLEAN_EXIT_WARN_WINDOW_SECS`]. An implementation that merely
/// checks "the meta key exists" would warn forever.
fn unclean_exit_check(unclean_at: i64, clean_at: Option<i64>, now: i64) -> Option<DiagnosticCheck> {
    if clean_at.is_some_and(|c| c >= unclean_at) {
        return None;
    }
    let age = now.saturating_sub(unclean_at);
    if age > crate::constants::DAEMON_UNCLEAN_EXIT_WARN_WINDOW_SECS {
        return None;
    }
    Some(
        DiagnosticCheck::warn(
            "Locks",
            "previous shutdown",
            format!(
                "borgd exited uncleanly ~{}h ago (OOM kill, SIGKILL, or host reset) — \
                 in-flight task runs may have been lost",
                age / crate::constants::SECS_PER_HOUR
            ),
        )
        .with_remediation(
            RemediationKind::Manual,
            "daemon.unclean_exit",
            "check `journalctl -u borgd` / `dmesg` for an OOM record around that time",
        ),
    )
}

/// Orphan daemon-lock detection. Emits a finding only when the lock is held by
/// a dead PID past the staleness grace — a running daemon (live PID or fresh
/// heartbeat) is never flagged. Suppressed when healthy/absent.
///
/// Also reports a previous unclean exit, which the daemon records at boot.
pub(in crate::doctor) async fn check_daemon_lock(checks: &mut Vec<DiagnosticCheck>) {
    let Ok(db) = Database::open().await else {
        return;
    };
    let now = chrono::Utc::now().timestamp();
    if let Ok(Some(age)) = db.daemon_lock_orphan_age(now, DAEMON_LOCK_STALE_SECS).await {
        checks.push(orphan_lock_check(age));
    }

    let parse = |v: Option<String>| v.and_then(|s| s.parse::<i64>().ok());
    let unclean = parse(
        db.get_meta(crate::constants::META_LAST_UNCLEAN_SHUTDOWN_AT)
            .await
            .unwrap_or(None),
    );
    if let Some(unclean_at) = unclean {
        let clean_at = parse(
            db.get_meta(crate::constants::META_LAST_CLEAN_SHUTDOWN_AT)
                .await
                .unwrap_or(None),
        );
        if let Some(check) = unclean_exit_check(unclean_at, clean_at, now) {
            checks.push(check);
        }
    }
}

/// Scheduled-task health from a DB handle (injectable for tests). Surfaces a
/// count summary plus any task whose last run errored — the genuinely
/// actionable signal. Overdue-but-not-errored tasks are left to the nightly
/// heal (flagging them here would be noise whenever the daemon is simply off).
async fn scheduled_task_checks(db: &Database) -> Vec<DiagnosticCheck> {
    // Single fetch: `list_tasks` already returns every row including
    // `last_error`, so the count summary and the errored subset come from one
    // scan rather than a second `tasks_with_last_error` query.
    let tasks = match db.list_tasks().await {
        Ok(t) => t,
        Err(e) => {
            return vec![DiagnosticCheck::warn(
                "Scheduled Tasks",
                "task store",
                format!("could not list tasks: {e}"),
            )]
        }
    };
    let mut out = vec![DiagnosticCheck::pass(
        "Scheduled Tasks",
        format!("{} scheduled task(s)", tasks.len()),
    )];
    let mut errored: Vec<&str> = tasks
        .iter()
        .filter(|t| t.last_error.as_deref().is_some_and(|e| !e.is_empty()))
        .map(|t| t.name.as_str())
        .collect();
    errored.sort_unstable(); // stable, name-ordered output
    if !errored.is_empty() {
        let sample = errored
            .iter()
            .take(3)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        out.push(DiagnosticCheck::warn(
            "Scheduled Tasks",
            "tasks with errors",
            format!(
                "{} task(s) failed their last run (e.g. {sample}) — inspect via `borg tasks list`",
                errored.len()
            ),
        ));
    }

    // A task whose delivery columns cannot address anyone runs to completion
    // and then drops its result. The scheduler refuses the run outright, but
    // that only shows up once the task next fires — this names it beforehand.
    // Same predicate as the scheduler's, so the two cannot disagree.
    // System-seeded rows deliberately have no channel, so they are exempt.
    let mut undeliverable: Vec<&str> = tasks
        .iter()
        .filter(|t| t.status == crate::tasks::TASK_STATUS_ACTIVE)
        .filter(|t| !crate::task_ids::is_system_task_id(&t.id))
        .filter(|t| {
            crate::tasks::delivery_defect(
                t.delivery_channel.as_deref(),
                t.delivery_target.as_deref(),
            )
            .is_some()
        })
        .map(|t| t.name.as_str())
        .collect();
    undeliverable.sort_unstable();
    if !undeliverable.is_empty() {
        out.push(DiagnosticCheck::warn(
            "Scheduled Tasks",
            "tasks that cannot deliver",
            format!(
                "{} task(s) name a delivery channel they cannot address, so their \
                 results go nowhere ({}) — ask Borg to set a delivery channel \
                 and target for them",
                undeliverable.len(),
                undeliverable.join(", ")
            ),
        ));
    }
    out
}

pub(in crate::doctor) async fn check_scheduled_tasks(checks: &mut Vec<DiagnosticCheck>) {
    match Database::open().await {
        Ok(db) => checks.extend(scheduled_task_checks(&db).await),
        Err(e) => checks.push(DiagnosticCheck::warn(
            "Scheduled Tasks",
            "task store",
            format!("database unavailable: {e}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{passes, warns};
    use super::*;
    use crate::db::NewTask;
    use crate::doctor::CheckStatus;

    #[test]
    fn format_relative_time_seconds() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 30), "30 seconds ago");
        assert_eq!(format_relative_time(now - 1), "1 second ago");
    }

    #[test]
    fn format_relative_time_minutes() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 60), "1 minute ago");
        assert_eq!(format_relative_time(now - 120), "2 minutes ago");
        assert_eq!(format_relative_time(now - 1800), "30 minutes ago");
    }

    #[test]
    fn format_relative_time_hours() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 3600), "1 hour ago");
        assert_eq!(format_relative_time(now - 7200), "2 hours ago");
    }

    #[test]
    fn format_relative_time_days() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 86400), "1 day ago");
        assert_eq!(format_relative_time(now - 172800), "2 days ago");
    }

    #[test]
    fn format_relative_time_future() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now + 100), "just now");
    }

    #[tokio::test]
    async fn heartbeat_check_shows_interval() {
        let config = Config::default();
        let mut checks = Vec::new();
        check_heartbeat(&config, &mut checks).await;
        assert!(checks.iter().any(|c| c.name.contains("interval: 60m")));
    }

    #[test]
    fn orphan_lock_check_is_autosafe_with_repair_key() {
        let c = orphan_lock_check(420);
        assert!(warns(&c));
        assert_eq!(c.remediation_kind, RemediationKind::AutoSafe);
        assert_eq!(c.remediation_key, Some("locks.clear_orphan"));
        let CheckStatus::Warn(msg) = &c.status else {
            unreachable!()
        };
        assert!(msg.contains("420"), "message names the idle age: {msg}");
    }

    #[tokio::test]
    async fn scheduled_tasks_warns_only_on_errored_tasks() {
        let db = Database::test_db().await;

        // Clean store: summary pass, no warning.
        let clean = scheduled_task_checks(&db).await;
        assert!(clean.iter().all(passes), "clean store → all pass");
        assert!(
            clean.iter().any(|c| c.name.contains("scheduled task(s)")),
            "must report a task count summary",
        );

        // Seed a task whose last run errored.
        db.create_task(&NewTask {
            id: "boom",
            name: "flaky job",
            prompt: "do stuff",
            schedule_type: "interval",
            schedule_expr: "1h",
            timezone: "local",
            next_run: Some(100),
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
            continuity: false,
        })
        .await
        .expect("create task");
        db.set_task_retry("boom", 1, "connection refused", 0)
            .await
            .expect("set last_error");

        let after = scheduled_task_checks(&db).await;
        let err_warn = after
            .iter()
            .find(|c| c.name == "tasks with errors")
            .expect("errored-task warning present");
        assert!(warns(err_warn), "a task with last_error must warn");
        let CheckStatus::Warn(msg) = &err_warn.status else {
            unreachable!()
        };
        assert!(
            msg.contains("flaky job"),
            "warning names the offending task: {msg}"
        );
    }

    /// Build a task row differing only in the fields this check reads.
    fn task_spec<'a>(
        id: &'a str,
        name: &'a str,
        channel: Option<&'a str>,
        target: Option<&'a str>,
    ) -> NewTask<'a> {
        NewTask {
            id,
            name,
            prompt: "do stuff",
            schedule_type: "interval",
            schedule_expr: "1h",
            timezone: "local",
            next_run: Some(100),
            max_retries: Some(3),
            timeout_ms: None,
            delivery_channel: channel,
            delivery_target: target,
            allowed_tools: None,
            task_type: "prompt",
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: None,
            min_interval_secs: None,
            continuity: false,
        }
    }

    #[tokio::test]
    async fn scheduled_tasks_warns_about_a_channel_with_no_target() {
        let db = Database::test_db().await;
        let find = |checks: Vec<DiagnosticCheck>| {
            checks
                .into_iter()
                .find(|c| c.name == "tasks that cannot deliver")
        };

        // Re-seed the heartbeat row in the shape that *would* warn, to prove
        // the system-task exemption actually carries its weight. As seeded it
        // has no channel, so the fail-open rule alone would exempt it and the
        // exemption could be deleted with this test still passing.
        db.delete_task(crate::task_ids::HEARTBEAT_TASK_ID)
            .await
            .expect("clear seeded heartbeat");

        // A system row must stay silent even misaddressed, as must a fully
        // addressed task, a channel-less user task, and a broadcast.
        for (id, name, channel, target) in [
            (
                crate::task_ids::HEARTBEAT_TASK_ID,
                "heartbeat",
                Some("telegram"),
                None,
            ),
            ("addressed", "addressed job", Some("telegram"), Some("42")),
            ("silent", "side-effect job", None, None),
            (
                "fanout",
                "broadcast job",
                Some(crate::heartbeat_task::BROADCAST_DELIVERY),
                None,
            ),
        ] {
            db.create_task(&task_spec(id, name, channel, target))
                .await
                .expect("create task");
        }
        assert!(
            find(scheduled_task_checks(&db).await).is_none(),
            "well-formed and system tasks must not warn"
        );

        db.create_task(&task_spec("orphan", "orphan job", Some("telegram"), None))
            .await
            .expect("create task");
        let warn = find(scheduled_task_checks(&db).await).expect("undeliverable warning present");
        assert!(warns(&warn), "a channel with no target must warn");
        let CheckStatus::Warn(msg) = &warn.status else {
            unreachable!()
        };
        assert!(msg.contains("orphan job"), "warning names the task: {msg}");
        assert!(msg.contains("ask Borg"), "warning names the fix: {msg}");
        assert_eq!(
            crate::tasks::unknown_borg_command(msg),
            None,
            "warning must not name a command that does not exist: {msg}"
        );
    }

    #[test]
    fn unclean_exit_check_only_warns_for_the_most_recent_unclean_stop() {
        const HOUR: i64 = 3600;
        let now = 1_000_000_i64;
        // (unclean_at, clean_at, should_warn)
        let cases: &[(i64, Option<i64>, bool)] = &[
            // Crashed, never cleanly stopped since.
            (now - HOUR, None, true),
            // Crashed, then stopped cleanly — retired, no warning.
            (now - 2 * HOUR, Some(now - HOUR), false),
            // Cleanly stopped, then crashed later — warn about the crash.
            (now - HOUR, Some(now - 2 * HOUR), true),
            // Same instant counts as retired (clean wins ties).
            (now - HOUR, Some(now - HOUR), false),
            // Ancient crash: past the window, nothing actionable remains.
            (now - 30 * 24 * HOUR, None, false),
        ];
        for (unclean_at, clean_at, want) in cases {
            let got = unclean_exit_check(*unclean_at, *clean_at, now).is_some();
            assert_eq!(
                got, *want,
                "unclean_at={unclean_at} clean_at={clean_at:?} should_warn={want}"
            );
        }
    }

    #[test]
    fn unclean_exit_check_points_at_the_os_log() {
        let now = 1_000_000_i64;
        let check = unclean_exit_check(now - 3600, None, now).expect("warns");
        assert_eq!(check.category, "Locks");
        let msg = match &check.status {
            crate::doctor::types::CheckStatus::Warn(m) => m.clone(),
            other => panic!("expected a Warn, got {other:?}"),
        };
        assert!(msg.contains("uncleanly"), "got: {msg}");
        // The hint is what makes this actionable rather than trivia.
        let hint = check.user_hint.as_deref().unwrap_or_default();
        assert!(
            hint.contains("journalctl") || hint.contains("dmesg"),
            "got: {hint}"
        );
        assert_eq!(check.remediation_kind, RemediationKind::Manual);
    }
}
