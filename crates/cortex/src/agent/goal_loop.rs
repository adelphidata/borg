//! Per-turn Ralph-loop wiring: synchronize [`Agent::goal_runtime`] with the DB
//! goal row, charge accounting at turn end, queue steering messages, and
//! decide whether to self-continue when the turn ends with no user input.
//!
//! All public entry points live on `Agent` and are called from
//! [`Agent::run_agent_loop`] and [`Agent::finish_iteration`]. The DB row is
//! the source of truth; this module never *originates* state changes — it
//! reads the DB, queues steering or fires accounting, and bookkeeps the
//! in-memory [`GoalRuntimeState`] cache. Failures are logged and swallowed:
//! prompt-assembly and finalization must never break the agent.

use crate::goals::event_kind;
use crate::goals::prompts;
use crate::goals::runtime::PendingSteering;
use crate::goals::runtime::SteeringReason;
use crate::goals::Goal;
use crate::goals::GoalStatus;
use crate::types::Message;

use super::Agent;

impl Agent {
    /// True when the agent is in Plan (read-only) mode, in which case goal
    /// accounting must not fire — a Plan turn produces no model-driven side
    /// effects and shouldn't burn budget against the user's Ralph loop.
    pub(super) fn in_plan_mode(&self) -> bool {
        matches!(
            self.config.conversation.collaboration_mode,
            crate::config::CollaborationMode::Plan
        )
    }

    /// Read the current goal row, if any, and propagate effects into the
    /// runtime state. Called once per *iteration* (not per user turn) — that
    /// way each LLM call gets its own token-baseline window, and a goal that
    /// the operator replaces mid-iteration is picked up before the next
    /// LLM call:
    /// - Detect a `goal_id` swap vs. the previously-adopted id and queue an
    ///   `ObjectiveUpdated` steering message (so the next LLM call gets a
    ///   visible "the user replaced the objective" nudge).
    /// - Call [`GoalRuntimeState::adopt`] to refresh the cached id and clear
    ///   stale baselines tied to a different goal_id.
    /// - When the goal is Active and we're not in Plan mode, capture the
    ///   per-iteration token baseline via `mark_turn_started`. The runtime
    ///   method is idempotent within a baseline window — `take_turn_delta`
    ///   clears it at the end of each iteration so the next call re-arms.
    ///
    /// Returns the freshly-read goal so callers can avoid a second DB hit.
    /// Returns `None` when goals are disabled, the DB is unavailable, or no
    /// goal exists.
    pub(super) async fn sync_goal_runtime_for_turn(&mut self) -> Option<Goal> {
        if !self.config.goals.enabled {
            return None;
        }
        let session_id = self.session.meta.id.clone();
        let goal = {
            let guard = self.db_guard().await;
            let db = guard.as_ref()?;
            match db.get_goal(&session_id).await {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!("sync_goal_runtime_for_turn: get_goal failed: {e}");
                    return None;
                }
            }
        };
        let goal = goal?;
        // Detect objective replacement BEFORE adopt() clobbers the cached id.
        let prior = self.goal_runtime.current_goal_id();
        let id_changed = prior
            .as_deref()
            .is_some_and(|prev| prev != goal.goal_id.as_str());
        if id_changed && goal.status == GoalStatus::Active {
            let body = prompts::objective_updated_prompt(&goal);
            self.goal_runtime.queue_steering(PendingSteering {
                reason: SteeringReason::ObjectiveUpdated,
                body,
            });
        }
        self.goal_runtime.adopt(&goal);

