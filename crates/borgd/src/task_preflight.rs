//! Pre-dispatch validation for scheduled tasks.
//!
//! A task with a delivery channel but no target used to run to completion,
//! spend tokens, record its output, and then deliver nowhere — every tick,
//! silently, forever. Every delivery site is an `if let (Some(ch), Some(tgt))`,
//! so the drop had no error path at all.
//!
//! This module answers one question before any of that happens: *could this
//! run possibly reach the user?* It is pure, synchronous, and does no I/O — no
//! network probe, because a probe fails closed on a flaky link and would block
//! a task that is perfectly well configured.
//!
//! It is a composer, not a policy owner. The delivery rule itself lives in
//! [`cortex::tasks::delivery_defect`], so the doctor warns about exactly the
//! condition the scheduler refuses; this module adds the LLM-provider check and
//! turns a defect into a refusal with an operator-facing message.
//!
//! **Every check fails open.** Only an affirmative misconfiguration blocks.
//! The seeded system rows legitimately have no delivery channel, and a
//! `command` task on a machine with no LLM provider is a working shell cron —
//! neither may be stopped.

use cortex::config::Config;
use cortex::db::ScheduledTaskRow;
use cortex::tasks::DeliveryDefect;

/// Whether a block is about delivery itself.
///
/// This decides whether the operator may be told through the channel the task
/// names. It is a field rather than a naming convention on [`BlockCode`]
/// because getting it wrong sends a notice down the very pipe that is broken,
/// and a `match` makes the compiler ask the question for every new code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// The task cannot run, but its delivery path is fine.
    Runtime,
    /// The delivery path is the defect, so no notice can travel it.
    Delivery,
}

/// Why a run was refused. Stable identifiers for logs and tests; not shown to
/// the user, who gets the message instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockCode {
    /// The task needs an LLM and no provider is configured.
    NoLlmProvider,
    /// Still addressed to `origin`, which was never resolved.
    UnresolvedOrigin,
    /// A channel is named but no target, so the result would be discarded.
    NoTarget,
}

impl BlockCode {
    /// Whether this block is about delivery itself.
    pub fn kind(&self) -> BlockKind {
        match self {
            BlockCode::NoLlmProvider => BlockKind::Runtime,
            BlockCode::UnresolvedOrigin | BlockCode::NoTarget => BlockKind::Delivery,
        }
    }

    /// Log-friendly label.
    pub fn as_str(&self) -> &'static str {
        match self {
            BlockCode::NoLlmProvider => "no_llm_provider",
            BlockCode::UnresolvedOrigin => "delivery_unresolved_origin",
            BlockCode::NoTarget => "delivery_no_target",
        }
    }
}

/// Outcome of [`check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preflight {
    /// Nothing affirmatively wrong; dispatch the run.
    Ok,
    /// The task cannot succeed as configured. Nothing was executed.
    Blocked {
        /// What kind of misconfiguration this is.
        code: BlockCode,
        /// Operator-facing reason, which always names the next action.
        message: String,
    },
}

/// Task types this check must not block for a missing LLM provider.
///
/// `command` and `maintenance` never build an LLM client at all — see the
/// dispatch match in `task_runner`. `heartbeat` does, but it already owns a
/// deliberate, quiet exit for this exact case (`SkipReason::NoLlmProvider`):
/// on a fresh install the seeded row must skip without writing `last_error`
/// or raising a warning, so blocking it here would be the "heartbeat errored
/// during first-run" first impression that lane was written to prevent.
const NO_LLM_TASK_TYPES: &[&str] = &["command", "maintenance", "heartbeat"];

