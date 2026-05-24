//! Per-session goal runtime state: in-memory accounting baselines, queued
//! steering messages, and continuation reservation.
//!
//! The DB (`crates/cortex/src/db/goals.rs`) is the source of truth for the
//! persisted [`Goal`] row. This module holds the *volatile* state that does
//! not survive a process restart:
//!
//! * the per-turn baseline (token usage + wall-clock instant) used to compute
//!   token/time deltas;
//! * the one-shot steering message queue (budget-limit / objective-updated)
//!   drained before the next LLM call;
//! * the continuation reservation that prevents a single turn from spawning
//!   more than one self-continuation.

use std::sync::Mutex;
use std::time::Instant;

use crate::goals::Goal;
use crate::goals::GoalStatus;

/// A pending steering message — a synthetic user-message body to be prepended
/// to the next LLM call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSteering {
    /// Why the steering was queued; used by tests and telemetry.
    pub reason: SteeringReason,
    /// The fully-rendered message body (already XML-escaped).
    pub body: String,
}

/// Discriminator for why a steering injection was queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteeringReason {
    /// Goal flipped to `budget_limited` mid-turn.
    BudgetLimited,
    /// User replaced the objective on an existing Active goal.
    ObjectiveUpdated,
}

/// Volatile runtime state for the goal subsystem.
#[derive(Debug, Default)]
pub struct GoalRuntimeState {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Accounting baseline for the in-flight turn. `None` between turns.
    turn_baseline: Option<TurnBaseline>,
    /// Goal id this state is currently tracking. Cleared when the goal id
    /// changes (objective replaced) or the goal is removed.
    active_goal_id: Option<String>,
    /// One-shot steering message waiting for the next LLM call.
    pending_steering: Option<PendingSteering>,
    /// Turn id that already reserved a continuation, so the agent loop does
    /// not double-trigger.
    continuation_reserved_turn: Option<String>,
}

#[derive(Debug)]
struct TurnBaseline {
    /// Total session tokens at turn start (so delta = current_total - baseline).
    session_tokens_at_start: i64,
    /// Wall-clock instant the turn started.
    started_at: Instant,
}

/// Outcome of an accounting step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountingDelta {
    /// Tokens charged this step.
    pub token_delta: i64,
    /// Wall-clock seconds charged this step.
    pub time_delta_seconds: i64,
}