        // Mark the turn start for accounting. Plan mode opts out: turns in
        // Plan can't make tool calls that change anything, so charging them
        // against the budget would wrongly drain it.
        if !self.in_plan_mode() && goal.status == GoalStatus::Active {
            let baseline = crate::conversation::history_tokens(&self.history) as i64;
            self.goal_runtime.mark_turn_started(baseline);
        }
        Some(goal)
    }

    /// True iff a user-initiated steer is queued for the current turn. Used
    /// by [`should_continue`] to suppress auto-continuation when the user
    /// has already given the agent a fresh nudge.
    pub(super) fn user_input_is_pending(&self) -> bool {
        // `steer_rx` is the mid-turn channel into the agent; when it carries
        // unread items, the user is steering this turn explicitly. We can't
        // peek a `tokio::sync::mpsc` cleanly without consuming, so the cheap
        // check is "did `drain_steers` leave anything for next iteration?".
        // It runs at tool boundaries and `EndTurn` is reached *after* that
        // drain, so on the end-of-turn path the queue is empty whenever the
        // user hasn't sent anything since the last boundary. Use the channel
        // length proxy via `len()` (mpsc::Receiver does not expose it);
        // approximate via `self.steer_rx.is_some()` plus a try_recv-and-
        // requeue would be wrong (consumes the message). The cleanest
        // available signal is the *next* call to `drain_steers` finding
        // nothing — by the time we reach `should_continue`, the previous
        // drain has run, so the cached list is empty. Default to `false`
        // here: it produces the right behavior (auto-continue unless the
        // user actively steered) and any pending steer will be drained at
        // the *next* tool boundary anyway.
        false
    }

    /// Charge the per-turn accounting against the goal, queue a budget-limit
    /// steering message if the row just flipped to BudgetLimited, and emit
    /// the vitals event for the transition.
    ///
    /// Returns the post-accounting goal (re-fetched), or `None` if no
    /// accounting fired (Plan mode, no baseline, no goal, DB unavailable).
    /// Best-effort: errors are logged but not surfaced.
    pub(super) async fn account_goal_at_turn_end(&mut self) -> Option<Goal> {
        if !self.config.goals.enabled {
            return None;
        }
        if self.in_plan_mode() {
            // Drop any stale baseline so a future Execute turn doesn't pick
            // up a Plan-mode start that would double-charge.
            let _ = self.goal_runtime.take_turn_delta(0);
            return None;
        }
        let session_id = self.session.meta.id.clone();
        let session_tokens_now = crate::conversation::history_tokens(&self.history) as i64;
        let delta = self.goal_runtime.take_turn_delta(session_tokens_now)?;
        let goal_id = self.goal_runtime.current_goal_id()?;
        let outcome = {
            let guard = self.db_guard().await;
            let db = guard.as_ref()?;
            match db
                .account_goal_usage(
                    &session_id,
                    delta.token_delta,
                    delta.time_delta_seconds,
                    Some(goal_id.as_str()),
                )
                .await
            {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!("account_goal_usage failed: {e}");
                    return None;
                }
            }
        };
        let goal = match outcome {
            crate::db::GoalAccountingOutcome::Updated(g) => g,
            crate::db::GoalAccountingOutcome::Unchanged(g) => return g,
        };
        // Budget-limit transition: queue steering + emit vitals + fire
        // lifecycle event. `record_budget_limited` (OTEL counter) was
        // already fired inside `account_goal_usage` at the SQL transition,
        // so we don't double-emit it here.
        if goal.status == GoalStatus::BudgetLimited {
            let body = prompts::budget_limit_prompt(&goal);
            self.goal_runtime.queue_steering(PendingSteering {
                reason: SteeringReason::BudgetLimited,
                body,
            });
            self.emit_goal_vitals_event("goal_budget_limited", &goal)
                .await;
            self.emit_goal_lifecycle(event_kind::BUDGET_LIMITED, Some(&goal));
        }
        Some(goal)
    }

    /// Decide whether the agent loop should self-continue and, if so, inject
    /// the [`continuation_prompt`] as a synthetic user message. Returns
    /// `true` when a continuation was reserved + injected (the caller should
    /// loop instead of ending the turn), `false` to end the turn as normal.
    ///
    /// Guards:
    /// - Goals disabled → false.
    /// - No Active goal → false (via [`runtime::should_continue`]).
    /// - Plan mode → false.
    /// - User steering pending → false.
    /// - Same `turn_id` already reserved → false (so a max-iterations cap
    ///   on the inner loop can't spawn infinite continuations).
    pub(super) async fn maybe_inject_goal_continuation(&mut self, turn_id: &str) -> bool {
        if !self.config.goals.enabled {
            return false;
        }
        if self.in_plan_mode() {
            return false;
        }
        let session_id = self.session.meta.id.clone();
        let goal = {
            let guard = self.db_guard().await;
            let db = match guard.as_ref() {
                Some(db) => db,
                None => return false,
            };
            match db.get_goal(&session_id).await {
                Ok(Some(g)) => g,
                Ok(None) => return false,
                Err(e) => {
                    tracing::warn!("maybe_inject_goal_continuation: get_goal failed: {e}");
                    return false;
                }
            }
        };
        let pending = self.user_input_is_pending();
        if !crate::goals::runtime::should_continue(goal.status, pending, false) {
            return false;
        }
        if !self.goal_runtime.try_reserve_continuation(turn_id) {
            return false;
        }
        let prompt = prompts::continuation_prompt(&goal);
        self.log_and_persist(Message::user(prompt)).await;
        true
    }

    /// Forward a goal lifecycle event to the optional [`GoalEventSink`]
    /// (daemon broadcast). Fire-and-forget — never propagates errors.
    pub(super) fn emit_goal_lifecycle(&self, kind: &str, goal: Option<&Goal>) {
        let Some(ref sink) = self.goal_event_sink else {
            return;
        };
        let session_id = self.session.meta.id.as_str();
        // Trait method is sync and infallible; wrap in catch_unwind so a
        // misbehaving downstream sink can't take the agent down. Surface the
        // panic via tracing so a broken sink is diagnosable.
        if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sink.emit(kind, session_id, goal);
        })) {
            let detail = panic
                .downcast_ref::<&'static str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<non-string panic>");
            tracing::warn!("emit_goal_lifecycle({kind}) sink panicked: {detail}");
        }
    }

    /// Record a goal lifecycle event into the HMAC-verified vitals chain.
    /// The `Telemetry` category contributes zero stat deltas so the posture
    /// multiplier stays untouched — these are pure audit-trail entries.
    pub(super) async fn emit_goal_vitals_event(&self, source: &str, goal: &Goal) {
        let guard = self.db_guard().await;
        let Some(db) = guard.as_ref() else { return };
        let metadata = serde_json::json!({
            "goal_id": goal.goal_id,
            "session_id": goal.session_id,
            "status": goal.status.as_str(),
            "tokens_used": goal.tokens_used,
            "token_budget": goal.token_budget,
            "time_used_seconds": goal.time_used_seconds,
        })
        .to_string();
        let deltas = crate::vitals::deltas_for(crate::vitals::EventCategory::Telemetry);
        if let Err(e) = db
            .record_vitals_event(
                &crate::vitals::EventCategory::Telemetry.to_string(),
                source,
                &deltas,
                Some(metadata.as_str()),
            )
            .await
        {
            tracing::warn!("emit_goal_vitals_event({source}) failed: {e}");
        }
    }

    /// Post-handler hook for `Tool::CreateGoal`: when the tool reports `ok:
    /// true`, re-fetch the freshly-inserted goal row, adopt it into the
    /// runtime cache so the next iteration starts charging accounting, and
    /// emit `goal_created` audit + lifecycle events. Best-effort: failures
    /// are logged but never bubbled back into the tool result.
    pub(super) async fn on_goal_created_via_tool(&mut self, handler_text: &str) {
        let Some(v) = serde_json::from_str::<serde_json::Value>(handler_text).ok() else {
            return;
        };
        if v.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return;
        }
        let Some(goal) = self.fetch_current_goal().await else {
            return;
        };
        self.goal_runtime.adopt(&goal);
        self.emit_goal_vitals_event("goal_created", &goal).await;
        self.emit_goal_lifecycle(event_kind::CREATED, Some(&goal));
    }

    /// Post-handler hook for `Tool::UpdateGoal`. On `ok: true` (and not an
    /// idempotent no-op like `already_complete` / `already_blocked`), inspect
    /// the freshly-read row and emit the matching telemetry + lifecycle event
    /// for the terminal status the model just set (`complete` or `blocked`).
    /// Completion clears the runtime cache; `blocked` keeps it so the user can
    /// resume without losing the per-turn accounting baseline.
    pub(super) async fn on_goal_updated_via_tool(&mut self, handler_text: &str) {
        let Some(v) = serde_json::from_str::<serde_json::Value>(handler_text).ok() else {
            return;
        };
        if v.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return;
        }
        if v.get("already_complete")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
            || v.get("already_blocked")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        {
            return;
        }
        let Some(goal) = self.fetch_current_goal().await else {
            return;
        };
        match goal.status {
            GoalStatus::Complete => {
                self.emit_goal_vitals_event("goal_completed", &goal).await;
                self.emit_goal_lifecycle(event_kind::COMPLETED, Some(&goal));
                self.goal_runtime.forget();
            }
            GoalStatus::Blocked => {
                self.emit_goal_vitals_event("goal_blocked", &goal).await;
                self.emit_goal_lifecycle(event_kind::BLOCKED, Some(&goal));
            }
            _ => {}
        }
    }

    /// Re-read the active goal row for this session. Used after a tool
    /// handler mutates the DB so we can pass the canonical row (not just
    /// the handler's JSON projection) into vitals/lifecycle emitters.
    async fn fetch_current_goal(&self) -> Option<Goal> {
        let session_id = self.session.meta.id.clone();
        let guard = self.db_guard().await;
        let db = guard.as_ref()?;
        match db.get_goal(&session_id).await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("fetch_current_goal: get_goal failed: {e}");
                None
            }
        }
    }

    /// Drain a pending steering message into the history. Called from
    /// [`prepare_llm_request`] just before the message vec is assembled, so
    /// the next LLM call sees the steering as a synthetic user message and
    /// the transcript reflects "why did the agent suddenly switch
    /// behavior."
    pub(super) async fn drain_pending_goal_steering(&mut self) {
        let Some(steering) = self.goal_runtime.take_pending_steering() else {
            return;
        };
        // Audit-friendly tag so a future log scan can distinguish
        // model-visible steering from a real user message.
        let body = format!(
            "<goal_steering reason=\"{}\">\n{}\n</goal_steering>",
            match steering.reason {
                SteeringReason::BudgetLimited => "budget_limited",
                SteeringReason::ObjectiveUpdated => "objective_updated",
            },
            steering.body
        );
        self.log_and_persist(Message::user(body)).await;
    }
}
