//! Sub-agent spawn and execution machinery. [`AgentControl::spawn_agent`]
//! assembles a [`SubAgentSpawn`] parameter object and hands it to
//! [`run_sub_agent_task`], which acquires the concurrency permit, builds the
//! child's preamble, drives the agent loop, and ships the terminal status (plus
//! any `apply_patch` paths the child mutated) back to the parent. Kept separate
//! from the `AgentControl` registry/control plane in the parent module.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{mpsc, Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::agent::AgentEvent;
use crate::config::Config;

use super::{
    AgentRole, AgentRoleKind, AgentTreeBus, AgentTreeEventKind, LiveActivity, SubAgentCompletion,
    SubAgentInfo, SubAgentStatus,
};

/// Heartbeat cycles of grace after the stall threshold trips before the
/// watchdog cancels the child (default interval 30s → 120s of grace). A child
/// that produces real progress inside the window recovers and finalizes
/// normally; one that stays frozen is cancelled with a structured stall
/// error so the parent hears a terminal outcome instead of waiting forever.
pub(super) const STALL_GRACE_CYCLES: u32 = 4;

/// Prefixed to every parent-supplied `<context>` block so the child reads the
/// handoff as a snapshot taken when it was spawned, not guaranteed-current
/// truth. The parent's view can drift while the child runs (files change, tasks
/// complete), and an LLM otherwise treats handed-down context as live fact
/// regardless of age — so it's told to re-verify anything time-sensitive.
const SUBAGENT_CONTEXT_FRESHNESS_NOTE: &str =
    "[Point-in-time snapshot from when you were spawned — re-verify anything \
     time-sensitive against the live source before acting on it.]";

/// Render the child's initial user-message preamble: identity + role +
/// delegation policy + workspace + optional context + goal.
///
/// The structured `<context trust="parent">…</context>` and
/// `<goal>…</goal>` blocks let the child parse intent unambiguously and
/// give callers (and downstream tests) stable anchors to grep against. The
/// context block opens with [`SUBAGENT_CONTEXT_FRESHNESS_NOTE`].
#[allow(clippy::too_many_arguments)]
pub fn build_subagent_preamble(
    nickname: &str,
    agent_id: &str,
    role: Option<&AgentRole>,
    role_kind: AgentRoleKind,
    goal: &str,
    context: Option<&str>,
    depth: u32,
    max_depth: u32,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "You are a sub-agent named \"{nickname}\" (id: {agent_id}).\n"
    ));

    let role_label = role.map_or("default", |r| r.name.as_str());
    out.push_str(&format!(
        "Role: {role_label} ({kind})",
        kind = role_kind.as_str()
    ));
    if let Some(r) = role {
        if !r.description.is_empty() {
            out.push_str(&format!(" — {desc}", desc = r.description));
        }
    }
    out.push('\n');

    out.push_str(&format!("Depth: {depth}/{max_depth}. "));
    match role_kind {
        AgentRoleKind::Orchestrator if depth + 1 < max_depth => {
            out.push_str("You may delegate further via spawn_agent.\n");
        }
        AgentRoleKind::Orchestrator => {
            out.push_str("Orchestrator role but depth cap reached — cannot delegate further.\n");
        }
        AgentRoleKind::Leaf => {
            out.push_str("You are a leaf — do not attempt to delegate.\n");
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        out.push_str(&format!("Workspace: {}\n", cwd.display()));
    }

    if let Some(r) = role {
        if let Some(instr) = r.system_instructions.as_deref() {
            if !instr.is_empty() {
                out.push_str(instr);
                out.push('\n');
            }
        }
    }

    out.push_str(
        "\nComplete the task and return a clear, concise final response. \
         Your full response is returned verbatim to the parent.\n\n",
    );

    if let Some(ctx) = context {
        if !ctx.is_empty() {
            // Frame sits OUTSIDE the trust boundary: it's our own trusted
            // directive, so keep it clear of the lower-trust parent content
            // (which could carry an injection payload).
            out.push_str(SUBAGENT_CONTEXT_FRESHNESS_NOTE);
            out.push('\n');
            out.push_str("<context trust=\"parent\">\n");
            // Neutralize any embedded `</context>` so parent-supplied content
            // can't break out of the trust frame and smuggle text in adjacent to
            // the goal as if it were our own directive. Scoped to the frame tags
            // (not the global boundary set) so we don't mangle legitimate
            // `</context>` text the parent passes through verbatim.
            let safe_ctx = crate::security::sanitize::escape_trust_frame_tags(ctx);
            out.push_str(&safe_ctx);
            if !safe_ctx.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("</context>\n\n");
        }
    }

    out.push_str("<goal>\n");
    // Same breakout guard for the goal block — a smuggled `</goal>` would let
    // following text escape the framing.
    let safe_goal = crate::security::sanitize::escape_trust_frame_tags(goal);
    out.push_str(&safe_goal);
    if !safe_goal.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("</goal>\n");

    out
}