impl GoalRuntimeState {
    /// Construct a fresh runtime container.
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the inner state, transparently recovering from a poisoned mutex.
    ///
    /// Panicking from prompt-assembly or accounting paths would take the agent
    /// down. Recover and continue with the inner value rather than letting a
    /// prior panic cascade.
    fn lock_inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!("goal runtime mutex poisoned — recovering");
                poisoned.into_inner()
            }
        }
    }

    /// Adopt a [`Goal`] as the current active one. Clears any baseline tied to
    /// a different goal id (so an objective replacement resets accounting).
    pub fn adopt(&self, goal: &Goal) {
        let mut inner = self.lock_inner();
        if inner.active_goal_id.as_deref() != Some(goal.goal_id.as_str()) {
            inner.turn_baseline = None;
            inner.continuation_reserved_turn = None;
            inner.active_goal_id = Some(goal.goal_id.clone());
        }
    }

    /// Snapshot the currently-adopted goal id, if any. Used by the agent
    /// loop to detect "the goal row's `goal_id` changed under us"
    /// (objective replacement) so an `ObjectiveUpdated` steering can be
    /// queued before the next LLM call.
    pub fn current_goal_id(&self) -> Option<String> {
        let inner = self.lock_inner();
        inner.active_goal_id.clone()
    }

    /// Drop the currently-tracked goal (cleared, completed, paused at user
    /// request, etc.). Subsequent accounting calls are no-ops until [`adopt`]
    /// is called again.
    pub fn forget(&self) {
        let mut inner = self.lock_inner();
        inner.active_goal_id = None;
        inner.turn_baseline = None;
        inner.continuation_reserved_turn = None;
        inner.pending_steering = None;
    }

    /// Mark the start of a turn. Captures the token baseline and wall-clock
    /// instant. Idempotent — re-calling within the same turn keeps the
    /// earliest baseline.
    pub fn mark_turn_started(&self, session_tokens_at_start: i64) {
        let mut inner = self.lock_inner();
        if inner.turn_baseline.is_none() {
            inner.turn_baseline = Some(TurnBaseline {
                session_tokens_at_start,
                started_at: Instant::now(),
            });
        }
    }

    /// Compute the accounting delta since [`mark_turn_started`] and clear the
    /// baseline. Returns `None` if no baseline was recorded (e.g. Plan mode).
    pub fn take_turn_delta(&self, session_tokens_now: i64) -> Option<AccountingDelta> {
        let mut inner = self.lock_inner();
        let baseline = inner.turn_baseline.take()?;
        let token_delta = session_tokens_now
            .saturating_sub(baseline.session_tokens_at_start)
            .max(0);
        let time_delta_seconds = baseline.started_at.elapsed().as_secs() as i64;
        Some(AccountingDelta {
            token_delta,
            time_delta_seconds,
        })
    }

    /// Queue a steering message for the next LLM call. If a message is
    /// already queued, the newer one wins (objective-updated supersedes a
    /// pending budget-limit).
    pub fn queue_steering(&self, steering: PendingSteering) {
        let mut inner = self.lock_inner();
        inner.pending_steering = Some(steering);
    }

    /// Drain the pending steering message, if any.
    pub fn take_pending_steering(&self) -> Option<PendingSteering> {
        let mut inner = self.lock_inner();
        inner.pending_steering.take()
    }

    /// Reserve a continuation for the given turn id. Returns `true` if this
    /// call reserved the slot; `false` if the slot was already held (the
    /// caller should *not* fire another continuation).
    pub fn try_reserve_continuation(&self, turn_id: &str) -> bool {
        let mut inner = self.lock_inner();
        if inner.continuation_reserved_turn.as_deref() == Some(turn_id) {
            return false;
        }
        inner.continuation_reserved_turn = Some(turn_id.to_string());
        true
    }

    /// Clear the continuation reservation (e.g. on user-initiated cancel).
    pub fn clear_continuation_reservation(&self) {
        let mut inner = self.lock_inner();
        inner.continuation_reserved_turn = None;
    }
}