/// Decide whether `task` can run, given `config`.
///
/// `config` must be the *resolved* config for this run — the per-task model
/// pin has already been applied — or a task that pins its own provider would
/// be blocked for lacking the global one.
pub fn check(task: &ScheduledTaskRow, config: &Config) -> Preflight {
    let blocked = |code: BlockCode, message: String| Preflight::Blocked { code, message };

    let needs_llm = !NO_LLM_TASK_TYPES.contains(&task.task_type.as_str())
        // The seeded prompt rows (security audit, daily summary, both
        // consolidations) are on every install, including one that has not
        // reached the credential step yet. Blocking them there would write a
        // `misconfigured` run and a warn per row per tick, for the same
        // fresh-install reason `heartbeat` is exempt above.
        && !cortex::task_ids::is_system_task_id(&task.id);
    if needs_llm && config.llm.no_provider_at_all() {
        return blocked(
            BlockCode::NoLlmProvider,
            format!(
                "task '{}' needs an LLM but no provider is configured — \
                 run `borg init`, or set llm.provider",
                task.name
            ),
        );
    }

    let channel = task.delivery_channel.as_deref();
    match cortex::tasks::delivery_defect(channel, task.delivery_target.as_deref()) {
        Some(DeliveryDefect::UnresolvedOrigin) => {
            return blocked(
                BlockCode::UnresolvedOrigin,
                format!(
                    "task '{}' delivers to 'origin' but no originating conversation \
                     was recorded — {}",
                    task.name,
                    DeliveryDefect::UnresolvedOrigin.next_action()
                ),
            );
        }
        Some(DeliveryDefect::NoTarget) => {
            return blocked(
                BlockCode::NoTarget,
                format!(
                    "task '{}' delivers to {} but has no target, so its result would \
                     be discarded — {}",
                    task.name,
                    channel.unwrap_or_default().trim(),
                    DeliveryDefect::NoTarget.next_action()
                ),
            );
        }
        None => {}
    }

    // Deliberately *not* blocked on here: whether `channel_delivery` has a
    // send arm for the named channel, or holds its credential.
    //
    // `resolve_origin_delivery` stamps the originating channel straight into
    // `delivery_channel`, and Borg receives on more channels than it sends on
    // (imessage, signal, twilio, teams, google_chat). Refusing those would stop
    // a task that runs perfectly well today from executing at all — where now
    // it runs, records its output in `task_runs`, and the hand-off alone fails
    // as `delivery_failed`, which is already a visible, distinct status.
    //
    // The line is "could this run produce anything of value": a task with no
    // target produces a result nobody can ever read, while one on an
    // unsupported channel still leaves a readable run row.
    Preflight::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(task_type: &str, channel: Option<&str>, target: Option<&str>) -> ScheduledTaskRow {
        ScheduledTaskRow {
            id: "task-1".to_string(),
            name: "nightly digest".to_string(),
            prompt: "do the thing".to_string(),
            schedule_type: "cron".to_string(),
            schedule_expr: "0 3 * * *".to_string(),
            timezone: "local".to_string(),
            status: "active".to_string(),
            next_run: None,
            created_at: 0,
            max_retries: 3,
            retry_count: 0,
            retry_after: None,
            last_error: None,
            timeout_ms: 300_000,
            delivery_channel: channel.map(str::to_string),
            delivery_target: target.map(str::to_string),
            allowed_tools: None,
            task_type: task_type.to_string(),
            pre_run_command: None,
            pre_run_cwd: None,
            pre_run_timeout_ms: 60_000,
            bypass_quiet_hours_once: false,
            min_interval_secs: None,
            last_response_hash: None,
            model: None,
            model_provider: None,
            monitor_mode: false,
            last_pre_run_hash: None,
            continuity: false,
        }
    }

    fn with_provider() -> Config {
        let mut config = Config::default();
        config.llm.provider = Some("anthropic".to_string());
        config
    }

    fn with_telegram() -> Config {
        let mut config = with_provider();
        // `PATH` stands in for a real token: credentials resolve through the
        // environment, and PATH is the one variable guaranteed to be set and
        // non-empty wherever these tests run.
        config.credentials.insert(
            "TELEGRAM_BOT_TOKEN".to_string(),
            cortex::config::CredentialValue::EnvVar("PATH".to_string()),
        );
        config
    }

    /// One row per decision `check` can reach. `expected: None` means the run
    /// is dispatched; `Some(code)` means it is refused with that code.
    ///
    /// Table-driven on purpose: a new block reason is one row, and the message
    /// assertions below then cover it automatically instead of only covering
    /// the reasons someone remembered to list twice.
    #[allow(clippy::type_complexity)]
    const CASES: &[(&str, &str, Option<&str>, Option<&str>, Option<BlockCode>)] = &[
        // The seeded system rows all look like this. Blocking them would take
        // out the heartbeat, maintenance, and consolidation loops at once.
        ("no channel", "prompt", None, None, None),
        ("blank channel", "prompt", Some("  "), None, None),
        (
            "fully configured",
            "prompt",
            Some("telegram"),
            Some("12345"),
            None,
        ),
        (
            "broadcast needs no target",
            "prompt",
            Some("broadcast"),
            None,
            None,
        ),
        // The silent-drop bug: this ran, cost tokens, and delivered nothing.
        (
            "channel with no target",
            "prompt",
            Some("telegram"),
            None,
            Some(BlockCode::NoTarget),
        ),
        (
            "blank target is no target",
            "prompt",
            Some("telegram"),
            Some(" "),
            Some(BlockCode::NoTarget),
        ),
        (
            "unresolved origin",
            "prompt",
            Some("origin"),
            Some("x"),
            Some(BlockCode::UnresolvedOrigin),
        ),
        // Borg receives on more channels than it sends on, and
        // `resolve_origin_delivery` stamps the receiving channel into the row.
        // These still run and record output; only the hand-off fails, as
        // `delivery_failed`. Blocking them would stop working tasks dead.
        (
            "channel delivery cannot send on",
            "prompt",
            Some("imessage"),
            Some("+15550100"),
            None,
        ),
        (
            "channel nobody has heard of",
            "prompt",
            Some("carrier-pigeon"),
            Some("x"),
            None,
        ),
    ];

    #[test]
    fn check_classifies_every_delivery_shape() {
        for (label, task_type, channel, target, expected) in CASES {
            let result = check(&task(task_type, *channel, *target), &with_telegram());
            match (expected, &result) {
                (None, Preflight::Ok) => {}
                (Some(want), Preflight::Blocked { code, .. }) if code == want => {}
                _ => panic!("{label}: expected {expected:?}, got {result:?}"),
            }
        }
    }

    /// Imperatives a blocker message may open its fix with. A message that
    /// carries none of these describes the problem without saying what to do
    /// about it, which is the failure mode this test exists to catch.
    const NEXT_ACTION_VERBS: &[&str] = &["set ", "run `borg", "ask Borg"];

    #[test]
    fn every_blocker_message_names_the_task_and_a_real_action() {
        // A reworded-but-useless message must fail this, not pass it — and so
        // must one that tells the operator to run a command that does not
        // exist, which is how `borg schedule update` shipped in the first
        // draft of this module.
        for (label, task_type, channel, target, expected) in CASES {
            if expected.is_none() {
                continue;
            }
            let result = check(&task(task_type, *channel, *target), &with_telegram());
            let Preflight::Blocked { message, .. } = result else {
                panic!("{label}: expected a block");
            };
            assert!(
                NEXT_ACTION_VERBS.iter().any(|v| message.contains(v)),
                "{label}: no next action in: {message}"
            );
            assert_eq!(
                cortex::tasks::unknown_borg_command(&message),
                None,
                "{label}: names a command that does not exist: {message}"
            );
            assert!(
                message.contains("nightly digest"),
                "{label}: the operator must know which task: {message}"
            );
        }
    }

    #[test]
    fn a_channel_delivery_cannot_send_on_still_runs() {
        // The regression this guards: `resolve_origin_delivery` stamps the
        // originating channel into the row, and Borg receives on channels
        // `send_threaded` has no arm for. Refusing them would stop tasks that
        // work today — they run, record output, and fail only at hand-off.
        for channel in ["imessage", "signal", "twilio", "carrier-pigeon"] {
            assert_eq!(
                check(
                    &task("prompt", Some(channel), Some("somewhere")),
                    &with_provider()
                ),
                Preflight::Ok,
                "{channel} must still run"
            );
        }
        // Including one whose credential is absent: the token is delivery's
        // problem, not a reason to skip the work.
        assert_eq!(
            check(
                &task("prompt", Some("telegram"), Some("12345")),
                &with_provider()
            ),
            Preflight::Ok,
            "a missing token must not stop the run"
        );
    }

    #[test]
    fn only_delivery_blockers_are_marked_as_such() {
        // This decides whether the alert may be delivered through the channel
        // the task names. Getting it wrong sends a notice down a broken pipe.
        assert_eq!(BlockCode::NoLlmProvider.kind(), BlockKind::Runtime);
        for code in [BlockCode::UnresolvedOrigin, BlockCode::NoTarget] {
            assert_eq!(code.kind(), BlockKind::Delivery, "{code:?}");
        }
    }

    #[test]
    fn an_aux_only_provider_is_enough_to_run() {
        // `LlmClient::for_auxiliary` resolves `llm.aux.provider` before falling
        // back to the primary, so a box configured that way runs scheduled
        // tasks fine. Blocking on the primary alone would refuse working work.
        let mut config = Config::default();
        config.llm.aux = Some(cortex::config::AuxLlmConfig {
            provider: Some("anthropic".to_string()),
            ..Default::default()
        });
        assert_eq!(check(&task("prompt", None, None), &config), Preflight::Ok);
    }

    #[test]
    fn the_seeded_prompt_rows_survive_a_provider_less_install() {
        // Security audit, daily summary and the consolidations are `prompt`
        // rows on every install, including one that has not reached the
        // credential step. Blocking them would write a `misconfigured` run and
        // a warn per row per tick — the same first-run noise `heartbeat` is
        // exempt from.
        let mut row = task("prompt", None, None);
        row.id = cortex::task_ids::SECURITY_AUDIT_TASK_ID.to_string();
        assert_eq!(check(&row, &Config::default()), Preflight::Ok);
    }

    #[test]
    fn the_provider_exempt_task_types_run_without_an_llm_provider() {
        // A `command` cron on a provider-less box is a supported setup, and
        // the seeded heartbeat row must reach its own quiet skip rather than
        // be recorded as misconfigured on every fresh install.
        for task_type in NO_LLM_TASK_TYPES {
            assert_eq!(
                check(&task(task_type, None, None), &Config::default()),
                Preflight::Ok,
                "{task_type} must not need a provider"
            );
        }
        assert_eq!(
            check(
                &task(
                    "heartbeat",
                    Some(cortex::heartbeat_task::BROADCAST_DELIVERY),
                    None
                ),
                &Config::default()
            ),
            Preflight::Ok,
            "the seeded heartbeat row is exactly this shape"
        );
    }

    #[test]
    fn a_prompt_task_without_an_llm_provider_is_blocked() {
        let result = check(&task("prompt", None, None), &Config::default());
        let Preflight::Blocked { code, message } = result else {
            panic!("a prompt task with no provider must block");
        };
        assert_eq!(code, BlockCode::NoLlmProvider);
        assert!(message.contains("nightly digest"), "{message}");
    }
}
