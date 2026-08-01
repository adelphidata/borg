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
const STALL_GRACE_CYCLES: u32 = 4;

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
    pub(super) input_rx: mpsc::Receiver<String>,
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

    if let Ok(db) = crate::db::Database::open().await {
        if let Err(e) = db.update_sub_agent_status(&agent_id, &status).await {
            tracing::warn!(%agent_id, "failed to update sub-agent status: {e}");
        }
    }

    if completion_tx
        .send(SubAgentCompletion {
            agent_id,
            nickname,
            status,
            final_response,
            files_written: files_written_vec,
        })
        .await
        .is_err()
    {
        tracing::debug!("sub-agent completion receiver dropped");
    }
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
    mut input_rx: mpsc::Receiver<String>,
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
        use crate::agent::{Agent, AgentEvent};

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

        if let Ok(db) = crate::db::Database::open().await {
            if let Err(e) = db
                .update_sub_agent_status(&agent_id, &SubAgentStatus::Running)
                .await
            {
                tracing::warn!(%agent_id, "failed to update sub-agent running status: {e}");
            }
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
        let stale_threshold = agents_config.heartbeat_stale_cycles.max(1);
        let mut heartbeat_tick =
            tokio::time::interval(std::time::Duration::from_secs(heartbeat_interval_secs));
        // Skip the immediate first tick — we want to wait one full interval
        // before the first heartbeat fires, not race the spawn.
        heartbeat_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        heartbeat_tick.tick().await;
        let mut idle_cycles: u32 = 0;
        let mut stalled_emitted = false;
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
                    // Reset the idle counter only on real progress
                    // signals — TextDelta (LLM produced text) and
                    // ToolResult (a tool finished). Chatty noise like
                    // ToolOutputDelta from a hung shell's stdout MUST
                    // NOT reset, or a hung child that keeps streaming
                    // stdout never trips the `Stalled` watchdog the
                    // operator relies on.
                    match event {
                        Some(AgentEvent::TextDelta(delta)) => {
                            idle_cycles = 0;
                            stalled_emitted = false;
                            live.touch();
                            final_text.push_str(&delta);
                        }
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
                            // ToolResult is a real progress signal — reset
                            // idle counter even on failure (a failed tool
                            // is still observable forward progress).
                            idle_cycles = 0;
                            stalled_emitted = false;
                            live.touch();
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
                        _ => {}
                    }
                }
                _ = heartbeat_tick.tick(), if heartbeat_enabled => {
                    if let Some(ref b) = bus {
                        b.publish(AgentTreeEventKind::Heartbeat, &info);
                    }
                    idle_cycles = idle_cycles.saturating_add(1);
                    if !stalled_emitted && idle_cycles >= stale_threshold {
                        stalled_emitted = true;
                        if let Some(ref b) = bus {
                            b.publish(AgentTreeEventKind::Stalled, &info);
                        }
                    }
                    if stalled_emitted
                        && idle_cycles >= stale_threshold.saturating_add(STALL_GRACE_CYCLES)
                    {
                        // Grace exhausted with zero progress: cancel the
                        // wedged child and surface a structured stall error.
                        // The cancel unwinds cancel-aware await points in the
                        // send task; the bounded join below covers the rest.
                        let (_, in_tool) = live.snapshot();
                        let phase = if in_tool.is_some() { "in_tool" } else { "idle" };
                        let quiet_secs = u64::from(idle_cycles) * heartbeat_interval_secs;
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
                msg = input_rx.recv() => {
                    match msg {
                        Some(_additional_message) => {
                            // Future: support sending additional messages to running sub-agents
                        }
                        None => break,
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
