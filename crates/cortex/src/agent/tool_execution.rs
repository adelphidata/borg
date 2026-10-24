//! Tool-call execution for the agent loop.
//!
//! Three entry points from `finish_iteration_inner` in [`super`]:
//! - [`Agent::execute_tool_calls`] — invalid-call handling + group planning
//! - `Agent::run_tool_calls` — sequential per-group execution
//! - `Agent::run_tool_group_parallel` — bounded-parallel execution for read-safe groups
//!
//! The free function [`dispatch_parallel_safe_tool`] is invoked by the parallel
//! path on each worker; its closed-set `match` keeps the planner and dispatcher
//! in sync at compile time.

use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info_span, instrument, warn, Instrument};

use crate::config::Config;
use crate::hooks::{HookAction, HookData, HookPoint};
use crate::rate_guard::RateDecision;
use crate::types::{ContentPart, FunctionCall, Message, ToolCall, ToolOutput};

use super::tool_classification::{classify_action, is_mutating_tool_call};
use super::tool_validation::{
    expensive_budget_block_message, format_invalid_tool_result, repeated_tool_call_block_message,
    unknown_tool_refusal_message, InvalidToolCall, RepeatDecision,
};
use super::{Agent, AgentEvent};

const GOALS_DISABLED_ERROR: &str = r#"{"error":"goals are disabled"}"#;
const DATABASE_UNAVAILABLE_ERROR: &str = r#"{"error":"database unavailable"}"#;

/// Tools exempt from the builtin wall-clock deadline, by name:
///
/// - `run_shell` — self-bounds via `timeout_ms` / `tools.default_timeout_ms`,
///   and a caller may legitimately request a budget above 300s (long builds).
/// - `request_user_input` — waits on a human; unbounded by design.
/// - `spawn_agent` / `wait_for_agent` — delegation can legitimately run a
///   full sub-agent turn; `wait_for_agent` carries its own caller-chosen
///   `timeout_secs` budget.
///
/// `mcp_`-prefixed tools are also exempt (checked by prefix in
/// [`builtin_tool_deadline`]): the MCP client layer owns its own per-request
/// watchdog and configured timeouts, which may exceed this deadline.
const DEADLINE_EXEMPT_TOOLS: &[&str] = &[
    crate::tool::names::RUN_SHELL,
    crate::tool::names::REQUEST_USER_INPUT,
    crate::tool::names::SPAWN_AGENT,
    crate::tool::names::WAIT_FOR_AGENT,
];

/// Deadline for one handler invocation of tool `name`, from
/// `tools.builtin_deadline_ms`. `None` means the tool runs unbounded: it is
/// exempt by name (see [`DEADLINE_EXEMPT_TOOLS`]) or the operator disabled
/// the deadline with `0`.
///
/// Cancellation between calls already exists (the `CancellationToken` checks
/// in `run_tool_calls`), but nothing bounds a single handler: a wedged
/// network fetch or hung subprocess wait would stall the turn forever.
fn builtin_tool_deadline(name: &str, deadline_ms: u64) -> Option<Duration> {
    if deadline_ms == 0 || name.starts_with("mcp_") || DEADLINE_EXEMPT_TOOLS.contains(&name) {
        None
    } else {
        Some(Duration::from_millis(deadline_ms))
    }
}

/// Synthetic tool result handed to the model when a handler blows its
/// wall-clock deadline. Mirrors the `Error:`-prefixed shape the validation
/// layer uses for its synthetic results so the model can address the failure
/// and the turn continues.
fn tool_deadline_exceeded_message(name: &str, deadline: Duration) -> String {
    format!(
        "Error: tool '{name}' timed out after {deadline:?} (wall-clock deadline) and the \
         in-flight operation was abandoned — its future was dropped, so the work \
         did not complete but any partial side effects may persist. Retry with a \
         smaller input, or use a different approach."
    )
}

/// Drive a builtin tool handler future under an optional wall-clock deadline.
///
/// On timeout the handler future is dropped (we do not detach it — nothing
/// silently keeps holding its resources in the background) and a synthetic
/// addressable error result is returned as `Ok(ToolOutput::Text(...))` so the
/// turn continues with a tool_result the model can react to. Handler errors
/// pass through untouched. `deadline` is a parameter (rather than reading the
/// constant inline) so tests can inject a short deadline instead of sleeping
/// 300s.
async fn run_with_deadline<F>(
    name: &str,
    deadline: Option<Duration>,
    handler: F,
) -> Result<ToolOutput>
where
    F: std::future::Future<Output = Result<ToolOutput>>,
{
    match deadline {
        None => handler.await,
        Some(limit) => match tokio::time::timeout(limit, handler).await {
            Ok(result) => result,
            Err(_) => {
                warn!(
                    tool.name = %name,
                    deadline_secs = limit.as_secs(),
                    "builtin tool exceeded wall-clock deadline; abandoning handler future \
                     and returning synthetic timeout result"
                );
                Ok(ToolOutput::Text(tool_deadline_exceeded_message(
                    name, limit,
                )))
            }
        },
    }
}

impl Agent {
    /// Call-time counterpart of the filtering `build_tool_definitions` applies
    /// to the advertised list: the sub-agent `tools_filter` allowlist and the
    /// `tools.policy` profile/allow/deny layers, through the same predicate.
    /// The `conditional_loading` relevance filter is deliberately not part of
    /// this — a tool it hides this turn is still a legitimate call.
    ///
    /// `tools_filter` binds only names with a catalog group, the same rule
    /// the policy's allow layer uses: the filter is a snapshot of the core
    /// definitions at spawn, and deferred MCP tools (`mcp.tool_search_mode`
    /// Auto), WASM plugins, and servers that connect later are never in it —
    /// they are discovered through `search_tools` and dispatched by registry.
    /// Those names stay callable (still subject to deny / `subagent_deny`);
    /// a forked child still cannot run a core tool it was not given.
    fn tool_available_in_session(&self, name: &str) -> bool {
        if let Some(allowed) = &self.tools_filter {
            if crate::tool::catalog::tool_group(name).is_some()
                && !allowed.iter().any(|t| t == name)
            {
                return false;
            }
        }
        crate::tool::policy::is_permitted(name, &self.config.tools.policy, self.spawn_depth > 0)
    }

    /// Persist the assistant message with tool calls, partition and execute them,
    /// running shell/browser sequentially and other tools in parallel.
    ///
    /// `invalid` holds tool calls the LLM emitted that failed validation
    /// (malformed JSON, oversized) but still have id+name. We surface them as
    /// synthetic `Error:`-prefixed tool_results both to the TUI (so the user
    /// sees the failure) and to the LLM history (so the next turn can retry).
    /// All shells — valid and invalid — go into the assistant message so the
    /// API contract "every tool_call has a matching tool_result" is preserved.
    pub(super) async fn execute_tool_calls(
        &mut self,
        tc: &[ToolCall],
        invalid: &[InvalidToolCall],
        text_content: &str,
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) {
        let mut tc_for_msg = tc.to_vec();
        for inv in invalid {
            tc_for_msg.push(ToolCall {
                id: inv.id.clone(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: inv.name.clone(),
                    arguments: inv.raw_arguments.clone(),
                },
            });
        }
        let assistant_msg = Message {
            role: crate::types::Role::Assistant,
            content: if text_content.is_empty() {
                None
            } else {
                Some(crate::types::MessageContent::Text(text_content.to_string()))
            },
            tool_calls: Some(tc_for_msg),
            tool_call_id: None,
            timestamp: Some(chrono::Local::now().to_rfc3339()),
            seq: None,
            // The reasoning that led to these calls rides on the message so
            // the next request of this tool loop can replay it to the model
            // that produced it (Anthropic signed blocks, DeepSeek-style
            // `reasoning_content`). Taken, not cloned: it belongs to this
            // stream only.
            reasoning_blocks: {
                let blocks = std::mem::take(&mut self.turn_reasoning_blocks);
                (!blocks.is_empty()).then_some(blocks)
            },
            reasoning_content: {
                let content = std::mem::take(&mut self.turn_reasoning_content);
                (!content.is_empty()).then_some(content)
            },
            reasoning_model: self.turn_reasoning_model.take(),
        };
        // The assistant message and its invalid-call results are adjacent
        // rows: flush them as one batch (one transaction) instead of N+1.
        let mut head_batch = vec![assistant_msg];
        for inv in invalid {
            let body = format_invalid_tool_result(inv);
            // Hooks intentionally NOT fired here: the tool never dispatched,
            // so emitting AfterToolCall without a matching BeforeToolCall
            // would skew hook accounting and surprise authors counting
            // Before↔After pairs.
            let _ = event_tx
                .send(AgentEvent::ToolExecuting {
                    tool_call_id: inv.id.clone(),
                    name: inv.name.clone(),
                    args: inv.raw_arguments.clone(),
                })
                .await;
            let _ = event_tx
                .send(AgentEvent::ToolResult {
                    tool_call_id: inv.id.clone(),
                    name: inv.name.clone(),
                    result: body.clone(),
                    images: Vec::new(),
                })
                .await;
            head_batch.push(Message::tool_result(&inv.id, &body));
        }
        self.log_and_persist_batch(head_batch).await;

        // Plan concurrent tool groups. The planner guarantees every group
        // either (a) contains a single Unknown-effect tool or (b) contains
        // only parallel-safe tools with no conflicting file-path writes.
        let concurrent_cfg = &self.config.conversation.concurrent_tools;
        let max_workers = concurrent_cfg.max_workers.max(1);
        let parallel_enabled = concurrent_cfg.enabled && max_workers > 1;

        // Resolve relative paths against the same cwd `dispatch_parallel_safe_tool`
        // hands the executor below, so the planner reasons about the files the
        // tools will actually write.
        let groups = crate::tool::effects::plan_groups(tc, self.execution_context.workspace_cwd());
        for group in groups {
            if cancel.is_cancelled() {
                // Emit cancellation markers for the remaining calls so the LLM
                // sees a tool_result for every tool_call it issued.
                for idx in group {
                    self.skip_tool_call(&tc[idx].id, crate::constants::TOOL_CANCELLED_BY_USER)
                        .await;
                }
                continue;
            }

            if group.len() == 1 || !parallel_enabled {
                let slice: Vec<&ToolCall> = group.iter().map(|i| &tc[*i]).collect();
                self.run_tool_calls(&slice, event_tx, cancel).await;
            } else {
                let slice: Vec<&ToolCall> = group.iter().map(|i| &tc[*i]).collect();
                self.run_tool_group_parallel(&slice, max_workers, event_tx, cancel)
                    .await;
            }
        }
    }

