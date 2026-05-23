//! OTEL instruments for the per-session goal subsystem.
//!
//! Instruments are lazy `LazyLock`s on the global meter; when OTEL isn't
//! configured, the no-op meter discards them with no runtime cost.

use std::sync::LazyLock;

use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::{global, KeyValue};

use super::Goal;

const SCOPE: &str = "borg.goals";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(SCOPE));

static GOAL_CREATED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("borg.goal.created")
        .with_description("Per-session goals created.")
        .build()
});

static GOAL_COMPLETED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("borg.goal.completed")
        .with_description("Per-session goals marked complete.")
        .build()
});

static GOAL_BUDGET_LIMITED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("borg.goal.budget_limited")
        .with_description("Per-session goals transitioned to budget_limited.")
        .build()
});

static GOAL_BLOCKED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("borg.goal.blocked")
        .with_description("Per-session goals the agent self-reported as blocked.")
        .build()
});

static GOAL_TOKEN_COUNT: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("borg.goal.token_count")
        .with_description("Tokens consumed by a goal at terminal transition.")
        .with_unit("token")
        .build()
});

static GOAL_DURATION_SECONDS: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    METER
        .u64_histogram("borg.goal.duration_seconds")
        .with_description("Wall-clock duration a goal was Active before terminal transition.")
        .with_unit("s")
        .build()
});

/// Increment `borg.goal.created`. The `budget` attribute distinguishes
/// budgeted from unbounded goals so dashboards can split cost-forecast
/// cohorts — string values render more cleanly in Prometheus exporters
/// than booleans.
pub fn record_created(goal: &Goal) {
    let budget = if goal.token_budget.is_some() {
        "budgeted"
    } else {
        "unbounded"
    };
    GOAL_CREATED.add(1, &[KeyValue::new("budget", budget)]);
}

/// Increment `borg.goal.completed` and record terminal token / duration
/// histograms. The `outcome="completed"` attribute on the histograms lets
/// dashboards split the cohort from budget-limited terminals.
pub fn record_completed(goal: &Goal) {
    let attrs = [KeyValue::new("outcome", "completed")];
    GOAL_COMPLETED.add(1, &[]);
    GOAL_TOKEN_COUNT.record(goal.tokens_used.max(0) as u64, &attrs);
    GOAL_DURATION_SECONDS.record(goal.time_used_seconds.max(0) as u64, &attrs);
}

/// Increment `borg.goal.budget_limited` and record terminal token /
/// duration histograms. Called exactly once at the budget-crossing
/// transition (the DB-level `account_goal_usage` is the sole caller).
pub fn record_budget_limited(goal: &Goal) {
    let attrs = [KeyValue::new("outcome", "budget_limited")];
    GOAL_BUDGET_LIMITED.add(1, &[]);
    GOAL_TOKEN_COUNT.record(goal.tokens_used.max(0) as u64, &attrs);
    GOAL_DURATION_SECONDS.record(goal.time_used_seconds.max(0) as u64, &attrs);
}

/// Increment `borg.goal.blocked` and record terminal token / duration
/// histograms. Called from the `update_goal` tool when the model self-
/// reports a repeated impasse via `status="blocked"`.
pub fn record_blocked(goal: &Goal) {
    let attrs = [KeyValue::new("outcome", "blocked")];
    // Counter name already encodes the outcome; match the existing
    // `record_completed` / `record_budget_limited` convention of
    // attaching the `outcome` attr only to the shared cohort histograms.
    GOAL_BLOCKED.add(1, &[]);
    GOAL_TOKEN_COUNT.record(goal.tokens_used.max(0) as u64, &attrs);
    GOAL_DURATION_SECONDS.record(goal.time_used_seconds.max(0) as u64, &attrs);
}
