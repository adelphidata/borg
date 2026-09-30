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
use crate::goals::gates::GateVerdict;
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

    /// True iff a user-initiated steer is queued and not yet consumed. Used by
    /// [`maybe_inject_goal_continuation`] to suppress auto-continuation when the
    /// user is actively steering, so a queued user turn drives the next
    /// iteration instead of a synthetic continuation nudge.
    ///
    /// `steer_rx` is the mid-turn channel into the agent. `drain_steers` runs
    /// at tool boundaries, but the end-of-turn path taken by a *text-only*
    /// response (`tool_calls.is_empty()` → `EndTurn`) never hits that drain, so
    /// a steer that landed after the last boundary is still buffered here. If we
    /// injected a continuation nudge in that state, the user's fresh input would
    /// be delayed behind a synthetic message it should have superseded. We peek
    /// the queue non-destructively via `is_empty()` (tokio ≥1.30 exposes it on
    /// the unbounded receiver) rather than `try_recv`, which would consume the
    /// message and drop it.
    pub(super) fn user_input_is_pending(&self) -> bool {
        self.steer_rx.as_ref().is_some_and(|rx| !rx.is_empty())
    }

    /// At the end of an otherwise-final turn, absorb a user steer that landed
    /// after the last tool boundary. Because `drain_steers` only runs at tool
    /// boundaries, a text-only `EndTurn` would otherwise strand the buffered
    /// steer in `steer_rx` until an unrelated future turn injected it out of
    /// context — the agent would simply halt and the user's redirect would be
    /// lost. Draining it into history here and returning `true` lets the caller
    /// re-enter the loop so the model responds to the redirect immediately;
    /// this is the correct resolution when a pending steer suppresses the goal
    /// continuation. Returns `false` (no-op) when nothing is buffered.
    pub(super) async fn absorb_pending_steer_at_turn_end(
        &mut self,
        event_tx: &tokio::sync::mpsc::Sender<crate::agent::AgentEvent>,
    ) -> bool {
        if !self.user_input_is_pending() {
            return false;
        }
        self.drain_steers(event_tx).await;
        true
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
            // The `0` is a discard, not a measurement: taking the delta clears
            // the baseline, and the returned value is meaningless in Plan mode.
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
    pub(super) async fn maybe_inject_goal_continuation(
        &mut self,
        turn_id: &str,
        event_tx: &tokio::sync::mpsc::Sender<crate::agent::AgentEvent>,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> bool {
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

        // Gates run BEFORE the `should_continue` short-circuit, because the
        // case they exist for is precisely the one that short-circuit swallows:
        // the model marks the goal `complete` mid-turn, and by the time we get
        // here the row is no longer Active. Checking gates only on the
        // continue path would make them a nicer continuation prompt rather than
        // a completion contract — a red suite could still be declared done.
        //
        // Only `Complete` is re-opened. Paused / Blocked / BudgetLimited were
        // parked deliberately by the operator or the budget guard, and a gate
        // must not drag the loop back out of them.
        let gating = matches!(goal.status, GoalStatus::Active | GoalStatus::Complete)
            && !pending
            && !self.in_plan_mode();
        let verdict = if gating {
            self.evaluate_goal_gates(cancel).await
        } else {
            GateVerdict::AllPassed
        };

        // A failing gate on a goal the model just completed reverts it: the
        // completion was not true, and leaving the row Complete would both lie
        // in `/goal status` and stop the loop that has to fix it.
        if goal.status == GoalStatus::Complete && !matches!(verdict, GateVerdict::AllPassed) {
            if !self.reopen_completed_goal_for_failed_gate().await {
                // Could not revert the row; do not inject a continuation that
                // contradicts the persisted state.
                return false;
            }
        } else if !crate::goals::runtime::should_continue(goal.status, pending, false) {
            return false;
        }
        if !self.goal_runtime.try_reserve_continuation(turn_id) {
            return false;
        }

        let prompt = match verdict {
            GateVerdict::AllPassed => prompts::continuation_prompt(&goal),
            GateVerdict::Failed { prompt, .. } => prompt,
            GateVerdict::Exhausted { message, .. } => {
                // The agent has had its retries and cannot satisfy the check.
                // Continuing would burn the token budget re-failing, so park
                // the goal for the operator exactly as the budget cap does.
                self.pause_goal_for_failed_gate(&message, event_tx).await;
                self.goal_runtime.clear_continuation_reservation();
                return false;
            }
        };
        self.log_and_persist(Message::user(prompt)).await;
        true
    }

    /// Flip a `Complete` goal back to `Active` because a gate says it is not
    /// done. Returns whether the row now reads Active.
    async fn reopen_completed_goal_for_failed_gate(&self) -> bool {
        let session_id = self.session.meta.id.clone();
        let updated = {
            let guard = self.db_guard().await;
            match guard.as_ref() {
                Some(db) => {
                    db.update_goal(
                        &session_id,
                        crate::db::GoalUpdate {
                            status: Some(GoalStatus::Active),
                            ..Default::default()
                        },
                    )
                    .await
                }
                None => Ok(None),
            }
        };
        match updated {
            Ok(Some(goal)) => {
                tracing::info!(
                    session_id = %session_id,
                    "goal marked complete but a quality gate is failing; reopening"
                );
                self.goal_runtime.adopt(&goal);
                self.emit_goal_lifecycle(event_kind::STATUS_CHANGED, Some(&goal));
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!("goal gates: reopening the completed goal failed: {e}");
                false
            }
        }
    }

    /// Run the active goal's quality gates and report the verdict.
    ///
    /// Best-effort like the rest of this module: if the DB is unreachable the
    /// gates are treated as passing, because a storage hiccup must not
    /// permanently wedge a goal loop that is otherwise healthy.
    async fn evaluate_goal_gates(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> GateVerdict {
        let session_id = self.session.meta.id.clone();
        let gates = {
            let guard = self.db_guard().await;
            let Some(db) = guard.as_ref() else {
                return GateVerdict::AllPassed;
            };
            match db.list_goal_gates(&session_id).await {
                Ok(gates) => gates,
                Err(e) => {
                    tracing::warn!("goal gates: list_goal_gates failed: {e}");
                    return GateVerdict::AllPassed;
                }
            }
        };
        if gates.is_empty() {
            return GateVerdict::AllPassed;
        }

        // One fingerprint for the whole pass: the workspace cannot change
        // between gates, and probing per gate would double the git calls.
        // It feeds the retry cap only, never the decision to run a gate.
        let fingerprint =
            crate::git::workspace_fingerprint(self.execution_context.workspace_cwd()).await;
        // Both are operator-settable and unvalidated at the settings layer. A
        // zero or negative cap would pause the goal on the first failure with a
        // "failed 0 times" message; a zero timeout would fail every gate
        // instantly. Clamp at the point of use.
        let max_attempts = self.config.goals.gate_max_attempts.max(1);
        let timeout =
            std::time::Duration::from_millis(self.config.goals.gate_timeout_ms.max(1_000));

        for gate in &gates {
            // A gate stored before the add-time check, or edited in the DB by
            // hand, can carry a command whose exit code is always 0. Running it
            // would report a pass it never earned, so stop the goal for the
            // operator instead — retries cannot fix the command's shape.
            if let Some(reason) = crate::goals::gates::unverifiable_reason(&gate.command) {
                tracing::warn!(
                    ordinal = gate.ordinal,
                    command = %gate.command,
                    "goal gate: exit status cannot be attributed to the command"
                );
                return GateVerdict::Exhausted {
                    ordinal: gate.ordinal,
                    message: format!(
                        "Goal paused: quality gate {} (`{}`) cannot be verified — {reason} \
                         Fix it with `/goal gate remove {}` and add it again.",
                        gate.ordinal, gate.command, gate.ordinal
                    ),
                };
            }
            // Always a real run. A recorded failure is never reused: no cheap
            // signal proves the result would be the same (the agent can repair
            // an ignored file, or an input outside the repository), and a
            // reused failure turns a real fix into a paused goal. The retry cap
            // below is what bounds a suite that stays red.
            let run = crate::tool_handlers::shell::run_gate_command(
                &self.execution_context,
                &self.config,
                &self.policy,
                &gate.command,
                timeout,
                cancel,
            )
            .await;
            let (exit_code, output) = (i64::from(run.exit_code), run.output);

            // The retry cap counts failures the agent made *no progress*
            // against, not failing turns. A turn-boundary count gives a Ralph
            // loop ~3 edit-compile-test cycles before the goal is parked, which
            // is far too tight; what actually signals "stuck" is failing again
            // the same way without having touched the workspace.
            let made_progress =
                crate::goals::gates::made_progress(gate, fingerprint.as_deref(), exit_code);

            // Record every run, so a repeated failure on an untouched
            // workspace advances the attempt count rather than looping forever.
            {
                let guard = self.db_guard().await;
                if let Some(db) = guard.as_ref() {
                    if let Err(e) = db
                        .record_goal_gate_run(
                            &session_id,
                            gate.ordinal,
                            exit_code,
                            crate::goals::gates::output_tail(&output),
                            fingerprint.as_deref(),
                            made_progress,
                        )
                        .await
                    {
                        tracing::warn!("goal gates: record_goal_gate_run failed: {e}");
                    }
                }
            }

            if exit_code == 0 {
                continue;
            }
            // `attempts` is the count *before* this run. A run that made
            // progress restarts the budget at 1, so it can only exhaust the cap
            // when the cap is 1.
            let attempts_now = if made_progress { 1 } else { gate.attempts + 1 };
            if attempts_now >= max_attempts {
                return GateVerdict::Exhausted {
                    ordinal: gate.ordinal,
                    message: crate::goals::gates::exhausted_message(gate, max_attempts),
                };
            }
            return GateVerdict::Failed {
                ordinal: gate.ordinal,
                prompt: crate::goals::gates::failure_prompt(gate, exit_code, &output),
            };
        }
        GateVerdict::AllPassed
    }

    /// Park the goal after a gate exhausted its retries and tell the user why.
    async fn pause_goal_for_failed_gate(
        &self,
        message: &str,
        event_tx: &tokio::sync::mpsc::Sender<crate::agent::AgentEvent>,
    ) {
        let session_id = self.session.meta.id.clone();
        let paused = {
            let guard = self.db_guard().await;
            match guard.as_ref() {
                Some(db) => db.pause_active_goal(&session_id).await,
                None => Ok(None),
            }
        };
        match paused {
            Ok(Some(goal)) => {
                tracing::warn!(session_id = %session_id, "{message}");
                self.emit_goal_lifecycle(event_kind::STATUS_CHANGED, Some(&goal));
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("goal gates: pause_active_goal failed: {e}"),
        }
        // Surfaced as a notice, not just a log line: the loop stopping without
        // an explanation reads as the agent giving up for no reason.
        if event_tx
            .send(crate::agent::AgentEvent::SystemNotice {
                category: "goal".to_string(),
                text: message.to_string(),
            })
            .await
            .is_err()
        {
            tracing::trace!("goal gates: event channel closed before the pause notice");
        }
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

#[cfg(test)]
mod gate_tests {
    use super::*;
    use crate::db::Database;

    fn git(repo: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with one commit that holds `flag` = "bad" and `other`.
    fn committed_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        git(repo, &["init", "-q"]);
        std::fs::write(repo.join("flag"), "bad\n").unwrap();
        std::fs::write(repo.join("other"), "one\n").unwrap();
        git(repo, &["add", "."]);
        git(
            repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        dir
    }

    /// An agent in `workspace` with one gate on its session.
    async fn agent_with_gate(
        workspace: &std::path::Path,
        command: &str,
        max_attempts: i64,
    ) -> Agent {
        let mut config = crate::config::Config::default();
        config.goals.gate_max_attempts = max_attempts;
        let mut agent = super::super::tests::minimal_test_agent(config);
        agent.execution_context =
            crate::execution_context::ExecutionContext::unrestricted(workspace.to_path_buf());

        let db = Database::test_db().await;
        let session_id = agent.session.meta.id.clone();
        db.upsert_session(&session_id, 0, 0, 0, "test-model", "test")
            .await
            .expect("seed session row");
        db.add_goal_gate(&session_id, command)
            .await
            .expect("add gate");
        *agent.db.lock().await = Some(db);
        agent
    }

    async fn only_gate(agent: &Agent) -> crate::db::GoalGate {
        let guard = agent.db.lock().await;
        let db = guard.as_ref().expect("db");
        let mut gates = db
            .list_goal_gates(&agent.session.meta.id)
            .await
            .expect("list gates");
        assert_eq!(gates.len(), 1);
        gates.remove(0)
    }

    /// Regression: the agent repairs a file that is already modified. The
    /// `git status` text is the same before and after, so a verdict keyed on
    /// it reported the old failure and the goal was paused on a green check.
    #[tokio::test]
    async fn a_fix_in_an_already_dirty_file_passes_the_gate() {
        let repo = committed_repo();
        let cancel = tokio_util::sync::CancellationToken::new();
        let agent = agent_with_gate(repo.path(), "grep -q good flag", 3).await;

        // The tree is dirty and the check is red.
        std::fs::write(repo.path().join("flag"), "bad\nstill bad\n").unwrap();
        let first = agent.evaluate_goal_gates(&cancel).await;
        assert!(
            matches!(first, GateVerdict::Failed { ordinal: 1, .. }),
            "the check is red: {first:?}"
        );
        assert_eq!(only_gate(&agent).await.attempts, 1);

        // The fix lands in the same, already modified file.
        let status = |repo: &std::path::Path| {
            let out = std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(repo)
                .output()
                .expect("git status");
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let before = status(repo.path());
        std::fs::write(repo.path().join("flag"), "good\n").unwrap();
        assert_eq!(
            before,
            status(repo.path()),
            "precondition: the status text cannot see the fix"
        );

        let second = agent.evaluate_goal_gates(&cancel).await;
        assert_eq!(second, GateVerdict::AllPassed);
        let gate = only_gate(&agent).await;
        assert_eq!(gate.last_exit_code, Some(0));
        assert_eq!(gate.attempts, 0, "a pass resets the retry budget");
    }

    /// A fix that git cannot see at all (the input is outside the
    /// repository) must pass too: the gate runs for real on every boundary.
    #[tokio::test]
    async fn a_fix_outside_the_repository_passes_the_gate() {
        let repo = committed_repo();
        let outside = tempfile::tempdir().expect("tempdir");
        let input = outside.path().join("input");
        std::fs::write(&input, "bad\n").unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let command = format!("grep -q good '{}'", input.display());
        let agent = agent_with_gate(repo.path(), &command, 3).await;

        let first = agent.evaluate_goal_gates(&cancel).await;
        assert!(matches!(first, GateVerdict::Failed { .. }), "{first:?}");

        std::fs::write(&input, "good\n").unwrap();
        assert_eq!(
            agent.evaluate_goal_gates(&cancel).await,
            GateVerdict::AllPassed
        );
    }

    /// The retry cap still bounds an agent that is stuck: the same failure
    /// against an untouched workspace reaches the cap and parks the goal.
    #[tokio::test]
    async fn the_same_failure_without_a_change_exhausts_the_retries() {
        let repo = committed_repo();
        let cancel = tokio_util::sync::CancellationToken::new();
        let agent = agent_with_gate(repo.path(), "grep -q good flag", 3).await;

        for attempt in 1..=2 {
            let verdict = agent.evaluate_goal_gates(&cancel).await;
            assert!(
                matches!(verdict, GateVerdict::Failed { ordinal: 1, .. }),
                "attempt {attempt}: {verdict:?}"
            );
            assert_eq!(only_gate(&agent).await.attempts, attempt);
        }
        let last = agent.evaluate_goal_gates(&cancel).await;
        assert!(
            matches!(last, GateVerdict::Exhausted { ordinal: 1, .. }),
            "the third identical failure must park the goal: {last:?}"
        );
    }

    /// Work in an already dirty file that does not yet fix the check is
    /// still work. It must restart the retry budget, not use it up.
    #[tokio::test]
    async fn failed_work_in_a_dirty_file_does_not_use_up_the_retries() {
        let repo = committed_repo();
        let cancel = tokio_util::sync::CancellationToken::new();
        let agent = agent_with_gate(repo.path(), "grep -q good flag", 3).await;

        for round in 0..5 {
            std::fs::write(
                repo.path().join("flag"),
                format!("bad\n{}\n", "x".repeat(round + 1)),
            )
            .unwrap();
            let verdict = agent.evaluate_goal_gates(&cancel).await;
            assert!(
                matches!(verdict, GateVerdict::Failed { .. }),
                "round {round}: the goal must not be parked while the agent works: {verdict:?}"
            );
        }
        assert_eq!(only_gate(&agent).await.attempts, 1);
    }
}
