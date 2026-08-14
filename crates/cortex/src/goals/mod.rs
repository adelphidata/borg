//! Per-session goals: typed objective + status + token budget, accounting,
//! steering prompts, and runtime state.
//!
//! A session has at most one goal at a time. The model can read it via
//! `get_goal`, create one via `create_goal` (only when none exists), and mark
//! the active goal complete via `update_goal`. Pause/resume/budget changes are
//! user-driven (via `/goal …` in the TUI or the daemon RPC).

use serde::Deserialize;
use serde::Serialize;

pub mod gates;
pub mod metrics;
pub mod prompts;
pub mod runtime;

/// Lifecycle kinds emitted to a [`GoalEventSink`]. Mirrors the wire-format
/// `kind` field on the admin gRPC `GoalEvent` so the daemon can forward the
/// value without re-mapping. Strings (not an enum) keep the trait surface
/// stable when new kinds are added without cortex needing a recompile of
/// downstream binaries.
pub mod event_kind {
    /// A goal row was just inserted (`create_goal` tool / `SetGoal{Create}` RPC).
    pub const CREATED: &str = "created";
    /// An existing goal's objective was replaced (regenerated `goal_id`).
    pub const REPLACED: &str = "replaced";
    /// Status changed (paused, resumed, etc.).
    pub const STATUS_CHANGED: &str = "status_changed";
    /// Token budget was edited without a status change.
    pub const BUDGET_CHANGED: &str = "budget_changed";
    /// Accounting flipped status to `budget_limited`.
    pub const BUDGET_LIMITED: &str = "budget_limited";
    /// Agent reported a repeated impasse via `update_goal` (status="blocked").
    pub const BLOCKED: &str = "blocked";
    /// Goal completed via `update_goal`.
    pub const COMPLETED: &str = "completed";
    /// Goal row was deleted (`ClearGoal`).
    pub const CLEARED: &str = "cleared";
}

/// Best-effort lifecycle sink for goal mutations. Implemented by the daemon
/// (forwards onto the `SubscribeGoalEvents` broadcast channel) so any
/// subscriber — TUI footer, future remote dashboard — can react without
/// polling the DB.
///
/// `goal` is `None` for the `cleared` kind (the row no longer exists);
/// always `Some` otherwise. Implementations MUST NOT panic; the agent loop
/// fires these on hot paths and treats them as fire-and-forget.
pub trait GoalEventSink: Send + Sync + 'static {
    /// Forward one lifecycle event. `kind` is one of the [`event_kind`]
    /// constants. `session_id` is non-empty.
    fn emit(&self, kind: &str, session_id: &str, goal: Option<&Goal>);
}

/// Hard cap on objective length. Prevents the objective from blowing
/// out the system-prompt suffix.
pub const MAX_OBJECTIVE_CHARS: usize = 4_000;

/// Goal lifecycle status. Stored in SQLite as snake_case strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// Agent is actively pursuing the goal.
    Active,
    /// User paused the goal; no auto-continuation.
    Paused,
    /// Token budget reached; agent should wrap up.
    BudgetLimited,
    /// LLM provider usage limit reached (rate-limit / quota / 429) — distinct
    /// from [`GoalStatus::BudgetLimited`] (per-goal token budget) so the
    /// operator can tell "you spent the budget you set" from "the provider cut
    /// us off." Fully honored on the read side (auto-continuation stops via
    /// [`runtime::should_continue`], the heartbeat scheduler treats it as
    /// terminal, and the TUI renders it), and the operator resumes it like any
    /// other stop. Not auto-written today: borg's LLM resilience layer absorbs
    /// provider 429s with per-key cooldowns and the `llm.fallback` chain, so a
    /// transient cap doesn't reach the goal layer, and terminal exhaustion
    /// surfaces only as a formatted stream-error string that is unsafe to
    /// pattern-match into a status flip. Reserved for a future structured
    /// exhaustion signal and for daemon/operator use.
    UsageLimited,
    /// Agent hit a repeated impasse (permission denial, missing external
    /// resource, persistent tool failure) and called `update_goal` with
    /// `status="blocked"` after ≥3 attempts past the same wall. Auto-
    /// continuation stops until the user resumes.
    Blocked,
    /// Objective achieved.
    Complete,
}