    #[instrument(skip_all)]
    async fn run_tool_calls(
        &mut self,
        tool_calls: &[&ToolCall],
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) {
        for tool_call in tool_calls {
            if cancel.is_cancelled() {
                self.skip_tool_call(&tool_call.id, crate::constants::TOOL_CANCELLED_BY_USER)
                    .await;
                continue;
            }

            let name = &tool_call.function.name;
            let args = tool_call.function.arguments.as_str();

            // Session availability is the first gate: a tool the model was
            // never shown (operator deny list, sub-agent `toolsets`) must not
            // run because the model named it anyway. No hook fires for it —
            // like an invalid call, it never dispatches.
            if !self.tool_available_in_session(name) {
                warn!(tool.name = %name, "tool call refused: not available in this session");
                self.skip_tool_call(
                    &tool_call.id,
                    &crate::tool::policy::unavailable_tool_message(name),
                )
                .await;
                continue;
            }

            let hook_ctx = self.hook_ctx(
                HookPoint::BeforeToolCall,
                HookData::ToolCall {
                    name: name.clone(),
                    args: args.to_string(),
                },
            );
            // A hook can rewrite the arguments. The rewrite is applied here,
            // before every gate below, so plan mode, the loop guard, and the
            // rate limiter all judge the call that actually runs.
            let mut rewritten_args: Option<String> = None;
            match self.hook_registry.dispatch(&hook_ctx).await {
                HookAction::Skip { reason } => {
                    // Surface the hook's explanation (its stderr) to the model —
                    // through the same redaction/truncation as any tool output,
                    // since hook scripts can echo anything.
                    let msg = match reason {
                        Some(r) => self.truncate_and_redact(&r),
                        None => crate::constants::TOOL_SKIPPED_BY_HOOK.to_string(),
                    };
                    self.skip_tool_call(&tool_call.id, &msg).await;
                    continue;
                }
                HookAction::Modify { args: new_args } => {
                    tracing::debug!("PreToolUse hook rewrote arguments for '{name}'");
                    rewritten_args = Some(new_args);
                }
                HookAction::Continue | HookAction::InjectContext(_) => {}
            }
            let args: &str = rewritten_args.as_deref().unwrap_or(args);

            // Plan mode: block mutating tool calls
            if self
                .config
                .conversation
                .collaboration_mode
                .blocks_mutations()
                && is_mutating_tool_call(name, args)
            {
                self.skip_tool_call(&tool_call.id, crate::constants::PLAN_MODE_MUTATION_DENIED)
                    .await;
                continue;
            }

            // Identical-tool-call loop guard: must run BEFORE rate-guard so a
            // blocked repeat does not consume rate-guard budget. Backoff
            // delays the would-be-blocked call without surrendering the
            // execution slot, buying an operator-interrupt window before the
            // hard block fires.
            match self.tool_call_repeat.observe(name, args) {
                RepeatDecision::Allow => {}
                RepeatDecision::Backoff(delay) => {
                    let count = self.tool_call_repeat.consecutive_count;
                    warn!(
                        "Repeated identical tool call backoff: '{name}' x{count}, sleeping {}ms",
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                }
                RepeatDecision::Block => {
                    let count = self.tool_call_repeat.consecutive_count;
                    warn!("Repeated identical tool call blocked: '{name}' x{count}");
                    self.metrics.tool_repeats_blocked.add(1, &[]);
                    self.skip_tool_call(
                        &tool_call.id,
                        &repeated_tool_call_block_message(name, count),
                    )
                    .await;
                    continue;
                }
            }

            // Per-turn expensive-tool budget: distinct-argument web searches /
            // sub-agent spawns evade the identical-call guard above, so a
            // runaway loop is bounded here by total volume instead.
            if let Some(cap) = self.expensive_tool_budget.observe(name) {
                warn!("Per-turn budget exhausted for '{name}' (cap {cap}); skipping call");
                self.skip_tool_call(&tool_call.id, &expensive_budget_block_message(name, cap))
                    .await;
                continue;
            }

            // Rate limits are a security boundary (not an approval prompt), so
            // they key off the action class the tool performs rather than its
            // name. `classify_action` is an explicit tool-name match with a
            // generic `ToolCall` fallback, so a new mutating tool lands in the
            // loosest bucket until it is added there.
            let action_type = classify_action(name);
            match self.rate_guard.record(action_type) {
                RateDecision::Block(reason) => {
                    warn!("Rate limit blocked tool call '{name}': {reason}");
                    self.skip_tool_call(&tool_call.id, &format!("Error: {reason}"))
                        .await;
                    continue;
                }
                RateDecision::Warn(reason) => {
                    warn!("{reason}");
                }
                RateDecision::Allow => {}
            }

            let _ = event_tx
                .send(AgentEvent::ToolExecuting {
                    tool_call_id: tool_call.id.clone(),
                    name: name.clone(),
                    args: args.to_string(),
                })
                .await;

            let tool_start = Instant::now();
            let tool_output = run_with_deadline(
                name,
                builtin_tool_deadline(name, self.config.tools.builtin_deadline_ms),
                self.execute_tool(name, args, event_tx, &tool_call.id, cancel)
                    .instrument(info_span!("tool.execute", tool.name = %name)),
            )
            .await
            .unwrap_or_else(|e| {
                ToolOutput::Text(crate::security::sanitize::sanitize_tool_error(
                    &e.to_string(),
                ))
            });
            let tool_elapsed = tool_start.elapsed().as_secs_f64();
            self.metrics.tool_executions.add(1, &[]);
            self.metrics.tool_duration.record(tool_elapsed, &[]);

            let msg = self
                .finalize_tool_output(&tool_call.id, name, tool_output, event_tx)
                .await;
            self.log_and_persist(msg).await;
        }
    }

    /// Post-process a completed tool call: redact + sanitize the output, wrap
    /// it in a `<tool_output>` element, fire the after-tool hook, and emit the
    /// `ToolResult` event. Returns the tool-result message for the caller to
    /// persist — the sequential path persists per call, the parallel merge
    /// collects its group into one batched flush. Shared verbatim by both
    /// paths so the two cannot drift.
    async fn finalize_tool_output(
        &mut self,
        tool_call_id: &str,
        name: &str,
        tool_output: ToolOutput,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) -> Message {
        // Sanitize tool name for XML embedding to prevent injection
        let safe_name = crate::security::sanitize::escape_xml_attr(name);
        let (raw_text, extra_parts) = match tool_output {
            ToolOutput::Text(t) => (t, None),
            ToolOutput::Multimodal { text, parts } => (text, Some(parts)),
        };
        let redacted = self.truncate_and_redact(&raw_text);
        let sanitized = crate::security::sanitize::sanitize_xml_boundaries(&redacted);
        let xml = format!(
            "<tool_output name=\"{safe_name}\" trust=\"external\">\n{sanitized}\n</tool_output>"
        );
        Self::fire_after_tool_hook(
            &mut self.hook_registry,
            &self.session.meta.id,
            self.turn_count,
            name,
            &xml,
        )
        .await;
        let images = match &extra_parts {
            Some(parts) => preview_images(parts).await,
            None => Vec::new(),
        };
        let _ = event_tx
            .send(AgentEvent::ToolResult {
                tool_call_id: tool_call_id.to_string(),
                name: name.to_string(),
                result: redacted,
                images,
            })
            .await;
        if let Some(parts) = extra_parts {
            let mut msg_parts = vec![ContentPart::Text(xml)];
            msg_parts.extend(
                parts
                    .into_iter()
                    .filter(|p| !matches!(p, ContentPart::Text(_))),
            );
            Message::tool_result_multimodal(tool_call_id, msg_parts)
        } else {
            Message::tool_result(tool_call_id, &xml)
        }
    }

    /// Execute a batch of parallel-safe tool calls concurrently, preserving
    /// result order. Pre-spawn checks (hooks, plan-mode, rate-limit) run
    /// sequentially on the main task; execution fans out to a bounded
    /// `JoinSet`; post-work (hooks, persistence, events) runs sequentially
    /// in the group's original order.
    #[instrument(skip_all)]
    async fn run_tool_group_parallel(
        &mut self,
        tool_calls: &[&ToolCall],
        max_workers: usize,
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) {
        // Pre-spawn serial pass: dispatch hooks, check plan-mode, record
        // rate-limit budget. For each call, build a Slot describing whether
        // it will execute or be skipped. `ToolExecuting` events and
        // persistence are deferred to the post-fan-out merge so they fire
        // in original LLM-issued order regardless of interleaving.
        enum Slot {
            Approved { name: String, args: String },
            Skipped { reason: String },
        }
        let mut slots: Vec<Slot> = Vec::with_capacity(tool_calls.len());

        for tool_call in tool_calls {
            if cancel.is_cancelled() {
                slots.push(Slot::Skipped {
                    reason: crate::constants::TOOL_CANCELLED_BY_USER.into(),
                });
                continue;
            }

            let name = tool_call.function.name.clone();
            let mut args = tool_call.function.arguments.clone();

            // Same session-availability gate as the sequential path, so a
            // parallel batch cannot carry a denied tool past the policy.
            if !self.tool_available_in_session(&name) {
                warn!(tool.name = %name, "tool call refused: not available in this session");
                slots.push(Slot::Skipped {
                    reason: crate::tool::policy::unavailable_tool_message(&name),
                });
                continue;
            }

            let hook_ctx = self.hook_ctx(
                HookPoint::BeforeToolCall,
                HookData::ToolCall {
                    name: name.clone(),
                    args: args.clone(),
                },
            );
            // Same hook contract as the sequential path: a block skips the
            // call, a rewrite replaces its arguments before any later gate
            // sees them.
            match self.hook_registry.dispatch(&hook_ctx).await {
                HookAction::Skip { reason } => {
                    // Same reason-surfacing (and redaction) as the sequential path.
                    slots.push(Slot::Skipped {
                        reason: match reason {
                            Some(r) => self.truncate_and_redact(&r),
                            None => crate::constants::TOOL_SKIPPED_BY_HOOK.into(),
                        },
                    });
                    continue;
                }
                HookAction::Modify { args: new_args } => {
                    tracing::debug!("PreToolUse hook rewrote arguments for '{name}' (parallel)");
                    args = new_args;
                }
                HookAction::Continue | HookAction::InjectContext(_) => {}
            }

            if self
                .config
                .conversation
                .collaboration_mode
                .blocks_mutations()
                && is_mutating_tool_call(&name, &args)
            {
                slots.push(Slot::Skipped {
                    reason: crate::constants::PLAN_MODE_MUTATION_DENIED.into(),
                });
                continue;
            }

            // Identical-tool-call loop guard — must precede rate-guard so a
            // blocked repeat does not consume rate-guard budget. The sleep
            // serializes the planning loop on a backoff hit; that's
            // intentional — the whole point of the guard is to slow a
            // runaway batch down before the hard cap fires.
            match self.tool_call_repeat.observe(&name, &args) {
                RepeatDecision::Allow => {}
                RepeatDecision::Backoff(delay) => {
                    let count = self.tool_call_repeat.consecutive_count;
                    warn!(
                        "Repeated identical tool call backoff (parallel): '{name}' x{count}, sleeping {}ms",
                        delay.as_millis()
                    );
                    tokio::time::sleep(delay).await;
                }
                RepeatDecision::Block => {
                    let count = self.tool_call_repeat.consecutive_count;
                    warn!("Repeated identical tool call blocked: '{name}' x{count}");
                    self.metrics.tool_repeats_blocked.add(1, &[]);
                    slots.push(Slot::Skipped {
                        reason: repeated_tool_call_block_message(&name, count),
                    });
                    continue;
                }
            }

            // Per-turn expensive-tool budget — same boundary as the
            // sequential path so a parallel batch can't sidestep it.
            if let Some(cap) = self.expensive_tool_budget.observe(&name) {
                warn!("Per-turn budget exhausted for '{name}' (cap {cap}); skipping call");
                slots.push(Slot::Skipped {
                    reason: expensive_budget_block_message(&name, cap),
                });
                continue;
            }

            let action_type = classify_action(&name);
            match self.rate_guard.record(action_type) {
                RateDecision::Block(reason) => {
                    warn!("Rate limit blocked tool call '{name}': {reason}");
                    slots.push(Slot::Skipped {
                        reason: format!("Error: {reason}"),
                    });
                    continue;
                }
                RateDecision::Warn(reason) => warn!("{reason}"),
                RateDecision::Allow => {}
            }

            slots.push(Slot::Approved { name, args });
        }

        // Fan-out: spawn approved slots onto JoinSet. Bounded by `max_workers`.
        let mut join_set: tokio::task::JoinSet<(usize, String, Instant, Result<ToolOutput>)> =
            tokio::task::JoinSet::new();
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(max_workers));

        for (idx, slot) in slots.iter().enumerate() {
            if let Slot::Approved { name, args } = slot {
                // Announce the call BEFORE the work runs, in the LLM's
                // original order (this loop is ordered). The stall watchdog
                // counts a tool as in-flight from `ToolExecuting` to
                // `ToolResult`; announcing only in the post-join merge pass
                // reported zero tools in flight for the whole parallel batch,
                // so a child running long parallel fetches was cancelled as
                // stalled while doing exactly what it was asked.
                let _ = event_tx
                    .send(AgentEvent::ToolExecuting {
                        tool_call_id: tool_calls[idx].id.clone(),
                        name: name.clone(),
                        args: args.clone(),
                    })
                    .await;
                let config = std::sync::Arc::clone(&self.config_arc);
                let execution_context = self.execution_context.clone();
                let tx = event_tx.clone();
                let permits = std::sync::Arc::clone(&permits);
                let name_clone = name.clone();
                let args_clone = args.clone();
                join_set.spawn(async move {
                    let _permit = permits.acquire_owned().await.ok();
                    let start = Instant::now();
                    let result = run_with_deadline(
                        &name_clone,
                        builtin_tool_deadline(&name_clone, config.tools.builtin_deadline_ms),
                        dispatch_parallel_safe_tool(
                            &name_clone,
                            &args_clone,
                            config.as_ref(),
                            &tx,
                            Some(&execution_context),
                        ),
                    )
                    .await;
                    (idx, name_clone, start, result)
                });
            }
        }

        // Collect completed tasks into a vec indexed by `idx` so we can
        // re-interleave with skip slots in original order.
        let mut results: Vec<Option<(String, f64, Result<ToolOutput>)>> =
            (0..slots.len()).map(|_| None).collect::<Vec<_>>();
        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok((idx, name, start, result)) => {
                    let elapsed = start.elapsed().as_secs_f64();
                    results[idx] = Some((name, elapsed, result));
                }
                Err(e) => {
                    tracing::error!("parallel tool worker panicked or was cancelled: {e}");
                }
            }
        }

        // Single in-order pass: fire hooks and build the result messages in
        // the LLM's original call order (`ToolExecuting` was already emitted
        // at spawn time, above). Skip slots and completed slots are
        // interleaved at their original positions. Every result already
        // exists in memory at this point, so persistence is deferred to one
        // batched flush after the loop — same rows, one transaction instead
        // of one per result.
        let mut result_batch: Vec<Message> = Vec::with_capacity(tool_calls.len());
        for (idx, slot) in slots.into_iter().enumerate() {
            let tool_call = tool_calls[idx];
            let (name, elapsed, result) = match slot {
                Slot::Skipped { reason } => {
                    result_batch.push(Message::tool_result(&tool_call.id, &reason));
                    continue;
                }
                Slot::Approved { name, .. } => match results[idx].take() {
                    Some((nm, el, res)) => (nm, el, res),
                    None => {
                        // Worker panicked — surface a placeholder error so the
                        // LLM still sees a tool_result for this call. Also
                        // close the `ToolExecuting` announced at spawn time,
                        // so event consumers (the stall watchdog's in-flight
                        // counter) do not count a dead worker as running.
                        let _ = event_tx
                            .send(AgentEvent::ToolResult {
                                tool_call_id: tool_call.id.clone(),
                                name: name.clone(),
                                result: "[tool call failed — worker panicked]".to_string(),
                                images: Vec::new(),
                            })
                            .await;
                        result_batch.push(Message::tool_result(
                            &tool_call.id,
                            "[tool call failed — worker panicked]",
                        ));
                        continue;
                    }
                },
            };

            let tool_output = result.unwrap_or_else(|e| {
                ToolOutput::Text(crate::security::sanitize::sanitize_tool_error(
                    &e.to_string(),
                ))
            });
            self.metrics.tool_executions.add(1, &[]);
            self.metrics.tool_duration.record(elapsed, &[]);

            let msg = self
                .finalize_tool_output(&tool_call.id, &name, tool_output, event_tx)
                .await;
            result_batch.push(msg);
        }
        self.log_and_persist_batch(result_batch).await;
    }

    #[instrument(skip_all, fields(tool.name = %name))]
    async fn execute_tool(
        &mut self,
        name: &str,
        args_json: &str,
        event_tx: &mpsc::Sender<AgentEvent>,
        tool_call_id: &str,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput> {
        let mut raw_args: serde_json::Value = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolOutput::Text(format!(
                    "Error: Invalid JSON arguments: {e}. Please provide valid JSON."
                )));
            }
        };
        // Fail closed on valid-JSON-but-not-an-object arguments before any
        // dispatch (including MCP and plugin paths) so a bare scalar/array
        // can't be silently coerced to an empty arg set and executed.
        if let Some(err) = reject_non_object_args(&mut raw_args) {
            return Ok(ToolOutput::Text(err));
        }
        if let Some(refusal) =
            crate::subscriptions::run_refusal(self.root_session_id(), name, &raw_args)
        {
            return Ok(ToolOutput::Text(refusal));
        }

        // MCP tools live outside the typed `Tool` enum and use a reserved
        // `mcp_` prefix. Short-circuit before `Tool::parse` so the LLM
        // cannot shadow an MCP tool by reusing its name through the
        // tagged deser, and so MCP failures surface with structured error
        // shape rather than masquerading as `UnknownTool`.
        if name.starts_with("mcp_") {
            // MCP calls have no builtin deadline and can run for the whole
            // `call_timeout`, so a cancelled turn must not wait them out.
            // Dropping the dispatch drops its reply receiver, which makes the
            // session actor abandon the call and tell the server. A reply
            // that is already here wins over the cancel.
            let result = tokio::select! {
                biased;
                result = crate::mcp_clients::try_execute_tool(name, &raw_args) => result?,
                () = cancel.cancelled() => {
                    // Cancellation is only a request to the server: say so,
                    // or the model may repeat an action that already ran.
                    return Ok(ToolOutput::Text(format!(
                        "{} The MCP server was told to stop, but it may already \
                         have acted — check before calling it again.",
                        crate::constants::TOOL_CANCELLED_BY_USER
                    )));
                }
            };
            return match result {
                // An image-bearing result goes through the same vision gate
                // as every other image-producing tool: a text-only model is
                // told the pixels were dropped instead of answering blind.
                Some(out) => Ok(crate::tool::vision::downgrade_for_non_vision_model(
                    out,
                    &self.config,
                    "the MCP tool returned an image",
                    "Ask the tool for a text description, or switch to a vision-capable model.",
                )),
                None => Err(anyhow::anyhow!("Unknown MCP tool: {name}")),
            };
        }

        // Single tagged-deser entry point. Schema-validation errors surface
        // here as structured tool output so the LLM can recover; the dispatch
        // arms below see only typed `Args`.
        let tool = match crate::tool::coerce::parse_with_coercion(name, raw_args.clone()) {
            Ok(t) => t,
            Err(crate::tool::ToolParseError::UnknownTool(n)) => {
                // WASM plugin execution does async DB + FS + wasmtime compile + run.
                // The wasmtime portion is CPU-bound; tolerate the in-band await
                // here for now — the manifest's timeout_ms budget already bounds it.
                // Plugin lookup runs BEFORE the unknown-tool counter so a
                // legitimate plugin name never trips the guard.
                let plugin_result = crate::wasm_plugins::try_execute_tool(&n, &raw_args).await?;
                if let Some(output) = plugin_result {
                    return Ok(ToolOutput::Text(output));
                }
                let count = self.unknown_tool_tracker.observe(&n);
                if self.unknown_tool_tracker.should_refuse(count) {
                    warn!("Unknown tool '{n}' refused after {count} attempts this turn");
                    self.metrics.unknown_tools_refused.add(1, &[]);
                    return Ok(ToolOutput::Text(unknown_tool_refusal_message(&n, count)));
                }
                return Err(anyhow::anyhow!("Unknown tool: {n}"));
            }
            Err(crate::tool::ToolParseError::InvalidArgs { tool, source }) => {
                return Ok(ToolOutput::Text(format!(
                    "Error: Invalid arguments for tool '{tool}': {source}"
                )));
            }
        };

        // Variants returning ToolOutput directly (may contain multimodal content).
        match &tool {
            crate::tool::Tool::ReadFile(args) => {
                let out = crate::tool::read_file::handle_with_context(
                    args,
                    &self.config,
                    &self.execution_context,
                )?;
                self.maybe_refresh_skills_after_tool(name, &raw_args, "");
                return Ok(out);
            }
            crate::tool::Tool::Browser(args) => {
                let out =
                    crate::tool::browser::handle(args, &self.config, &mut self.browser_session)
                        .await?;
                self.maybe_refresh_skills_after_tool(name, &raw_args, "");
                return Ok(out);
            }
            crate::tool::Tool::TextToSpeech(args) => {
                if let Some(ref synth) = self.tts_synthesizer {
                    return Ok(crate::tool::text_to_speech::handle(args, synth).await);
                }
                return Ok(ToolOutput::Text(
                    "TTS is not configured. Enable it via: borg settings set tts.enabled true"
                        .into(),
                ));
            }
            crate::tool::Tool::RequestUserInput(args) => {
                return crate::tool::request_user_input::handle(args, event_tx).await;
            }
            crate::tool::Tool::VisionAnalyze(args) => {
                let out = crate::tool::vision_analyze::handle(args, &self.config).await?;
                self.maybe_refresh_skills_after_tool(name, &raw_args, "");
                return Ok(out);
            }
            crate::tool::Tool::VideoAnalyze(args) => {
                let out = crate::tool::video_analyze::handle(args, &self.config).await?;
                self.maybe_refresh_skills_after_tool(name, &raw_args, "");
                return Ok(out);
            }
            crate::tool::Tool::ComputerUse(args) => {
                let out = crate::tool::computer_use::handle(args, &self.config, cancel).await?;
                self.maybe_refresh_skills_after_tool(name, &raw_args, "");
                return Ok(out);
            }
            _ => {}
        }

        // Variants returning Result<String> wrap to ToolOutput::Text below.
        let text_result: Result<String> = match &tool {
            crate::tool::Tool::WriteMemory(args) => {
                let out = crate::tool::write_memory::handle(
                    args,
                    &self.config,
                    &self.config_arc,
                    &mut self.caches,
                )
                .await;
                if let (Ok(_), Some(name)) = (&out, args.user_name()) {
                    self.adopt_user_name(name);
                }
                out
            }
            crate::tool::Tool::ReadMemory(args) => {
                crate::tool::read_memory::handle(args, &self.config).await
            }
            crate::tool::Tool::MemorySearch(args) => {
                crate::tool::memory_search::handle(args, &self.config).await
            }
            crate::tool::Tool::List(args) => {
                crate::tool::list::handle(
                    args,
                    &self.config,
                    self.agent_control.as_ref(),
                    Some(self.execution_context.workspace_root()),
                )
            }
            crate::tool::Tool::ListSkills => crate::tool::list::handle_list_skills(
                &self.config,
                Some(self.execution_context.workspace_root()),
            ),
            crate::tool::Tool::ListChannels => crate::tool::list::handle_list_channels(&self.config),
            crate::tool::Tool::SearchTools(args) => Ok(crate::tool::search_tools::handle(args)),
            crate::tool::Tool::ApplyPatch(args) => {
                crate::tool::apply_patch::handle_apply_patch_with_context(
                    args,
                    &self.config,
                    &self.execution_context,
                )
            }
            crate::tool::Tool::ApplySkillPatch(args) => {
                crate::tool::apply_patch::handle_apply_skill_patch(args, &self.config)
            }
            crate::tool::Tool::CreateChannel(args) => {
                crate::tool::apply_patch::handle_create_channel(args)
            }
            crate::tool::Tool::ListDir(args) => crate::tool::list_dir::handle_with_context(
                args,
                &self.config,
                &self.execution_context,
            ),
            crate::tool::Tool::RunShell(args) => {
                crate::tool::run_shell::handle(
                    args,
                    &self.config,
                    &self.policy,
                    event_tx,
                    tool_call_id,
                    Some(&self.skill_env_allowlist),
                    &self.execution_context,
                )
                .await
            }
            crate::tool::Tool::WebFetch(args) => {
                crate::tool::web_fetch::handle(args, &self.config).await
            }
            crate::tool::Tool::WebSearch(args) => {
                crate::tool::web_search::handle(args, &self.config).await
            }
            crate::tool::Tool::Schedule(args) => {
                crate::tool::schedule::handle_schedule(args, &self.config).await
            }
            crate::tool::Tool::ManageTasks(args) => {
                crate::tool::schedule::handle_manage_tasks(args, &self.config).await
            }
            crate::tool::Tool::ManageCron(args) => {
                crate::tool::schedule::handle_manage_cron(args, &self.config).await
            }
            crate::tool::Tool::Subscribe(args) => {
                // A sub-agent reports back to its parent; it does not set up
                // standing work of its own.
                if self.spawn_depth > 0
                    && !crate::tool_handlers::subscribe::is_read_action(&args.0)
                {
                    Ok("Error: a sub-agent can read subscriptions but not change them. Report what should change to the parent agent.".to_string())
                } else {
                    crate::tool_handlers::subscribe::handle_subscribe(&args.0, &self.config).await
                }
            }
            crate::tool::Tool::GenerateImage(args) => {
                crate::tool::generate_image::handle(args, &self.config).await
            }
            crate::tool::Tool::PhoneCall(args) => {
                crate::tool::phone_call::handle(args, &self.config).await
            }
            crate::tool::Tool::TranscribeAudio(args) => {
                crate::tool::transcribe_audio::handle(
                    args,
                    &self.config,
                    &self.execution_context,
                )
                .await
            }
            crate::tool::Tool::SpawnAgent(args) => {
                crate::tool::multi_agent::handle_spawn_agent(
                    args,
                    &mut self.agent_control,
                    &self.config,
                    &self.history,
                )
                .await
            }
            crate::tool::Tool::SendToAgent(args) => {
                crate::tool::multi_agent::handle_send_to_agent(args, &self.agent_control).await
            }
            crate::tool::Tool::WaitForAgent(args) => {
                crate::tool::multi_agent::handle_wait_for_agent(args, &mut self.agent_control).await
            }
            crate::tool::Tool::ListAgents(_) => {
                crate::tool::multi_agent::handle_list_agents(self.agent_control.as_ref())
            }
            crate::tool::Tool::CloseAgent(args) => {
                crate::tool::multi_agent::handle_close_agent(args, &mut self.agent_control).await
            }
            crate::tool::Tool::ManageRoles(args) => {
                crate::tool::multi_agent::handle_manage_roles(args).await
            }
            crate::tool::Tool::GetGoal(args) => {
                if !self.config.goals.enabled {
                    return Ok(ToolOutput::Text(GOALS_DISABLED_ERROR.to_string()));
                }
                let guard = self.db_guard().await;
                match guard.as_ref() {
                    Some(db) => {
                        crate::tool::get_goal::handle(args, db, &self.session.meta.id).await
                    }
                    None => Ok("null".to_string()),
                }
            }
            crate::tool::Tool::CreateGoal(args) => {
                if !self.config.goals.enabled {
                    return Ok(ToolOutput::Text(GOALS_DISABLED_ERROR.to_string()));
                }
                let handler_out = {
                    let guard = self.db_guard().await;
                    match guard.as_ref() {
                        Some(db) => {
                            crate::tool::create_goal::handle(
                                args,
                                db,
                                &self.session.meta.id,
                                self.config.goals.default_token_budget,
                                self.config.goals.max_token_budget,
                            )
                            .await
                        }
                        None => Ok(DATABASE_UNAVAILABLE_ERROR.to_string()),
                    }
                };
                if let Ok(ref text) = handler_out {
                    self.on_goal_created_via_tool(text).await;
                }
                handler_out
            }
            crate::tool::Tool::UpdateGoal(args) => {
                if !self.config.goals.enabled {
                    return Ok(ToolOutput::Text(GOALS_DISABLED_ERROR.to_string()));
                }
                let handler_out = {
                    let guard = self.db_guard().await;
                    match guard.as_ref() {
                        Some(db) => {
                            crate::tool::update_goal::handle(args, db, &self.session.meta.id)
                                .await
                        }
                        None => Ok(DATABASE_UNAVAILABLE_ERROR.to_string()),
                    }
                };
                if let Ok(ref text) = handler_out {
                    self.on_goal_updated_via_tool(text).await;
                }
                handler_out
            }
            // Variants handled in the early-return block above.
            crate::tool::Tool::ReadFile(_)
            | crate::tool::Tool::Browser(_)
            | crate::tool::Tool::TextToSpeech(_)
            | crate::tool::Tool::RequestUserInput(_)
            | crate::tool::Tool::VisionAnalyze(_)
            | crate::tool::Tool::VideoAnalyze(_)
            | crate::tool::Tool::ComputerUse(_) => unreachable!(
                "ReadFile/Browser/TextToSpeech/RequestUserInput/Vision/Video/ComputerUse are handled in the early-return match above"
            ),
        };
        let text = text_result?;
        self.maybe_refresh_skills_after_tool(name, &raw_args, &text);
        Ok(ToolOutput::Text(text))
    }
}