/// True iff an `apply_patch` tool result string represents a successful
/// application. Allowlist of success prefixes — every failure path
/// (`Error applying…`, `Access denied…`, `Plan mode…`, `Unknown target…`,
/// `Rate limit…`, hook aborts) returns `Ok(format!(…))` rather than an
/// `Err`, and the set of failure prefixes is unbounded. Matching against
/// the known success prefix is the only safe way to gate `files_written`.
///
/// Success prefixes (see `format_patch_success` callers in
/// `tool_handlers/filesystem/patch.rs`):
///   - `"Patch applied successfully."`
///   - `"Unified patch applied successfully."`
///   - `"Skill patch applied successfully."`
///   - `"Channel patch applied successfully."` (future-proof — same shape)
pub(super) fn is_apply_patch_success(result: &str) -> bool {
    let trimmed = result.trim_start();
    // Tolerate any leading qualifier ("Patch", "Unified patch", "Skill
    // patch", etc.) followed by "applied successfully" — that's the
    // canonical success-prefix shape `format_patch_success` emits.
    trimmed.contains("applied successfully")
}

/// Extract file paths from `apply_patch` tool arguments. The patch DSL
/// uses `*** Add File:`, `*** Update File:`, `*** Delete File:` markers
/// preceded by an optional input path.
///
/// If the call uses `apply_patch_unified` with a non-`cwd` target
/// (`target: "skills"` or `target: "channels"`), the bare patch paths
/// are resolved relative to that target's data dir (e.g.
/// `~/.borg/skills/<name>/SKILL.md`) so the parent's LLM gets paths it
/// can actually re-read. Targets that can't be resolved (no HOME, weird
/// IO error) fall through to bare paths and emit a `tracing::warn!`.
pub(super) fn extract_apply_patch_paths(args_json: &str) -> Vec<String> {
    // Args are either {"patch": "...", "target": "skills"} (unified
    // apply_patch tool) or a raw patch string. Strip JSON wrapping if
    // present, then scan for markers and resolve relative to target.
    let (patch_text, base): (std::borrow::Cow<'_, str>, Option<std::path::PathBuf>) =
        match serde_json::from_str::<serde_json::Value>(args_json) {
            Ok(v) => {
                let patch = v
                    .get("patch")
                    .and_then(|p| p.as_str())
                    .map(|s| std::borrow::Cow::Owned(s.to_string()))
                    .unwrap_or(std::borrow::Cow::Borrowed(args_json));
                let target = v.get("target").and_then(|t| t.as_str()).unwrap_or("cwd");
                let base = match target {
                    "cwd" => None,
                    "skills" => match crate::config::Config::skills_dir() {
                        Ok(p) => Some(p),
                        Err(e) => {
                            tracing::warn!(
                                "extract_apply_patch_paths: failed to resolve skills_dir, \
                                 falling back to bare path: {e}"
                            );
                            None
                        }
                    },
                    "channels" => match crate::config::Config::channels_dir() {
                        Ok(p) => Some(p),
                        Err(e) => {
                            tracing::warn!(
                                "extract_apply_patch_paths: failed to resolve channels_dir, \
                                 falling back to bare path: {e}"
                            );
                            None
                        }
                    },
                    other => {
                        tracing::warn!(
                            target = other,
                            "extract_apply_patch_paths: unknown target — falling back to bare path"
                        );
                        None
                    }
                };
                (patch, base)
            }
            Err(_) => (std::borrow::Cow::Borrowed(args_json), None),
        };

    let mut paths = Vec::new();
    for line in patch_text.lines() {
        let trimmed = line.trim_start();
        for marker in ["*** Add File:", "*** Update File:", "*** Delete File:"] {
            if let Some(rest) = trimmed.strip_prefix(marker) {
                let raw = rest.trim();
                if raw.is_empty() {
                    continue;
                }
                let resolved = match &base {
                    Some(b) => b.join(raw).to_string_lossy().into_owned(),
                    None => raw.to_string(),
                };
                if !paths.contains(&resolved) {
                    paths.push(resolved);
                }
            }
        }
    }
    paths
}

/// Bundle of state captured by [`run_sub_agent_task`]. Lives only between
/// `spawn_agent`'s setup and the tokio task taking ownership — exists so the
/// spawn site can move 20 named fields into the task with a single `let _ =`
/// instead of an opaque `async move {}` closure that hides what gets cloned
/// vs. moved.
pub(super) struct SubAgentSpawn {
    pub(super) agent_id: String,
    pub(super) nickname: String,
    pub(super) config: Config,
    pub(super) depth: u32,
    pub(super) max_spawn_depth: u32,
    pub(super) agents_config: crate::config::MultiAgentConfig,
    pub(super) message: String,
    pub(super) context: Option<String>,
    pub(super) role: Option<AgentRole>,
    pub(super) role_kind: AgentRoleKind,
    pub(super) fork_context: Option<Vec<crate::types::Message>>,
    pub(super) input_rx: mpsc::UnboundedReceiver<String>,
    /// Shared ledger of parent-queued messages not yet observed as absorbed by
    /// the child. Popped on each `SteerReceived`; the remainder ships as
    /// `SubAgentCompletion::undelivered_input`.
    pub(super) pending_input: Arc<std::sync::Mutex<Vec<String>>>,
    pub(super) cancel: CancellationToken,
    pub(super) is_paused: Arc<AtomicBool>,
    pub(super) pause_notify: Arc<Notify>,
    pub(super) tools_filter: Option<Vec<String>>,
    pub(super) bus: Option<AgentTreeBus>,
    pub(super) info: SubAgentInfo,
    /// Prompt-cache scope inherited from the spawning controller — the root
    /// session of this agent tree, not necessarily the immediate parent.
    pub(super) prompt_cache_scope: String,
    /// Sandbox confinement inherited from the parent. `None` only when the
    /// parent itself never had a context installed (CLI/test paths).
    pub(super) execution_context: Option<crate::execution_context::ExecutionContext>,
    pub(super) semaphore: Arc<Semaphore>,
    pub(super) completion_tx: mpsc::Sender<SubAgentCompletion>,
    /// Live progress mirror shared with the parent's handle (last progress
    /// time + currently executing tool), read by `list_agents`.
    pub(super) live: Arc<LiveActivity>,
}

/// Driver for the tokio task `spawn_agent` creates. Acquires the concurrency
/// permit, runs the agent loop, then ships the terminal status (plus any
/// `apply_patch` file paths the child mutated) back to the parent via
/// `completion_tx`. Send failures are logged but never panic — the parent
/// may have shut down the receiver.
pub(super) async fn run_sub_agent_task(spawn: SubAgentSpawn) {
    let SubAgentSpawn {
        agent_id,
        nickname,
        config,
        depth,
        max_spawn_depth,
        agents_config,
        message,
        context,
        role,
        role_kind,
        fork_context,
        input_rx,
        pending_input,
        cancel,
        is_paused,
        pause_notify,
        tools_filter,
        bus,
        info,
        prompt_cache_scope,
        execution_context,
        semaphore,
        completion_tx,
        live,
    } = spawn;

    let _permit = match semaphore.acquire().await {
        Ok(p) => p,
        Err(_) => {
            if completion_tx
                .send(SubAgentCompletion {
                    agent_id,
                    nickname,
                    status: SubAgentStatus::Errored {
                        error: "Semaphore closed".to_string(),
                    },
                    final_response: None,
                    files_written: Vec::new(),
                    // Nothing could have been queued yet: the handle is only
                    // published to the parent after the task is spawned.
                    undelivered_input: Vec::new(),
                })
                .await
                .is_err()
            {
                tracing::debug!("sub-agent completion receiver dropped");
            }
            return;
        }
    };

    let files_written = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let result = run_sub_agent(
        agent_id.clone(),
        nickname.clone(),
        config,
        depth,
        max_spawn_depth,
        agents_config,
        message,
        context,
        role,
        role_kind,
        fork_context,
        input_rx,
        pending_input.clone(),
        cancel,
        is_paused,
        pause_notify,
        tools_filter,
        files_written.clone(),
        bus,
        info,
        prompt_cache_scope,
        execution_context,
        live,
    )
    .await;

    let files_written_vec = files_written.lock().map(|g| g.clone()).unwrap_or_default();

    // Whatever the child never absorbed. Drained (not cloned) so a parent that
    // inspects the handle afterwards doesn't see the same text reported twice.
    let undelivered_input = pending_input
        .lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default();
    if !undelivered_input.is_empty() {
        tracing::warn!(
            %agent_id,
            count = undelivered_input.len(),
            "sub-agent finished before absorbing queued input; reporting it as undelivered",
        );
    }

    let (status, final_response) = match result {
        Ok(response) => (
            SubAgentStatus::Completed {
                result: response.clone(),
            },
            Some(response),
        ),
        Err(e) => (
            SubAgentStatus::Errored {
                error: e.to_string(),
            },
            None,
        ),
    };

    match crate::db::Database::open().await {
        Ok(db) => {
            if let Err(e) = db.update_sub_agent_status(&agent_id, &status).await {
                tracing::warn!(%agent_id, "failed to update sub-agent status: {e}");
            }
        }
        Err(e) => tracing::warn!(
            %agent_id,
            "failed to open database; sub-agent final status not persisted: {e:#}"
        ),
    }

    if completion_tx
        .send(SubAgentCompletion {
            agent_id,
            nickname,
            status,
            final_response,
            files_written: files_written_vec,
            undelivered_input,
        })
        .await
        .is_err()
    {
        tracing::debug!("sub-agent completion receiver dropped");
    }
}

/// Clamp a child's history-token budget to `cap` (`0` = no cap).
///
/// The agent loop's compaction threshold is
/// `crate::conversation::effective_history_budget_for(config)` —
/// `conversation.max_history_tokens` bounded by the model's usable window —
/// so lowering `max_history_tokens` on the child's config is exactly the
/// lever. `min` keeps an already-smaller budget as is; the cap never raises.
/// Fold one event of the child into the text that the parent gets as the
/// result. A retraction removes the text of a failed LLM attempt: the full
/// response comes again, and the result is the text of the attempt that
/// completes.
fn fold_result_text(final_text: &mut String, event: &AgentEvent) {
    match event {
        AgentEvent::TextDelta(delta) => final_text.push_str(delta),
        AgentEvent::TextRetracted { bytes } => {
            crate::agent::retract_text_tail(final_text, *bytes);
        }
        _ => {}
    }
}

pub(super) fn clamp_child_history_budget(mut config: Config, cap: usize) -> Config {
    if cap > 0 {
        config.conversation.max_history_tokens = config.conversation.max_history_tokens.min(cap);
    }
    config
}

/// Run a sub-agent task. Returns the final text response.
/// Uses Box::pin to break the async recursion cycle (Agent -> AgentControl -> spawn -> Agent).
#[allow(clippy::too_many_arguments)]
fn run_sub_agent(
    agent_id: String,
    nickname: String,
    config: Config,
    depth: u32,
    max_spawn_depth: u32,
    agents_config: crate::config::MultiAgentConfig,
    initial_message: String,
    context: Option<String>,
    role: Option<AgentRole>,
    role_kind: AgentRoleKind,
    fork_context: Option<Vec<crate::types::Message>>,
    input_rx: mpsc::UnboundedReceiver<String>,
    pending_input: Arc<std::sync::Mutex<Vec<String>>>,
    cancel: CancellationToken,
    is_paused: Arc<AtomicBool>,
    pause_notify: Arc<Notify>,
    tools_filter: Option<Vec<String>>,
    files_written: Arc<std::sync::Mutex<Vec<String>>>,
    bus: Option<AgentTreeBus>,
    info: SubAgentInfo,
    prompt_cache_scope: String,
    execution_context: Option<crate::execution_context::ExecutionContext>,
    live: Arc<LiveActivity>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>> {
    Box::pin(async move {
        use crate::agent::Agent;

        // The child's compaction trigger derives from its own `Config`, so
        // clamping the history budget on this clone caps the child without
        // touching the parent's threshold.
        let config = clamp_child_history_budget(config, agents_config.compaction_threshold_tokens);
        let mut agent = Agent::new_sub_agent(
            config.clone(),
            depth,
            &agents_config,
            crate::telemetry::BorgMetrics::noop(),
            tools_filter,
        )
        .await?;

        // Inherit the parent's sandbox confinement. `Agent::new_sub_agent`
        // builds a process-derived (unrestricted) context, so without this the
        // child runs unsandboxed while holding `auto_approve: ["*"]` — a
        // one-hop escape from a workspace-confined parent.
        if let Some(execution_context) = execution_context {
            agent.set_execution_context(execution_context);
        }

        // Share the spawning tree's prompt-cache key: parent and child
        // requests carry a near-identical prompt prefix, and OpenAI-family
        // caches only hit when both key and prefix match. The scope comes
        // from the controller, so a whole tree keys on the root session
        // rather than re-scoping at every level.
        agent.set_prompt_cache_scope(prompt_cache_scope);

        // Hand the parent's input channel to the child's steering machinery.
        // Without this the receiver was polled in the event loop below and the
        // message thrown away, while `send_to_agent` told the parent's LLM the
        // message had been sent. The agent loop absorbs steers at tool
        // boundaries and at turn end, so queued guidance becomes a real user
        // message in the child's history.
        agent.set_steer_channel(input_rx);

        if let Some(context_msgs) = fork_context {
            for msg in context_msgs {
                agent.inject_history_message(msg);
            }
        }

        let full_message = build_subagent_preamble(
            &nickname,
            &agent_id,
            role.as_ref(),
            role_kind,
            &initial_message,
            context.as_deref(),
            depth,
            max_spawn_depth,
        );

        match crate::db::Database::open().await {
            Ok(db) => {
                if let Err(e) = db
                    .update_sub_agent_status(&agent_id, &SubAgentStatus::Running)
                    .await
                {
                    tracing::warn!(%agent_id, "failed to update sub-agent running status: {e}");
                }
            }
            Err(e) => tracing::warn!(
                %agent_id,
                "failed to open database; sub-agent running status not persisted: {e:#}"
            ),
        }

        let (event_tx, mut event_rx) =
            mpsc::channel::<AgentEvent>(crate::constants::AGENT_EVENT_CHANNEL_CAPACITY);
        let cancel_clone = cancel.clone();

        let mut send_handle = {
            let event_tx_clone = event_tx.clone();
            let cancel = cancel_clone.clone();
            tokio::spawn(async move {
                agent
                    .send_message_with_cancel(&full_message, event_tx_clone, cancel)
                    .await
            })
        };

        // Heartbeat: tick every `heartbeat_interval_secs` and publish a
        // Heartbeat event on the bus. If no real progress (TextDelta /
        // ToolResult) for `heartbeat_stale_cycles` consecutive ticks,
        // publish a Stalled event and start the grace window; if the child
        // still produces nothing for `STALL_GRACE_CYCLES` further ticks, the
        // watchdog cancels it and returns a structured stall error — the
        // parent hears a terminal outcome instead of waiting on a wedged
        // child forever. Real progress inside the grace window recovers.
        // `heartbeat_interval_secs == 0` is the explicit disable sentinel —
        // no ticks, no events, no watchdog.
        let heartbeat_enabled = agents_config.heartbeat_interval_secs > 0;
        let heartbeat_interval_secs = agents_config.heartbeat_interval_secs.max(1);
        let stale_threshold = super::stall::effective_stale_cycles(
            agents_config.heartbeat_stale_cycles.max(1),
            heartbeat_interval_secs,
            config.llm.request_timeout_ms,
        );
        let mut heartbeat_tick =
            tokio::time::interval(std::time::Duration::from_secs(heartbeat_interval_secs));
        // Skip the immediate first tick — we want to wait one full interval
        // before the first heartbeat fires, not race the spawn.
        heartbeat_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        heartbeat_tick.tick().await;
        let mut watchdog =
            super::stall::StallWatchdog::new(stale_threshold, agents_config.tool_stale_cycles);
        let mut stall_error: Option<String> = None;

        // Track apply_patch calls that have NOT yet had a ToolResult: the
        // patch paths are known at ToolExecuting time (from args), but
        // committing them to files_written before we see a successful
        // result would surface paths from hook-blocked, plan-mode-blocked,
        // or otherwise-failed patches that never touched disk.
        let mut pending_patch_paths: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();

        // Snapshot info with status flipped to Running for the spawn's
        // initial heartbeat payload. NOTE: subsequent transitions in
        // AgentControl (Paused / Errored / Shutdown) are NOT reflected
        // here — the spawn task has no shared reference to the handle's
        // live info. TUI subscribers needing exact live status should
        // re-query via `list_agents` / `get_status`; heartbeats only
        // guarantee "child is still ticking" semantics.
        let mut info = info;
        info.status = SubAgentStatus::Running;

        let mut final_text = String::new();
        loop {
            // Pause gate: park here while the operator has flipped the flag.
            // Cooperative — an in-flight LLM call already inside `send_message`
            // is not interrupted; this only stops *additional* iterations from
            // being processed. Wake on resume (notify_waiters) or shutdown
            // (cancel.cancel()) which also wakes the notify.
            //
            // We register on `notified` BEFORE re-checking `is_paused`. This
            // closes the lost-wakeup race where `set_paused(false)` +
            // `notify_waiters()` could fire after the load but before we
            // entered the select arm — `notify_waiters` only wakes already-
            // registered waiters, so a late registration would block forever.
            loop {
                let notified = pause_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !is_paused.load(Ordering::Acquire) {
                    break;
                }
                if cancel.is_cancelled() {
                    break;
                }
                tokio::select! {
                    _ = notified => {}
                    _ = cancel.cancelled() => break,
                }
            }
            tokio::select! {
                _ = cancel.cancelled() => {
                    break;
                }
                event = event_rx.recv() => {
                    // Which events count as forward progress — and, just as
                    // importantly, which deliberately do not — is documented
                    // on `StallWatchdog::on_event`.
                    if let Some(ref ev) = event {
                        if watchdog.on_event(ev) {
                            live.touch();
                        }
                    }
                    if let Some(ref ev) = event {
                        fold_result_text(&mut final_text, ev);
                    }
                    match event {
                        Some(AgentEvent::TurnComplete) | None => {
                            break;
                        }
                        Some(AgentEvent::ShellConfirmation { respond, .. }) => {
                            if respond.send(true).is_err() {
                                tracing::debug!("sub-agent shell confirmation receiver dropped");
                            }
                        }
                        Some(AgentEvent::ToolExecuting { tool_call_id, name, args }) => {
                            live.set_current_tool(Some(name.clone()));
                            // Stash the apply_patch paths keyed by call id;
                            // commit to files_written only when the matching
                            // ToolResult comes back successful. Buffered here
                            // (not committed) so failures don't leak.
                            if name == "apply_patch" {
                                let new_paths = extract_apply_patch_paths(&args);
                                if !new_paths.is_empty() {
                                    pending_patch_paths.insert(tool_call_id, new_paths);
                                }
                            }
                        }
                        Some(AgentEvent::ToolResult { tool_call_id, name, result }) => {
                            // Display-only: under parallel tool fan-out the
                            // first result clears the field while a sibling
                            // is still running; the next ToolExecuting
                            // repopulates it.
                            live.set_current_tool(None);
                            if name == "apply_patch" {
                                if let Some(paths) = pending_patch_paths.remove(&tool_call_id) {
                                    if is_apply_patch_success(&result) {
                                        if let Ok(mut guard) = files_written.lock() {
                                            for p in paths {
                                                if !guard.contains(&p) {
                                                    guard.push(p);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Some(AgentEvent::SteerReceived { text }) => {
                            // The child absorbed a parent-queued message into
                            // its history: clear the matching ledger entry so
                            // it isn't reported as undelivered at completion.
                            // Matched by value from the front — the channel is
                            // FIFO, so the oldest identical text is the one
                            // that just landed.
                            // Recover on poisoning: skipping the removal would
                            // report an absorbed message as undelivered.
                            let mut pending = pending_input
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            if let Some(pos) = pending.iter().position(|m| *m == text) {
                                pending.remove(pos);
                            }
                        }
                        _ => {}
                    }
                }
                _ = heartbeat_tick.tick(), if heartbeat_enabled => {
                    if let Some(ref b) = bus {
                        b.publish(AgentTreeEventKind::Heartbeat, &info);
                    }
                    match watchdog.on_tick() {
                        super::stall::TickOutcome::Quiet => {}
                        super::stall::TickOutcome::EmitStalled => {
                            if let Some(ref b) = bus {
                                b.publish(AgentTreeEventKind::Stalled, &info);
                            }
                        }
                        super::stall::TickOutcome::Cancel => {
                        // Grace exhausted with zero progress: cancel the
                        // wedged child and surface a structured stall error.
                        // The cancel unwinds cancel-aware await points in the
                        // send task; the bounded join below covers the rest.
                        let (_, in_tool) = live.snapshot();
                        let phase = if in_tool.is_some() { "in_tool" } else { "idle" };
                        let quiet_secs = u64::from(watchdog.idle_cycles()) * heartbeat_interval_secs;
                        let threshold_secs =
                            u64::from(stale_threshold) * heartbeat_interval_secs;
                        let grace_secs =
                            u64::from(STALL_GRACE_CYCLES) * heartbeat_interval_secs;
                        tracing::warn!(
                            %agent_id,
                            nickname = %nickname,
                            quiet_secs,
                            phase,
                            "sub-agent stalled — cancelling after exhausted grace window",
                        );
                        stall_error = Some(format!(
                            "stalled: no progress for {quiet_secs}s \
                             (threshold {threshold_secs}s, phase {phase}, \
                             grace {grace_secs}s exhausted)"
                        ));
                        cancel.cancel();
                        break;
                        }
                    }
                }
            }
        }

        if let Some(stall) = stall_error {
            // The cancel above unwinds cancel-aware await points, but a
            // blocking, non-cancel-aware operation inside the agent loop can
            // ignore it — bound the join so the watchdog's terminal guarantee
            // holds even against the wedge it just detected. One grace window
            // is the budget; past it, abort and abandon the task.
            let join_budget = std::time::Duration::from_secs(
                u64::from(STALL_GRACE_CYCLES) * heartbeat_interval_secs,
            );
            match tokio::time::timeout(join_budget, &mut send_handle).await {
                Ok(Err(e)) => tracing::warn!("sub-agent send_message task panicked: {e}"),
                Ok(Ok(_)) => {}
                Err(_) => {
                    send_handle.abort();
                    tracing::warn!(
                        %agent_id,
                        "stalled sub-agent ignored cancellation within the join budget; \
                         task aborted and abandoned",
                    );
                }
            }
            // Terminal stall: surfaces as `Errored { error }` (persisted +
            // shipped in the completion), so the parent gets the structured
            // stall context instead of a partial "completed" result.
            return Err(anyhow::anyhow!(stall));
        }

        if let Err(e) = send_handle.await {
            tracing::warn!("sub-agent send_message task panicked: {e}");
        }

        if final_text.is_empty() {
            Ok("(sub-agent completed with no text output)".to_string())
        } else {
            Ok(final_text)
        }
    })
}

#[cfg(test)]
mod compaction_cap_tests {
    use super::*;
    use crate::conversation::effective_history_budget_for;

    /// A parent on a million-token model whose configured history budget is
    /// far above the window, so the window (minus output reservation) is the
    /// binding threshold — the case where an uncapped child re-sends the
    /// whole prefix.
    /// Spec: the parent gets the text of the attempt that completes. Text
    /// of an earlier tool iteration stays in the result.
    #[test]
    fn the_result_text_drops_a_failed_attempt() {
        let delta = |text: &str| AgentEvent::TextDelta(text.to_string());
        let attempt = |attempt| AgentEvent::LlmAttemptStarted { attempt };
        // (name, events, result)
        let cases: Vec<(&str, Vec<AgentEvent>, &str)> = vec![
            (
                "retry after partial output",
                vec![
                    attempt(0),
                    delta("The answer is fo"),
                    AgentEvent::TextRetracted { bytes: 16 },
                    attempt(1),
                    delta("The answer is "),
                    delta("forty-two."),
                    AgentEvent::TurnComplete,
                ],
                "The answer is forty-two.",
            ),
            (
                "multi-byte text",
                vec![
                    delta("R\u{e9}sum\u{e9} \u{2014} pa"),
                    AgentEvent::TextRetracted { bytes: 15 },
                    delta("R\u{e9}sum\u{e9} \u{2014} page 1."),
                ],
                "R\u{e9}sum\u{e9} \u{2014} page 1.",
            ),
            (
                "a new attempt marker alone removes nothing",
                vec![attempt(0), delta("Step 1. "), attempt(0), delta("Step 2.")],
                "Step 1. Step 2.",
            ),
        ];
        for (name, events, want) in cases {
            let mut text = String::new();
            for event in &events {
                fold_result_text(&mut text, event);
            }
            assert_eq!(text, want, "{name}");
        }
    }

    fn million_window_parent() -> Config {
        let mut config = Config::default();
        config.llm.model = "claude-opus-5".to_string();
        config.conversation.max_history_tokens = 2_000_000;
        config
    }

    #[test]
    fn child_threshold_is_min_of_inherited_and_cap() {
        let parent = million_window_parent();
        let inherited = effective_history_budget_for(&parent);
        assert!(
            inherited > 500_000,
            "fixture must model a window-bound threshold, got {inherited}"
        );

        let cases: &[(usize, usize)] = &[
            (0, inherited),
            (200_000, 200_000),
            (inherited - 1, inherited - 1),
            (inherited, inherited),
            (inherited + 1, inherited),
            (usize::MAX, inherited),
        ];
        for (cap, expected) in cases {
            let child = clamp_child_history_budget(parent.clone(), *cap);
            assert_eq!(effective_history_budget_for(&child), *expected, "cap={cap}");
        }
    }

    #[test]
    fn default_setting_caps_child_at_200k_on_million_window_model() {
        let parent = million_window_parent();
        let agents = crate::config::MultiAgentConfig::default();
        let child = clamp_child_history_budget(parent.clone(), agents.compaction_threshold_tokens);
        assert_eq!(effective_history_budget_for(&child), 200_000);
        // The parent's own threshold is window-bound and untouched.
        assert!(effective_history_budget_for(&parent) > 500_000);
    }

    #[test]
    fn cap_never_raises_a_smaller_budget() {
        let mut parent = Config::default();
        parent.conversation.max_history_tokens = 32_000;
        let child = clamp_child_history_budget(parent, 200_000);
        assert_eq!(child.conversation.max_history_tokens, 32_000);
    }

    #[test]
    fn setting_round_trips_through_registry_and_disables_at_zero() {
        let mut cfg = Config::default();
        assert_eq!(cfg.agents.compaction_threshold_tokens, 200_000);
        cfg.apply_setting("agents.compaction_threshold_tokens", "50000")
            .expect("must apply");
        assert_eq!(cfg.agents.compaction_threshold_tokens, 50_000);
        let extractor = crate::settings::SETTING_REGISTRY
            .iter()
            .find(|(k, _)| *k == "agents.compaction_threshold_tokens")
            .map(|(_, f)| f)
            .expect("registered");
        assert_eq!(extractor(&cfg), "50000");

        cfg.apply_setting("agents.compaction_threshold_tokens", "0")
            .unwrap();
        let parent = million_window_parent();
        let child =
            clamp_child_history_budget(parent.clone(), cfg.agents.compaction_threshold_tokens);
        assert_eq!(
            effective_history_budget_for(&child),
            effective_history_budget_for(&parent),
            "0 must mean inherit"
        );
    }
}