impl GoalStatus {
    /// SQL/wire string form.
    pub const fn as_str(&self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::BudgetLimited => "budget_limited",
            GoalStatus::UsageLimited => "usage_limited",
            GoalStatus::Blocked => "blocked",
            GoalStatus::Complete => "complete",
        }
    }

    /// Parse from SQL/wire string. Returns `None` for unknown variants.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "active" => GoalStatus::Active,
            "paused" => GoalStatus::Paused,
            "budget_limited" => GoalStatus::BudgetLimited,
            "usage_limited" => GoalStatus::UsageLimited,
            "blocked" => GoalStatus::Blocked,
            "complete" => GoalStatus::Complete,
            _ => return None,
        })
    }

    /// Human-readable label for TUI summary cards.
    pub const fn label(&self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::BudgetLimited => "limited by budget",
            GoalStatus::UsageLimited => "usage limited",
            GoalStatus::Blocked => "blocked",
            GoalStatus::Complete => "complete",
        }
    }
}

/// A persisted per-session goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    /// Session this goal belongs to (also the primary key).
    pub session_id: String,
    /// UUID v4 regenerated whenever the objective is replaced; lets accounting
    /// detect mid-flight objective swaps.
    pub goal_id: String,
    /// User-provided objective string (untrusted).
    pub objective: String,
    /// Current lifecycle status.
    pub status: GoalStatus,
    /// Optional positive token budget. `None` = unbounded.
    pub token_budget: Option<i64>,
    /// Tokens consumed pursuing the goal.
    pub tokens_used: i64,
    /// Wall-clock seconds spent with this goal Active.
    pub time_used_seconds: i64,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds.
    pub updated_at: i64,
}

/// Hint appended to the over-limit error so the operator knows the supported
/// workaround: stash long objectives in a file and reference it from `/goal`.
pub const OBJECTIVE_TOO_LONG_FILE_HINT: &str = "Put longer instructions in a file and refer to that file in the goal, for example: /goal follow the instructions in docs/goal.md.";

/// Validate a user-provided objective. Returns the trimmed objective on
/// success.
///
/// Rejects: empty / whitespace-only / longer than [`MAX_OBJECTIVE_CHARS`]
/// characters (counted as Unicode scalar values).
pub fn validate_objective(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("objective must not be empty".to_string());
    }
    let char_count = trimmed.chars().count();
    if char_count > MAX_OBJECTIVE_CHARS {
        return Err(format!(
            "objective is {char_count} characters; limit is {MAX_OBJECTIVE_CHARS}. {OBJECTIVE_TOO_LONG_FILE_HINT}"
        ));
    }
    Ok(trimmed.to_string())
}