/// Execute a tool call that the planner classified as parallel-safe.
///
/// Parses through the typed [`crate::tool::Tool`] enum so the parallel-safety
/// gate is enforced by exhaustive `match` on `Tool::parallel_safe` — drift
/// between the planner and the dispatcher is impossible without a compile
/// error. Non-parallel-safe variants return a hard error so the planner bug
/// surfaces in tests instead of silent misbehaviour.
///
/// **No `ToolOutputDelta` from this path.** The surrounding loop emits
/// `ToolExecuting` / `ToolResult` events with the correct `tool_call_id`,
/// but the parallel dispatcher does not thread that id into the inner
/// handlers. None of the parallel-safe tools today (`read_file`,
/// `read_memory`, `memory_search`, `list_skills`, `list_channels`,
/// `list_dir`, `web_fetch`, `web_search`, `apply_patch`) emit streaming
/// output deltas, so the gap is invisible. Adding a tool here that wants
/// `ToolOutputDelta` requires extending this signature with `tool_call_id`
/// first, otherwise the delta will land at the OpenAI compat layer with an
/// empty id (see `agent_dispatcher::translate_events`).
/// Fail closed on tool arguments that parsed as valid JSON but are not a JSON
/// object. A tool invocation's `arguments` must be an object (`{...}`); a bare
/// string, number, boolean, or array is a malformed call and must not be
/// coerced into an empty arg set and executed — several tools accept a raw
/// `serde_json::Value` (`#[serde(transparent)]`) and would otherwise silently
/// default every field and walk into dispatch. `null` is the conventional
/// "no arguments" form, so it is normalized to an empty object and accepted,
/// keeping no-argument tools (`list_skills`, `list_channels`) working. A string
/// that is *itself* a JSON object is the double-encoded-arguments failure mode
/// (common from open-weight models) that the coercion layer recovers downstream;
/// since this guard runs *before* coercion, it unwraps that case in place rather
/// than rejecting it. Returns `Some(error_text)` to hand back to the model when
/// the value must be rejected.
fn reject_non_object_args(raw_args: &mut serde_json::Value) -> Option<String> {
    let kind = match raw_args {
        serde_json::Value::Object(_) => return None,
        serde_json::Value::Null => {
            *raw_args = serde_json::Value::Object(serde_json::Map::new());
            return None;
        }
        serde_json::Value::String(s) => {
            // Whole-arguments object double-encoded as a JSON string, e.g.
            // `args_json = "\"{\\\"path\\\":\\\"/etc/hosts\\\"}\""`. The coerce
            // layer (`tool::coerce::coerce_in_place`) would normally re-parse
            // this, but it runs after this guard — so unwrap an encoded object
            // here and accept it. A bare string that is not an encoded object
            // is a genuinely malformed call and is rejected below.
            match serde_json::from_str::<serde_json::Value>(s) {
                Ok(obj @ serde_json::Value::Object(_)) => {
                    *raw_args = obj;
                    return None;
                }
                _ => "string",
            }
        }
        serde_json::Value::Number(_) => "number",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Array(_) => "array",
    };
    Some(format!(
        "Error: Tool arguments must be a JSON object; got a {kind}. The tool was not executed."
    ))
}