/// Decide whether the agent loop should self-continue after a turn ends.
///
/// Returns `true` only when *all* of:
/// - the goal is Active (not Paused, BudgetLimited, Blocked, or Complete);
/// - user input is not pending;
/// - we are not in Plan mode.
pub fn should_continue(
    goal_status: GoalStatus,
    user_input_pending: bool,
    in_plan_mode: bool,
) -> bool {
    matches!(goal_status, GoalStatus::Active) && !user_input_pending && !in_plan_mode
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(id: &str, status: GoalStatus) -> Goal {
        Goal {
            session_id: "sess".into(),
            goal_id: id.into(),
            objective: "obj".into(),
            status,
            token_budget: None,
            tokens_used: 0,
            time_used_seconds: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn adopt_clears_baseline_when_goal_id_changes() {
        let rt = GoalRuntimeState::new();
        rt.adopt(&goal("g1", GoalStatus::Active));
        rt.mark_turn_started(100);
        rt.adopt(&goal("g2", GoalStatus::Active));
        // Baseline was tied to g1; switching to g2 must drop it.
        assert!(rt.take_turn_delta(500).is_none());
    }

    #[test]
    fn adopt_preserves_baseline_for_same_goal_id() {
        let rt = GoalRuntimeState::new();
        rt.adopt(&goal("g1", GoalStatus::Active));
        rt.mark_turn_started(100);
        rt.adopt(&goal("g1", GoalStatus::Active));
        let delta = rt.take_turn_delta(180).expect("delta present");
        assert_eq!(delta.token_delta, 80);
    }

    #[test]
    fn take_turn_delta_returns_none_without_baseline() {
        let rt = GoalRuntimeState::new();
        assert!(rt.take_turn_delta(100).is_none());
    }

    #[test]
    fn take_turn_delta_clamps_negative_to_zero() {
        let rt = GoalRuntimeState::new();
        rt.adopt(&goal("g1", GoalStatus::Active));
        rt.mark_turn_started(200);
        let delta = rt.take_turn_delta(150).unwrap();
        assert_eq!(delta.token_delta, 0);
    }

    #[test]
    fn mark_turn_started_is_idempotent_within_turn() {
        let rt = GoalRuntimeState::new();
        rt.adopt(&goal("g1", GoalStatus::Active));
        rt.mark_turn_started(100);
        rt.mark_turn_started(500); // ignored — first baseline wins
        let delta = rt.take_turn_delta(200).unwrap();
        assert_eq!(delta.token_delta, 100);
    }

    #[test]
    fn pending_steering_drained_once() {
        let rt = GoalRuntimeState::new();
        rt.queue_steering(PendingSteering {
            reason: SteeringReason::BudgetLimited,
            body: "x".into(),
        });
        assert!(rt.take_pending_steering().is_some());
        assert!(rt.take_pending_steering().is_none());
    }

    #[test]
    fn objective_updated_supersedes_pending_budget_limit() {
        let rt = GoalRuntimeState::new();
        rt.queue_steering(PendingSteering {
            reason: SteeringReason::BudgetLimited,
            body: "old".into(),
        });
        rt.queue_steering(PendingSteering {
            reason: SteeringReason::ObjectiveUpdated,
            body: "new".into(),
        });
        let drained = rt.take_pending_steering().unwrap();
        assert_eq!(drained.reason, SteeringReason::ObjectiveUpdated);
        assert_eq!(drained.body, "new");
    }

    #[test]
    fn continuation_reservation_locks_turn() {
        let rt = GoalRuntimeState::new();
        assert!(rt.try_reserve_continuation("turn-1"));
        // Same turn — second call must fail.
        assert!(!rt.try_reserve_continuation("turn-1"));
        // Different turn — succeeds.
        assert!(rt.try_reserve_continuation("turn-2"));
    }

    #[test]
    fn current_goal_id_tracks_adopt_and_forget() {
        let rt = GoalRuntimeState::new();
        // Fresh runtime has no adopted goal.
        assert_eq!(rt.current_goal_id(), None);
        rt.adopt(&goal("g1", GoalStatus::Active));
        assert_eq!(rt.current_goal_id().as_deref(), Some("g1"));
        // Re-adopting the same id is a no-op for the snapshot.
        rt.adopt(&goal("g1", GoalStatus::Paused));
        assert_eq!(rt.current_goal_id().as_deref(), Some("g1"));
        // Replacing surfaces the new id so the agent can detect the swap.
        rt.adopt(&goal("g2", GoalStatus::Active));
        assert_eq!(rt.current_goal_id().as_deref(), Some("g2"));
        rt.forget();
        assert_eq!(rt.current_goal_id(), None);
    }

    #[test]
    fn forget_drops_all_state() {
        let rt = GoalRuntimeState::new();
        rt.adopt(&goal("g1", GoalStatus::Active));
        rt.mark_turn_started(100);
        rt.queue_steering(PendingSteering {
            reason: SteeringReason::BudgetLimited,
            body: "x".into(),
        });
        rt.try_reserve_continuation("t1");
        rt.forget();
        assert!(rt.take_turn_delta(500).is_none());
        assert!(rt.take_pending_steering().is_none());
        assert!(rt.try_reserve_continuation("t1"));
    }

    #[test]
    fn should_continue_table() {
        // Active + idle + execute => continue
        assert!(should_continue(GoalStatus::Active, false, false));
        // Active + queued input => stop
        assert!(!should_continue(GoalStatus::Active, true, false));
        // Active + plan mode => stop
        assert!(!should_continue(GoalStatus::Active, false, true));
        // Paused / BudgetLimited / Blocked / Complete => stop
        for s in [
            GoalStatus::Paused,
            GoalStatus::BudgetLimited,
            GoalStatus::UsageLimited,
            GoalStatus::Blocked,
            GoalStatus::Complete,
        ] {
            assert!(!should_continue(s, false, false), "status={s:?}");
        }
    }
}