/// Validate a token budget against an optional ceiling.
///
/// `budget: None` is unbounded and always valid; `Some(n)` must be strictly
/// positive. When `max` is set, a budget above it is rejected — an unchecked
/// budget lets a model opt out of the limit that bounds the Ralph loop simply
/// by asking for a large enough number.
pub fn validate_budget(budget: Option<i64>, max: Option<i64>) -> Result<Option<i64>, String> {
    let n = match budget {
        None => return Ok(None),
        Some(n) if n > 0 => n,
        Some(n) => return Err(format!("token_budget must be positive (got {n})")),
    };
    match max {
        Some(max) if n > max => Err(format!(
            "token_budget {n} exceeds the configured maximum of {max} \
             (raise goals.max_token_budget to allow it)"
        )),
        _ => Ok(Some(n)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_as_str_and_back_roundtrip() {
        for status in [
            GoalStatus::Active,
            GoalStatus::Paused,
            GoalStatus::BudgetLimited,
            GoalStatus::UsageLimited,
            GoalStatus::Blocked,
            GoalStatus::Complete,
        ] {
            assert_eq!(GoalStatus::from_str(status.as_str()), Some(status));
        }
        assert_eq!(GoalStatus::from_str("nonsense"), None);
    }

    #[test]
    fn usage_limited_wire_form_is_snake_case() {
        // Wire form for ThreadGoalStatus::UsageLimited must serialize as
        // snake_case "usage_limited" so a daemon-side write with
        // `status="usage_limited"` round-trips through `GoalStatus::from_str`
        // rather than silently dropping the status to an unrecognized variant.
        assert_eq!(GoalStatus::UsageLimited.as_str(), "usage_limited");
        assert_eq!(
            GoalStatus::from_str("usage_limited"),
            Some(GoalStatus::UsageLimited)
        );
        // Negative: a camelCase form must not be silently accepted alongside
        // the snake_case wire form, or we'd hide a real serialization mismatch.
        assert_eq!(GoalStatus::from_str("usageLimited"), None);
    }

    #[test]
    fn status_serde_is_snake_case() {
        let json = serde_json::to_string(&GoalStatus::BudgetLimited).unwrap();
        assert_eq!(json, "\"budget_limited\"");
        let back: GoalStatus = serde_json::from_str("\"budget_limited\"").unwrap();
        assert_eq!(back, GoalStatus::BudgetLimited);
    }

    #[test]
    fn validate_objective_rejects_empty_and_whitespace() {
        assert!(validate_objective("").is_err());
        assert!(validate_objective("   \n\t").is_err());
    }

    #[test]
    fn validate_objective_trims() {
        assert_eq!(
            validate_objective("  ship it  ").unwrap(),
            "ship it".to_string()
        );
    }

    #[test]
    fn validate_objective_enforces_char_cap() {
        let ok = "x".repeat(MAX_OBJECTIVE_CHARS);
        assert_eq!(
            validate_objective(&ok).unwrap().chars().count(),
            MAX_OBJECTIVE_CHARS
        );
        let too_big = "x".repeat(MAX_OBJECTIVE_CHARS + 1);
        let err = validate_objective(&too_big).unwrap_err();
        assert!(err.contains("limit is"));
        // Operator-facing recovery hint must include the file-affordance.
        // Without it the user is told the limit but not what to do about it,
        // so we'd lose the "put it in a file" workaround for long objectives.
        assert!(
            err.contains("docs/goal.md"),
            "too-long error must include the file-hint suggestion: {err}"
        );
    }

    #[test]
    fn validate_objective_counts_unicode_scalars_not_bytes() {
        // Each `é` is 2 bytes but 1 scalar value; MAX_OBJECTIVE_CHARS scalars must pass
        // even though byte length is 2x.
        let s = "é".repeat(MAX_OBJECTIVE_CHARS);
        assert!(validate_objective(&s).is_ok());
    }

    #[test]
    fn validate_budget_rejects_zero_and_negative() {
        assert!(validate_budget(Some(0), None).is_err());
        assert!(validate_budget(Some(-1), None).is_err());
    }

    #[test]
    fn validate_budget_accepts_none_and_positive() {
        assert_eq!(validate_budget(None, None).unwrap(), None);
        assert_eq!(validate_budget(Some(100), None).unwrap(), Some(100));
    }

    /// Spec: the ceiling is what keeps a budget meaningful. Without it a model
    /// can ask for a number large enough that the Ralph loop is effectively
    /// unbounded again.
    #[test]
    fn validate_budget_enforces_the_ceiling() {
        let max = Some(1_000);
        assert_eq!(validate_budget(Some(999), max).unwrap(), Some(999));
        assert_eq!(
            validate_budget(Some(1_000), max).unwrap(),
            Some(1_000),
            "the ceiling itself must be allowed"
        );

        let err = validate_budget(Some(1_001), max).expect_err("above the ceiling");
        assert!(
            err.contains("1001") && err.contains("1000"),
            "the error must name both numbers: {err}"
        );
        assert!(
            err.contains("goals.max_token_budget"),
            "the error must name the setting that would allow it: {err}"
        );
    }

    /// An unset ceiling must not start rejecting budgets that were valid
    /// before — the default is still unbounded.
    #[test]
    fn validate_budget_without_a_ceiling_is_unchanged() {
        assert_eq!(
            validate_budget(Some(i64::MAX), None).unwrap(),
            Some(i64::MAX)
        );
    }
}