pub(super) async fn dispatch_parallel_safe_tool(
    name: &str,
    args_json: &str,
    config: &Config,
    _event_tx: &mpsc::Sender<AgentEvent>,
    execution_context: Option<&crate::execution_context::ExecutionContext>,
) -> Result<ToolOutput> {
    // Reject unknown / non-parallel-safe tools by *name* before parsing args.
    // Surface as a loud Err so a planner/dispatcher mismatch fails tests rather
    // than masquerading as an arg-validation error.
    if !crate::tool::is_parallel_safe_name(name) {
        return Err(anyhow::anyhow!(
            "dispatch_parallel_safe_tool called with non-parallel-safe tool '{name}' — planner/dispatch mismatch"
        ));
    }
    let mut raw_args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(e) => {
            return Ok(ToolOutput::Text(format!(
                "Error: Invalid JSON arguments: {e}. Please provide valid JSON."
            )));
        }
    };
    // Fail closed on valid-JSON-but-not-an-object arguments (see
    // `reject_non_object_args`) before coercion/dispatch.
    if let Some(err) = reject_non_object_args(&mut raw_args) {
        return Ok(ToolOutput::Text(err));
    }
    let tool = match crate::tool::coerce::parse_with_coercion(name, raw_args) {
        Ok(t) => t,
        Err(crate::tool::ToolParseError::UnknownTool(n)) => {
            return Err(anyhow::anyhow!(
                "dispatch_parallel_safe_tool called with unknown tool '{n}' — planner/dispatch mismatch"
            ));
        }
        Err(crate::tool::ToolParseError::InvalidArgs { tool, source }) => {
            return Ok(ToolOutput::Text(format!(
                "Error: Invalid arguments for tool '{tool}': {source}"
            )));
        }
    };

    debug_assert!(tool.parallel_safe(), "is_parallel_safe_name drift");

    match &tool {
        crate::tool::Tool::ReadFile(args) => match execution_context {
            Some(ctx) => crate::tool::read_file::handle_with_context(args, config, ctx),
            None => crate::tool::read_file::handle(args, config),
        },
        crate::tool::Tool::ReadMemory(args) => crate::tool::read_memory::handle(args, config)
            .await
            .map(ToolOutput::Text),
        crate::tool::Tool::MemorySearch(args) => crate::tool::memory_search::handle(args, config)
            .await
            .map(ToolOutput::Text),
        crate::tool::Tool::ListSkills => {
            // Same workspace resolution as the sequential path: the catalog
            // must not change depending on whether the model happened to
            // batch this call with other parallel-safe tools.
            crate::tool::list::handle_list_skills(
                config,
                execution_context.map(crate::execution_context::ExecutionContext::workspace_root),
            )
            .map(ToolOutput::Text)
        }
        crate::tool::Tool::ListChannels => {
            crate::tool::list::handle_list_channels(config).map(ToolOutput::Text)
        }
        crate::tool::Tool::SearchTools(args) => {
            Ok(ToolOutput::Text(crate::tool::search_tools::handle(args)))
        }
        crate::tool::Tool::ListDir(args) => match execution_context {
            Some(ctx) => crate::tool::list_dir::handle_with_context(args, config, ctx),
            None => crate::tool::list_dir::handle(args, config),
        }
        .map(ToolOutput::Text),
        crate::tool::Tool::WebFetch(args) => crate::tool::web_fetch::handle(args, config)
            .await
            .map(ToolOutput::Text),
        crate::tool::Tool::WebSearch(args) => crate::tool::web_search::handle(args, config)
            .await
            .map(ToolOutput::Text),
        crate::tool::Tool::ApplyPatch(args) => match execution_context {
            Some(ctx) => {
                crate::tool::apply_patch::handle_apply_patch_with_context(args, config, ctx)
            }
            None => crate::tool::apply_patch::handle_apply_patch(args, config),
        }
        .map(ToolOutput::Text),
        crate::tool::Tool::ApplySkillPatch(args) => {
            crate::tool::apply_patch::handle_apply_skill_patch(args, config).map(ToolOutput::Text)
        }
        // Anything still reaching here despite `parallel_safe()` returning true
        // is a closed-set inconsistency — fail loudly so tests catch it.
        other => Err(anyhow::anyhow!(
            "dispatch_parallel_safe_tool: variant '{}' is parallel_safe() but lacks a handler arm",
            other.name()
        )),
    }
}

/// Largest preview the transcript is sent per image. A full-resolution
/// screenshot is several hundred kilobytes of base64; the event ring keeps a
/// thousand events per session, so previews stay an order of magnitude smaller.
const TOOL_RESULT_PREVIEW_MAX_BYTES: usize = 96 * 1024;

/// How many images one tool result may preview. A capture plus its
/// `capture_after` follow-up is two; nothing legitimate needs more.
const TOOL_RESULT_PREVIEW_MAX_IMAGES: usize = 4;

/// Image types the transcript will render. The bytes come from an external
/// driver or MCP server; anything else (an SVG, a declared-but-wrong type)
/// is left out rather than handed to an `<img>` as a data URI.
const TOOL_RESULT_PREVIEW_MIMES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Preview copies of the images in a tool result, for the operator's
/// transcript. Compression is CPU-bound, so it runs off the runtime; an
/// image that will not decode is skipped rather than failing the tool.
async fn preview_images(parts: &[ContentPart]) -> Vec<crate::agent::ToolResultImage> {
    let sources: Vec<(String, String)> = parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::ImageBase64 { media }
                if TOOL_RESULT_PREVIEW_MIMES.contains(&media.mime_type.as_str()) =>
            {
                Some((media.mime_type.clone(), media.data.clone()))
            }
            _ => None,
        })
        .take(TOOL_RESULT_PREVIEW_MAX_IMAGES)
        .collect();
    if sources.is_empty() {
        return Vec::new();
    }
    let compressed = tokio::task::spawn_blocking(move || {
        sources
            .into_iter()
            .filter_map(|(mime, data)| {
                match crate::media::core::compress_image(
                    &data,
                    &mime,
                    TOOL_RESULT_PREVIEW_MAX_BYTES,
                ) {
                    Ok((data, mime_type)) => {
                        Some(crate::agent::ToolResultImage { mime_type, data })
                    }
                    Err(e) => {
                        tracing::debug!(
                            "tool result image skipped from the transcript preview: {e}"
                        );
                        None
                    }
                }
            })
            .collect::<Vec<_>>()
    })
    .await;
    match compressed {
        Ok(images) => images,
        Err(e) => {
            tracing::warn!("tool result preview task failed: {e}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use crate::types::MediaData;

    fn part(mime: &str, png: &[u8]) -> ContentPart {
        use base64::Engine as _;
        ContentPart::ImageBase64 {
            media: MediaData {
                mime_type: mime.into(),
                data: base64::engine::general_purpose::STANDARD.encode(png),
                filename: None,
            },
        }
    }

    fn png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(16, 16, image::Rgba([10, 20, 30, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[tokio::test]
    async fn previews_keep_raster_images_and_drop_the_rest() {
        let parts = vec![
            ContentPart::Text("summary".into()),
            part("image/png", &png()),
            part("image/svg+xml", b"<svg onload=alert(1)/>"),
            part("image/png", b"not a png"),
        ];
        let images = preview_images(&parts).await;
        // The small PNG fits the cap and passes through as PNG; the SVG is
        // refused by type; the broken PNG is skipped by the compressor's
        // decode when over the cap, or passed through when under it — here
        // it is tiny, so it survives as bytes the browser will reject.
        assert!(images.iter().all(|i| i.mime_type == "image/png"));
        assert!(images.len() <= 2 && !images.is_empty());
        assert!(!images.iter().any(|i| i.mime_type.contains("svg")));
    }

    #[tokio::test]
    async fn previews_are_capped_in_count() {
        let parts: Vec<ContentPart> = (0..6).map(|_| part("image/png", &png())).collect();
        assert_eq!(
            preview_images(&parts).await.len(),
            TOOL_RESULT_PREVIEW_MAX_IMAGES
        );
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    /// A handler that blows past an injected short deadline must yield the
    /// synthetic addressable timeout result (Ok, not Err — the turn continues
    /// and the model sees a tool_result it can react to). The deadline is
    /// injected (30ms) so the test doesn't sleep out the production 300s.
    #[tokio::test]
    async fn slow_handler_yields_timeout_error_result() {
        let slow = async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(ToolOutput::Text("finished".into()))
        };
        let out = run_with_deadline("web_fetch", Some(Duration::from_millis(30)), slow)
            .await
            .expect("timeout must synthesize Ok result, not Err");
        match out {
            ToolOutput::Text(t) => {
                assert!(
                    t.starts_with("Error: tool 'web_fetch' timed out after 30ms"),
                    "unexpected timeout text: {t}"
                );
                assert!(
                    t.contains("abandoned"),
                    "timeout text must state the operation was abandoned: {t}"
                );
            }
            other => panic!("expected Text output, got {other:?}"),
        }
    }

    /// A handler finishing inside the deadline passes its output through
    /// untouched — the wrapper must be invisible on the happy path.
    #[tokio::test]
    async fn fast_handler_passes_through() {
        let fast = async { Ok(ToolOutput::Text("payload".into())) };
        let out = run_with_deadline("read_file", Some(Duration::from_millis(30)), fast)
            .await
            .unwrap();
        assert!(matches!(out, ToolOutput::Text(t) if t == "payload"));
    }

    /// Handler errors are not swallowed or reshaped by the deadline wrapper.
    #[tokio::test]
    async fn handler_error_propagates_through_deadline() {
        let failing = async { Err(anyhow::anyhow!("disk on fire")) };
        let err = run_with_deadline("read_file", Some(Duration::from_millis(30)), failing)
            .await
            .expect_err("handler Err must propagate");
        assert!(err.to_string().contains("disk on fire"));
    }

    /// A `None` deadline applies no timeout at all: the same sleep that
    /// trips a 30ms bound completes under `None`. Pairs with
    /// `deadline_exemptions_honored` to encode "exempt tools run unbounded".
    #[tokio::test]
    async fn none_deadline_applies_no_bound() {
        let mk = || async {
            tokio::time::sleep(Duration::from_millis(80)).await;
            Ok(ToolOutput::Text("subagent done".into()))
        };
        let bounded = run_with_deadline("wait_for_agent", Some(Duration::from_millis(30)), mk())
            .await
            .unwrap();
        assert!(
            matches!(&bounded, ToolOutput::Text(t) if t.contains("timed out")),
            "control: 30ms bound must trip on an 80ms handler"
        );
        let unbounded = run_with_deadline("wait_for_agent", None, mk())
            .await
            .unwrap();
        assert!(matches!(unbounded, ToolOutput::Text(t) if t == "subagent done"));
    }

    /// Exemption policy: self-bounding / legitimately-long tools and MCP
    /// tools run unbounded; everything else gets the 300s deadline.
    #[test]
    fn builtin_deadline_follows_the_configured_value() {
        // The deadline is a setting, not a constant: an operator running a
        // slow local model or a fast CI box must be able to move it, and `0`
        // must switch it off rather than mean "expire immediately".
        assert_eq!(
            builtin_tool_deadline("web_fetch", 45_000),
            Some(Duration::from_millis(45_000)),
            "a configured deadline must reach the bounded tools"
        );
        assert_eq!(
            builtin_tool_deadline("web_fetch", 0),
            None,
            "0 must disable the deadline, not expire instantly"
        );
        assert_eq!(
            builtin_tool_deadline("run_shell", 45_000),
            None,
            "an exempt tool stays exempt at any configured value"
        );
    }

    #[test]
    fn deadline_exemptions_honored() {
        const CONFIGURED: u64 = crate::constants::TOOLS_BUILTIN_DEADLINE_MS;
        for exempt in [
            "run_shell",
            "request_user_input",
            "spawn_agent",
            "wait_for_agent",
            "mcp_github_search",
        ] {
            assert_eq!(
                builtin_tool_deadline(exempt, CONFIGURED),
                None,
                "'{exempt}' must be exempt from the wall-clock deadline"
            );
        }
        for bounded in [
            "web_fetch",
            "web_search",
            "read_file",
            "browser",
            "computer_use",
        ] {
            assert_eq!(
                builtin_tool_deadline(bounded, CONFIGURED),
                Some(Duration::from_millis(CONFIGURED)),
                "'{bounded}' must be bounded by the wall-clock deadline"
            );
        }
    }
}

#[cfg(test)]
mod session_availability_tests {
    //! The policy that decides what the model is *shown* must also decide
    //! what *runs*. These drive the real pre-dispatch loops with a real
    //! `read_file` on a temp file, so "the handler was not invoked" is
    //! observable: the needle never reaches the tool result.

    use super::super::tests::minimal_test_agent;
    use super::*;
    use crate::types::Role;

    const NEEDLE: &str = "SECRET-NEEDLE-7f3a";

    fn needle_file() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("note.txt");
        std::fs::write(&path, NEEDLE).expect("write needle");
        (dir, path.to_string_lossy().into_owned())
    }

    fn read_file_call(id: &str, path: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: crate::tool::names::READ_FILE.to_string(),
                arguments: serde_json::json!({ "path": path }).to_string(),
            },
        }
    }

    /// Run `calls` through the sequential or the parallel loop and return
    /// the tool-result texts in call order plus whether any `ToolExecuting`
    /// event (the handler-dispatch announcement) was emitted.
    async fn run(agent: &mut Agent, calls: &[ToolCall], parallel: bool) -> (Vec<String>, bool) {
        let (tx, mut rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let refs: Vec<&ToolCall> = calls.iter().collect();
        if parallel {
            agent.run_tool_group_parallel(&refs, 4, &tx, &cancel).await;
        } else {
            agent.run_tool_calls(&refs, &tx, &cancel).await;
        }
        drop(tx);
        let mut executing = false;
        while let Some(ev) = rx.recv().await {
            if matches!(ev, AgentEvent::ToolExecuting { .. }) {
                executing = true;
            }
        }
        let results: Vec<String> = calls
            .iter()
            .map(|c| {
                agent
                    .history
                    .iter()
                    .find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(&c.id))
                    .and_then(|m| m.text_content())
                    .unwrap_or_else(|| panic!("no tool_result for call {}", c.id))
                    .to_string()
            })
            .collect();
        (results, executing)
    }

    fn assert_refused(result: &str) {
        assert!(
            result.contains("`read_file` is not available in this session"),
            "expected the unavailable-tool error, got: {result}"
        );
        assert!(
            !result.contains(NEEDLE),
            "handler ran: the file content leaked into the result: {result}"
        );
    }

    #[tokio::test]
    async fn a_sub_agent_of_a_subscription_run_cannot_schedule_work() {
        let mut agent = minimal_test_agent(Config::default());
        // What a child spawned by a subscription run inherits.
        agent.set_prompt_cache_scope("subscription:sub_abc".to_string());
        let call = ToolCall {
            id: "s1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: crate::tool::names::SCHEDULE.to_string(),
                arguments: serde_json::json!({
                    "action": "create",
                    "type": "prompt",
                    "name": "follow up",
                    "prompt": "check again",
                    "schedule": "every 1m",
                })
                .to_string(),
            },
        };
        let (results, _) = run(&mut agent, &[call], false).await;
        assert!(
            results[0].contains("cannot change anything inside a subscription run"),
            "got: {}",
            results[0]
        );
    }

    #[tokio::test]
    async fn deny_listed_tool_named_by_model_is_refused_and_never_runs() {
        let (_dir, path) = needle_file();
        let mut config = Config::default();
        config.tools.policy.deny = vec![crate::tool::names::READ_FILE.to_string()];
        for parallel in [false, true] {
            let mut agent = minimal_test_agent(config.clone());
            let calls = [read_file_call("c1", &path), read_file_call("c2", &path)];
            let (results, executing) = run(&mut agent, &calls, parallel).await;
            for r in &results {
                assert_refused(r);
            }
            assert!(
                !executing,
                "parallel={parallel}: a refused call must not announce dispatch"
            );
        }
    }

    #[tokio::test]
    async fn sub_agent_cannot_call_a_tool_outside_its_tools_filter() {
        let (_dir, path) = needle_file();
        let mut agent = minimal_test_agent(Config::default());
        agent.spawn_depth = 1;
        agent.tools_filter = Some(vec![crate::tool::names::WRITE_MEMORY.to_string()]);
        let calls = [read_file_call("c1", &path)];
        let (results, executing) = run(&mut agent, &calls, false).await;
        assert_refused(&results[0]);
        assert!(!executing);

        // The same child with the tool in its allowlist runs it: the gate is
        // the allowlist, not sub-agent status.
        let mut agent = minimal_test_agent(Config::default());
        agent.spawn_depth = 1;
        agent.tools_filter = Some(vec![crate::tool::names::READ_FILE.to_string()]);
        let (results, executing) = run(&mut agent, &calls, false).await;
        assert!(
            results[0].contains(NEEDLE),
            "allowlisted tool must run: {}",
            results[0]
        );
        assert!(executing);
    }

    /// `tools_filter` is a snapshot of the core definitions at spawn. With
    /// `mcp.tool_search_mode=Auto` an MCP tool is deferred — absent from
    /// that snapshot, discovered later through `search_tools` — so binding
    /// the filter to every name would strip every child of all MCP, WASM
    /// and late-connecting tools. Ungrouped names pass the filter; grouped
    /// core tools outside it are still refused, and deny still binds.
    #[test]
    fn tools_filter_binds_core_tools_but_not_deferred_registry_tools() {
        let mut agent = minimal_test_agent(Config::default());
        agent.spawn_depth = 1;
        agent.tools_filter = Some(vec![crate::tool::names::SEARCH_TOOLS.to_string()]);

        assert!(
            agent.tool_available_in_session("mcp_github_list_issues"),
            "a deferred MCP tool discovered via search_tools must stay callable"
        );
        assert!(
            agent.tool_available_in_session("my_wasm_plugin_tool"),
            "an ungrouped runtime/plugin tool must stay callable"
        );
        assert!(
            !agent.tool_available_in_session(crate::tool::names::RUN_SHELL),
            "a core tool outside the filter must still be refused"
        );
        assert!(
            agent.tool_available_in_session(crate::tool::names::SEARCH_TOOLS),
            "a core tool inside the filter runs"
        );

        // An explicit deny still binds a name the filter let through.
        agent
            .config
            .tools
            .policy
            .deny
            .push("mcp_github_list_issues".to_string());
        assert!(
            !agent.tool_available_in_session("mcp_github_list_issues"),
            "deny must still apply to ungrouped names"
        );
    }

    #[tokio::test]
    async fn subagent_deny_is_enforced_at_call_time_for_children_only() {
        let (_dir, path) = needle_file();
        let mut config = Config::default();
        config.tools.policy.subagent_deny = vec![crate::tool::names::READ_FILE.to_string()];
        let calls = [read_file_call("c1", &path)];

        let mut child = minimal_test_agent(config.clone());
        child.spawn_depth = 1;
        let (results, _) = run(&mut child, &calls, false).await;
        assert_refused(&results[0]);

        let mut root = minimal_test_agent(config);
        let (results, _) = run(&mut root, &calls, false).await;
        assert!(
            results[0].contains(NEEDLE),
            "subagent_deny must not bind the root agent: {}",
            results[0]
        );
    }

    #[tokio::test]
    async fn permitted_tool_still_runs_on_both_paths() {
        let (_dir, path) = needle_file();
        for parallel in [false, true] {
            let mut agent = minimal_test_agent(Config::default());
            let calls = [read_file_call("c1", &path)];
            let (results, executing) = run(&mut agent, &calls, parallel).await;
            assert!(
                results[0].contains(NEEDLE),
                "parallel={parallel}: permitted read_file must return the file: {}",
                results[0]
            );
            assert!(executing, "parallel={parallel}: dispatch must be announced");
        }
    }
}

#[cfg(test)]
mod mcp_cancel_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex as StdMutex};

    /// Answers the handshake and `tools/list` (one tool, `slow`), then never
    /// answers `tools/call`: a server still working when the turn ends.
    struct BusyServer {
        replies: VecDeque<mcp::protocol::Message>,
        sent: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl mcp::transport::Transport for BusyServer {
        async fn send(&mut self, msg: serde_json::Value) -> mcp::errors::Result<()> {
            let method = msg["method"].as_str().unwrap_or_default().to_string();
            let result = match method.as_str() {
                "initialize" => Some(serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "serverInfo": {"name": "busy", "version": "0"}
                })),
                "tools/list" => Some(serde_json::json!({"tools": [{
                    "name": "slow",
                    "inputSchema": {"type": "object", "properties": {}}
                }]})),
                _ => None,
            };
            if let Some(result) = result {
                self.replies.push_back(mcp::protocol::Message {
                    jsonrpc: Some("2.0".into()),
                    id: msg["id"].as_u64(),
                    method: None,
                    result: Some(result),
                    error: None,
                    params: None,
                });
            }
            self.sent.lock().unwrap().push(method);
            Ok(())
        }

        async fn recv(&mut self) -> mcp::errors::Result<Option<mcp::protocol::Message>> {
            match self.replies.pop_front() {
                Some(m) => Ok(Some(m)),
                None => std::future::pending().await,
            }
        }

        async fn close(&mut self) -> mcp::errors::Result<()> {
            Ok(())
        }
    }

    /// MCP calls are exempt from the builtin deadline, so the turn's cancel
    /// token is the only thing that ends one before `call_timeout`. The
    /// server must hear about it, and the model must learn that the call may
    /// still have run.
    #[test]
    fn cancelling_the_turn_ends_an_mcp_call_in_flight() {
        let _serial = crate::mcp_clients::testing::serial_guard();
        crate::mcp_clients::testing::reset_all();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let sent = Arc::new(StdMutex::new(Vec::new()));
        rt.block_on(async {
            let mut cfg = crate::config::Config::default();
            cfg.mcp.enabled = true;
            let server_cfg = crate::config::McpServerConfig {
                enabled: true,
                transport: crate::config::TransportSpec::Stdio {
                    command: "/bin/false".into(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                },
                connect_timeout_ms: 5_000,
                call_timeout_ms: 60_000,
                rate_limit_per_minute: 60,
                rate_limit_burst: 10,
                tools: mcp::manifest::ToolFilter::default(),
                cache_tools: false,
            };
            let server = BusyServer {
                replies: VecDeque::new(),
                sent: Arc::clone(&sent),
            };
            crate::mcp_clients::testing::install_session(
                "busy",
                &server_cfg,
                &cfg,
                Box::new(server),
            )
            .await
            .expect("install");

            let mut agent =
                super::super::tests::minimal_test_agent(crate::config::Config::default());
            let (event_tx, _event_rx) = mpsc::channel(8);
            let cancel = CancellationToken::new();
            let trigger = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                trigger.cancel();
            });

            let out = tokio::time::timeout(
                Duration::from_secs(5),
                agent.execute_tool("mcp_busy_slow", "{}", &event_tx, "call-1", &cancel),
            )
            .await
            .expect("the cancel must end the call, not call_timeout")
            .expect("a cancelled call is a tool result, not an error");
            let ToolOutput::Text(text) = out else {
                panic!("expected the cancel marker, got {out:?}");
            };
            assert!(
                text.starts_with(crate::constants::TOOL_CANCELLED_BY_USER)
                    && text.contains("may already have acted"),
                "got {text}"
            );

            tokio::time::timeout(Duration::from_secs(2), async {
                while !sent
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|m| m == "notifications/cancelled")
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the server must be told the call was abandoned");
        });
        crate::mcp_clients::testing::reset_all();
    }
}
