/// Broadcast bus for agent-tree lifecycle events consumed by the TUI/CLI.
pub mod event_bus;
/// Name pool for generating unique sub-agent nicknames.
pub mod names;
/// Predefined role definitions for sub-agents.
pub mod roles;
/// Sub-agent spawn + execution machinery (driven by `AgentControl::spawn_agent`).
mod spawn;
/// Tool definitions for multi-agent orchestration.
pub mod tools;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Result};
use tokio::sync::{mpsc, Notify, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use event_bus::{AgentTreeBus, AgentTreeEvent, AgentTreeEventKind};

use crate::config::Config;

#[cfg(test)]
use spawn::{build_subagent_preamble, extract_apply_patch_paths, is_apply_patch_success};
use spawn::{run_sub_agent_task, SubAgentSpawn};

/// Delegation kind for a sub-agent. `Leaf` (default) cannot re-delegate
/// — the multi-agent tool surface is stripped from its tool definitions.
/// `Orchestrator` may re-delegate iff `agents.orchestrator_enabled` is true
/// *and* the numeric depth cap permits it. The kind is an overlay on top of
/// a named role: a "coder" role can be spawned as either a leaf or an
/// orchestrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentRoleKind {
    /// Worker child. Cannot spawn further sub-agents.
    #[default]
    Leaf,
    /// Coordinator child. May spawn grandchildren subject to depth and
    /// the `orchestrator_enabled` kill switch.
    Orchestrator,
}

impl AgentRoleKind {
    /// Parse a user-supplied kind literal. Unknown values fall back to
    /// `Leaf` — a safer default than rejecting the spawn outright since
    /// the LLM may type "orchestrate" or "leaf-only". The fallback emits
    /// a `tracing::warn!` so unexpected values aren't silently swallowed
    /// (the JSON schema declares this as a closed enum, so an unknown
    /// value indicates either an LLM mistake or schema drift).
    pub fn parse(s: &str) -> Self {
        let normalized = s.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "leaf" | "" => Self::Leaf,
            "orchestrator" | "orchestrate" => Self::Orchestrator,
            other => {
                tracing::warn!(
                    role_kind = other,
                    "spawn_agent: unknown role_kind, defaulting to leaf"
                );
                Self::Leaf
            }
        }
    }

    /// Stable string for telemetry / event payloads.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Leaf => "leaf",
            Self::Orchestrator => "orchestrator",
        }
    }
}

/// Role definition for a sub-agent.
///
/// **Note:** delegation kind (`Leaf` / `Orchestrator`) is intentionally
/// NOT a field on the role. It's a per-spawn overlay passed via the
/// `role_kind` argument to `spawn_agent`. A "coder" role can be spawned
/// as either a leaf or an orchestrator depending on the call site.
#[derive(Debug, Clone)]
pub struct AgentRole {
    /// Identifier for this role (e.g. "researcher", "coder").
    pub name: String,
    /// What this role does.
    pub description: String,
    /// LLM model override for this role.
    pub model: Option<String>,
    /// LLM provider override for this role.
    pub provider: Option<String>,
    /// Temperature override for this role.
    pub temperature: Option<f32>,
    /// Additional system prompt instructions.
    pub system_instructions: Option<String>,
    /// Allowlist of tool names this role can use.
    pub tools_allowed: Option<Vec<String>>,
    /// Maximum agent loop iterations for this role.
    pub max_iterations: Option<u32>,
}

/// State machine for sub-agent lifecycle.
#[derive(Debug, Clone, PartialEq)]
pub enum SubAgentStatus {
    /// Agent created but not yet started.
    PendingInit,
    /// Agent is actively processing.
    Running,
    /// Agent is paused at an iteration boundary; the loop is parked on its
    /// `Notify` gate and will resume when the operator clears the flag.
    Paused,
    /// Agent finished successfully with a result.
    Completed {
        /// The final text output from the sub-agent.
        result: String,
    },
    /// Agent encountered an error.
    Errored {
        /// Description of the error that occurred.
        error: String,
    },
    /// Agent was explicitly shut down.
    Shutdown,
}

impl SubAgentStatus {
    /// Returns a string representation suitable for database storage.
    pub fn as_str(&self) -> &str {
        match self {
            Self::PendingInit => "pending_init",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed { .. } => "completed",
            Self::Errored { .. } => "errored",
            Self::Shutdown => "shutdown",
        }
    }

    /// Whether the status represents a terminal (finished) state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Errored { .. } | Self::Shutdown
        )
    }

    /// Whether the status permits an external pause/resume transition.
    /// Paused itself is pausable (no-op resume); terminal states are not.
    pub fn is_pausable(&self) -> bool {
        matches!(self, Self::Running | Self::Paused | Self::PendingInit)
    }

    /// Reconstruct a status from database columns.
    pub fn from_db(status: &str, result_text: Option<&str>, error_text: Option<&str>) -> Self {
        match status {
            "pending_init" => Self::PendingInit,
            "running" => Self::Running,
            "paused" => Self::Paused,
            "completed" => Self::Completed {
                result: result_text.unwrap_or_default().to_string(),
            },
            "errored" => Self::Errored {
                error: error_text.unwrap_or_default().to_string(),
            },
            "shutdown" => Self::Shutdown,
            _ => Self::PendingInit,
        }
    }
}

/// Per-agent execution metrics. Updated on tool dispatch and LLM usage, and
/// flushed to `sub_agent_runs` on each terminal-state transition.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubAgentMetrics {
    /// Input/prompt tokens charged to this agent (across all turns).
    pub input_tokens: u64,
    /// Output/completion tokens produced by this agent.
    pub output_tokens: u64,
    /// Reasoning tokens (Anthropic extended-thinking, OpenAI o-series) when reported.
    pub reasoning_tokens: u64,
    /// Estimated USD cost computed from token counts × model rate-card.
    pub cost_usd: f64,
    /// Count of tool invocations this agent has dispatched.
    pub tool_calls: u64,
    /// Distinct file paths this agent has read or written, sorted.
    pub files_touched: Vec<String>,
}

impl SubAgentMetrics {
    /// Add another metrics record into this one. Used by `aggregate_subtree`.
    pub fn merge(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.cost_usd += other.cost_usd;
        self.tool_calls = self.tool_calls.saturating_add(other.tool_calls);
        for path in &other.files_touched {
            if !self.files_touched.iter().any(|p| p == path) {
                self.files_touched.push(path.clone());
            }
        }
        self.files_touched.sort();
    }

    /// Encode `files_touched` for `sub_agent_runs.files_touched` (JSON array).
    pub fn files_touched_json(&self) -> String {
        serde_json::to_string(&self.files_touched).unwrap_or_else(|_| "[]".to_string())
    }
}

/// Metadata for a running or completed sub-agent.
#[derive(Debug, Clone)]
pub struct SubAgentInfo {
    /// Unique identifier for this sub-agent.
    pub id: String,
    /// Human-friendly name (e.g. "Atlas", "Aurora").
    pub nickname: String,
    /// Role name this agent was spawned with.
    pub role: String,
    /// LLM model in use for this agent (snapshot of `child_config.llm.model`).
    pub model: String,
    /// Session ID of the parent agent that spawned this one.
    pub parent_session_id: String,
    /// This sub-agent's own session ID.
    pub session_id: String,
    /// Nesting depth (0 = top-level agent).
    pub depth: u32,
    /// Current lifecycle status.
    pub status: SubAgentStatus,
    /// Unix timestamp when this agent was created.
    pub created_at: i64,
    /// Unix timestamp when this agent reached a terminal state, if applicable.
    pub ended_at: Option<i64>,
    /// Accumulated execution metrics. Updated on tool dispatch + LLM usage.
    pub metrics: SubAgentMetrics,
}

/// One unit of work for `spawn_batch_and_wait`.
#[derive(Debug, Clone, Default)]
pub struct DelegatedTask {
    /// Task description sent to the child as its initial user message.
    pub goal: String,
    /// Background information rendered into the child's preamble under a
    /// `<context trust="parent">` block. Kept separate from `goal` so the
    /// child can distinguish what it should know vs. what it should do.
    pub context: Option<String>,
    /// Optional role name (looked up via `roles::load_role`).
    pub role_name: Option<String>,
    /// Delegation kind overlay (`Leaf` or `Orchestrator`). Default `Leaf`.
    pub role_kind: AgentRoleKind,
    /// Optional explicit toolset allowlist for this specific child.
    pub toolsets: Option<Vec<String>>,
    /// Optional model override for this specific child.
    pub model_override: Option<String>,
}

/// Result delivered from a sub-agent to its parent.
#[derive(Debug, Clone)]
pub struct SubAgentCompletion {
    /// Unique identifier of the completed sub-agent.
    pub agent_id: String,
    /// Human-friendly name of the sub-agent.
    pub nickname: String,
    /// Final lifecycle status (Completed, Errored, or Shutdown).
    pub status: SubAgentStatus,
    /// The sub-agent's final text output, if any.
    pub final_response: Option<String>,
    /// File paths the child mutated via `apply_patch` (any target —
    /// cwd, skills, channels). Surfaced to the parent's LLM so it knows
    /// which paths to re-read before continuing.
    ///
    /// **Scope limitation:** only `apply_patch` writes are tracked.
    /// Writes via `run_shell` (`sed -i …`, redirections, in-place
    /// formatters) are not detected — parsing arbitrary shell for write
    /// effects is fragile, and the canonical write tool in borg is
    /// `apply_patch`. Children that prefer shell-based edits will
    /// surface an incomplete list; the safe response on the parent
    /// side is to re-read any file it cares about, regardless of
    /// whether the child's reported list includes it.
    pub files_written: Vec<String>,
}

/// Handle to a spawned sub-agent.
struct SubAgentHandle {
    pub info: SubAgentInfo,
    pub input_tx: mpsc::Sender<String>,
    pub cancel: CancellationToken,
    /// Set true when the operator requests pause. The agent loop checks this
    /// at the top of each iteration and parks on `pause_notify` until cleared.
    pub is_paused: Arc<AtomicBool>,
    /// Notify woken when `is_paused` flips false, releasing the gate.
    pub pause_notify: Arc<Notify>,
    pub _join_handle: JoinHandle<()>,
}

/// Shared controller for managing sub-agents from a parent agent.
pub struct AgentControl {
    agents: HashMap<String, SubAgentHandle>,
    completion_tx: mpsc::Sender<SubAgentCompletion>,
    completion_rx: mpsc::Receiver<SubAgentCompletion>,
    /// Buffer for completions consumed while waiting for a specific agent.
    completion_buffer: Vec<SubAgentCompletion>,
    semaphore: Arc<Semaphore>,
    /// Maximum allowed nesting depth for spawned sub-agents.
    pub max_spawn_depth: u32,
    /// Maximum number of active children a single agent can spawn.
    pub max_children_per_agent: u32,
    _max_concurrent: u32,
    name_pool: names::NamePool,
    parent_session_id: String,
    current_depth: u32,
    /// Optional broadcast bus. Lifecycle events publish here when set so the
    /// gRPC `SubscribeAgentTreeEvents` stream can fan them out to operators.
    /// Optional so unit tests and embedded callers don't need to wire one.
    bus: Option<AgentTreeBus>,
}

/// Error variants from `AgentControl` mutating ops. Distinguished so callers
/// (gRPC, CLI) can map them to specific status codes / exit codes rather
/// than a generic stringly-typed `anyhow::Error`.
#[derive(Debug, thiserror::Error)]
pub enum AgentControlError {
    /// The agent id is unknown to this controller.
    #[error("agent '{0}' not found")]
    NotFound(String),
    /// The agent is in a terminal state and cannot be paused/resumed.
    #[error("agent '{id}' is not pausable (status: {status})")]
    NotPausable {
        /// The agent id that was rejected.
        id: String,
        /// The current status string at the time of rejection.
        status: String,
    },
}

impl AgentControl {
    /// Create a new controller for the given parent session and depth.
    pub fn new(
        config: &crate::config::MultiAgentConfig,
        parent_session_id: &str,
        current_depth: u32,
    ) -> Self {
        let (completion_tx, completion_rx) = mpsc::channel(64);
        Self {
            agents: HashMap::new(),
            completion_tx,
            completion_rx,
            completion_buffer: Vec::new(),
            semaphore: Arc::new(Semaphore::new(config.max_concurrent as usize)),
            max_spawn_depth: config.max_spawn_depth,
            max_children_per_agent: config.max_children_per_agent,
            _max_concurrent: config.max_concurrent,
            name_pool: names::NamePool::new(),
            parent_session_id: parent_session_id.to_string(),
            current_depth,
            bus: None,
        }
    }

    /// Attach an event bus. Subsequent state mutations publish to it.
    pub fn with_bus(mut self, bus: AgentTreeBus) -> Self {
        self.bus = Some(bus);
        self
    }

    /// The session this controller is rooted under.
    pub fn parent_session_id(&self) -> &str {
        &self.parent_session_id
    }

    /// Current depth of the parent in the agent tree. Exposed so callers
    /// (e.g. the `spawn_agent` tool handler) can compute orchestrator
    /// gating decisions without reaching into private state.
    pub fn current_depth(&self) -> u32 {
        self.current_depth
    }

    /// Publish to the bus if one is attached. Best-effort — a closed channel
    /// just logs and drops, since the bus is an observer, not a participant.
    fn publish(&self, kind: AgentTreeEventKind, info: &SubAgentInfo) {
        if let Some(ref bus) = self.bus {
            bus.publish(kind, info);
        }
    }

    /// Build a sub-agent config by applying role overrides and model override to the parent config.
    ///
    /// Precedence for the effective LLM model/provider, highest first:
    ///   1. `model_override` (explicit `spawn_agent` arg) / role.provider
    ///   2. `role.model` / `role.provider`
    ///   3. `agents.delegation_model` / `agents.delegation_provider`
    ///   4. Inherited parent value.
    ///
    /// The child's `ExecutionPolicy` is rebuilt from scratch: the parent's
    /// deny list is preserved (a child can never escalate past denies),
    /// and the auto-approve list is `["*"]` (wildcard). This matches
    /// borg's "no approval prompts" UX invariant — children must execute
    /// without prompting since they have no UI to ask. A previous build
    /// gated `auto_approve` on `agents.subagent_auto_approve`, but
    /// `ExecutionPolicy::check` (policy.rs) falls through to AutoApprove
    /// when nothing matches, making the gate a silent no-op.
    fn build_sub_agent_config(
        &self,
        role: Option<&AgentRole>,
        model_override: Option<&str>,
        parent_config: &Config,
    ) -> Config {
        let mut child_config = parent_config.clone();

        // Layer 3: per-spawn delegation defaults from config.
        if let Some(ref m) = parent_config.agents.delegation_model {
            child_config.llm.model = m.clone();
        }
        if let Some(ref p) = parent_config.agents.delegation_provider {
            child_config.llm.provider = Some(p.clone());
        }

        // Layer 2: role overrides win over delegation defaults.
        if let Some(role) = role {
            if let Some(ref model) = role.model {
                child_config.llm.model = model.clone();
            }
            if let Some(ref provider) = role.provider {
                child_config.llm.provider = Some(provider.to_string());
            }
            if let Some(temp) = role.temperature {
                child_config.llm.temperature = temp;
            }
            if let Some(max_iter) = role.max_iterations {
                child_config.conversation.max_iterations = max_iter;
            }
        }

        // Layer 1: explicit per-call override wins above all.
        if let Some(model) = model_override {
            child_config.llm.model = model.to_string();
        }

        // Sub-agent execution policy. The parent's deny list always carries
        // over so a child can never escalate past a parent's denial. The
        // auto-approve list is `["*"]` because borg's UX invariant
        // ("No Approval Prompts" — see CLAUDE.md) requires children to
        // execute without confirmation; there is no UI to prompt against.
        // Catastrophic commands are still blocked at the sandbox layer.
        child_config.policy = crate::policy::ExecutionPolicy {
            auto_approve: vec!["*".to_string()],
            deny: parent_config.policy.deny.clone(),
        };

        child_config
    }

    /// Build the SubAgentInfo and record it in the database.
    async fn prepare_sub_agent_info(
        &self,
        agent_id: &str,
        nickname: &str,
        role_name: &str,
        model: &str,
        session_id: &str,
        child_depth: u32,
    ) -> SubAgentInfo {
        let info = SubAgentInfo {
            id: agent_id.to_string(),
            nickname: nickname.to_string(),
            role: role_name.to_string(),
            model: model.to_string(),
            parent_session_id: self.parent_session_id.clone(),
            session_id: session_id.to_string(),
            depth: child_depth,
            status: SubAgentStatus::PendingInit,
            created_at: chrono::Utc::now().timestamp(),
            ended_at: None,
            metrics: SubAgentMetrics::default(),
        };

        // Record in DB
        match crate::db::Database::open().await {
            Ok(db) => {
                if let Err(e) = db
                    .insert_sub_agent_run(
                        agent_id,
                        nickname,
                        role_name,
                        &self.parent_session_id,
                        session_id,
                        child_depth,
                    )
                    .await
                {
                    tracing::warn!(agent_id, "failed to insert sub-agent run record: {e}");
                }
                // Persist the effective model in the same DB round-trip
                // window — without this, every row's `model` column stays
                // empty and the dashboard surface can't show "which model
                // is this agent running on" without re-resolving from
                // memory state.
                if let Err(e) = db.update_sub_agent_model(agent_id, model).await {
                    tracing::warn!(agent_id, "failed to persist sub-agent model: {e}");
                }
            }
            Err(e) => {
                tracing::warn!(
                    agent_id,
                    "db open failed while recording sub-agent spawn: {e}"
                );
            }
        }

        info
    }

    /// Validate spawn limits (depth and active children count).
    fn validate_spawn_limits(&self) -> Result<()> {
        if self.current_depth >= self.max_spawn_depth {
            bail!(
                "Cannot spawn sub-agent: max spawn depth ({}) reached",
                self.max_spawn_depth
            );
        }

        let active_count = self
            .agents
            .values()
            .filter(|h| {
                matches!(
                    h.info.status,
                    SubAgentStatus::PendingInit | SubAgentStatus::Running
                )
            })
            .count();
        if active_count >= self.max_children_per_agent as usize {
            bail!(
                "Cannot spawn sub-agent: max children ({}) reached",
                self.max_children_per_agent
            );
        }

        Ok(())
    }

    /// Default blocklist for tools that a delegated sub-agent should never
    /// be given, regardless of what the role's allowlist looks like. Keeps
    /// children from delegating further, asking the user mid-task, or
    /// mutating long-term memory behind the parent's back.
    pub const DELEGATE_DEFAULT_BLOCKLIST: &'static [&'static str] = &[
        "spawn_agent",
        "wait_for_agent",
        "close_agent",
        "manage_roles",
        "request_user_input",
        "write_memory",
    ];

    /// Spawn a new sub-agent. Returns (agent_id, nickname).
    ///
    /// Parameters:
    /// - `message`: the goal (first user message).
    /// - `context`: optional background block rendered into the child's
    ///   preamble under `<context trust="parent">`. Separates "what to know"
    ///   from "what to do".
    /// - `role`: optional named role (model/temperature/tool allowlist).
    /// - `role_kind`: delegation kind overlay. `Leaf` (default) cannot
    ///   re-delegate; `Orchestrator` can iff
    ///   `agents.orchestrator_enabled && depth + 1 < max_spawn_depth`.
    /// - `toolsets`: optional explicit per-call allowlist intersected with
    ///   the role's allowlist and the parent's enabled tools.
    /// - `tools_blocklist`: takes precedence over any allowlist; names in
    ///   the blocklist are stripped from the effective tool filter. The
    ///   `assemble_delegate_blocklist` helper in `tools::` is the canonical
    ///   producer (handles delegate defaults + Plan-mode mutating union +
    ///   orchestrator re-delegation re-enabling).
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_agent(
        &mut self,
        message: &str,
        context: Option<&str>,
        role: Option<AgentRole>,
        role_kind: AgentRoleKind,
        toolsets: Option<&[String]>,
        nickname: Option<&str>,
        model_override: Option<&str>,
        parent_config: &Config,
        fork_context: Option<&[crate::types::Message]>,
        tools_blocklist: Option<&[&str]>,
    ) -> Result<(String, String)> {
        self.validate_spawn_limits()?;

        let agent_id = uuid::Uuid::new_v4().to_string();
        let chosen_nickname = match nickname {
            Some(n) => n.to_string(),
            None => self.name_pool.next_name(),
        };
        let role_name = role
            .as_ref()
            .map_or("default", |r| r.name.as_str())
            .to_string();

        let child_config =
            self.build_sub_agent_config(role.as_ref(), model_override, parent_config);

        let child_depth = self.current_depth + 1;
        let max_spawn_depth = self.max_spawn_depth;
        let session_id = uuid::Uuid::new_v4().to_string();

        let info = self
            .prepare_sub_agent_info(
                &agent_id,
                &chosen_nickname,
                &role_name,
                &child_config.llm.model,
                &session_id,
                child_depth,
            )
            .await;
        self.publish(AgentTreeEventKind::Spawned, &info);

        // Spawn tokio task
        let (input_tx, input_rx) = mpsc::channel::<String>(16);
        let cancel = CancellationToken::new();
        let is_paused = Arc::new(AtomicBool::new(false));
        let pause_notify = Arc::new(Notify::new());
        let tools_filter =
            compute_tools_filter(role.as_ref(), toolsets, tools_blocklist, parent_config).await;

        let spawn_ctx = SubAgentSpawn {
            agent_id: agent_id.clone(),
            nickname: chosen_nickname.clone(),
            config: child_config,
            depth: child_depth,
            max_spawn_depth,
            agents_config: parent_config.agents.clone(),
            message: message.to_string(),
            context: context.map(str::to_string),
            role,
            role_kind,
            fork_context: fork_context.map(<[crate::types::Message]>::to_vec),
            input_rx,
            cancel: cancel.clone(),
            is_paused: is_paused.clone(),
            pause_notify: pause_notify.clone(),
            tools_filter,
            bus: self.bus.clone(),
            info: info.clone(),
            semaphore: self.semaphore.clone(),
            completion_tx: self.completion_tx.clone(),
        };
        let join_handle = tokio::spawn(run_sub_agent_task(spawn_ctx));

        // Store handle
        let handle = SubAgentHandle {
            info,
            input_tx,
            cancel,
            is_paused,
            pause_notify,
            _join_handle: join_handle,
        };
        self.agents.insert(agent_id.clone(), handle);

        Ok((agent_id, chosen_nickname))
    }

    /// Pause or resume a sub-agent at its next iteration boundary.
    ///
    /// Cooperative — an in-flight LLM call finishes; the gate takes effect at
    /// the top of the next loop iteration. Terminal agents return
    /// `NotPausable` (callers map this to gRPC `FailedPrecondition`).
    pub async fn set_paused(
        &mut self,
        agent_id: &str,
        paused: bool,
    ) -> std::result::Result<(), AgentControlError> {
        let handle = self
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| AgentControlError::NotFound(agent_id.to_string()))?;
        if !handle.info.status.is_pausable() {
            return Err(AgentControlError::NotPausable {
                id: agent_id.to_string(),
                status: handle.info.status.as_str().to_string(),
            });
        }

        let new_status = if paused {
            SubAgentStatus::Paused
        } else {
            SubAgentStatus::Running
        };
        handle.is_paused.store(paused, Ordering::Release);
        // Wake the gate so the loop observes the flip. Safe to call when
        // not paused too — no waiters means it's a cheap no-op.
        handle.pause_notify.notify_waiters();
        handle.info.status = new_status.clone();
        let info_clone = handle.info.clone();

        match crate::db::Database::open().await {
            Ok(db) => {
                if let Err(e) = db.update_sub_agent_status(agent_id, &new_status).await {
                    tracing::warn!(agent_id, "failed to persist pause/resume status: {e}");
                }
            }
            Err(e) => {
                tracing::warn!(agent_id, "db open failed during pause/resume: {e}");
            }
        }

        let kind = if paused {
            AgentTreeEventKind::Paused
        } else {
            AgentTreeEventKind::Resumed
        };
        self.publish(kind, &info_clone);
        Ok(())
    }

    /// Cancel the agent rooted at `root_id` and every descendant whose
    /// `parent_session_id` chains back to it. Order: children before parent
    /// so the parent sees a clean shutdown rather than a half-cancelled
    /// subtree.
    pub async fn shutdown_subtree(&mut self, root_id: &str) -> Result<Vec<String>> {
        if !self.agents.contains_key(root_id) {
            bail!("agent '{root_id}' not found");
        }
        // BFS by session_id → parent_session_id to collect descendants.
        let mut to_kill: Vec<String> = Vec::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(root_id.to_string());
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(root_id.to_string());
        while let Some(cur_id) = queue.pop_front() {
            let cur_session = match self.agents.get(&cur_id) {
                Some(h) => h.info.session_id.clone(),
                None => continue,
            };
            for (id, h) in &self.agents {
                if h.info.parent_session_id == cur_session && !seen.contains(id) {
                    seen.insert(id.clone());
                    queue.push_back(id.clone());
                }
            }
            to_kill.push(cur_id);
        }
        // Reverse so leaves shut down first.
        to_kill.reverse();
        for id in &to_kill {
            // Reuse single-agent shutdown so DB + bus publish stay consistent.
            if let Err(e) = self.shutdown_agent(id).await {
                tracing::warn!(agent_id = %id, "shutdown_subtree leg failed: {e}");
            }
        }
        Ok(to_kill)
    }

    /// Sum metrics across `root_id` and all descendants. Read-only.
    pub fn aggregate_subtree(&self, root_id: &str) -> Option<SubAgentMetrics> {
        let root = self.agents.get(root_id)?;
        let mut acc = root.info.metrics.clone();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(root.info.session_id.clone());
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(root.info.session_id.clone());
        while let Some(cur_session) = queue.pop_front() {
            for h in self.agents.values() {
                if h.info.parent_session_id == cur_session && !seen.contains(&h.info.session_id) {
                    seen.insert(h.info.session_id.clone());
                    acc.merge(&h.info.metrics);
                    queue.push_back(h.info.session_id.clone());
                }
            }
        }
        Some(acc)
    }

    /// Send an additional message to a running sub-agent.
    pub async fn send_input(&self, agent_id: &str, message: &str) -> Result<()> {
        let handle = self
            .agents
            .get(agent_id)
            .ok_or_else(|| anyhow::anyhow!("Agent '{agent_id}' not found"))?;
        handle
            .input_tx
            .send(message.to_string())
            .await
            .map_err(|_| anyhow::anyhow!("Agent '{agent_id}' is no longer running"))?;
        Ok(())
    }

    /// Wait for a specific sub-agent to complete.
    pub async fn wait_for_agent(
        &mut self,
        agent_id: &str,
        timeout_secs: u64,
    ) -> Result<SubAgentCompletion> {
        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        let deadline = tokio::time::Instant::now() + timeout;

        if let Some(handle) = self.agents.get(agent_id) {
            match &handle.info.status {
                SubAgentStatus::Completed { .. }
                | SubAgentStatus::Errored { .. }
                | SubAgentStatus::Shutdown => {
                    return Ok(SubAgentCompletion {
                        agent_id: agent_id.to_string(),
                        nickname: handle.info.nickname.clone(),
                        status: handle.info.status.clone(),
                        final_response: match &handle.info.status {
                            SubAgentStatus::Completed { result } => Some(result.clone()),
                            _ => None,
                        },
                        files_written: handle.info.metrics.files_touched.clone(),
                    });
                }
                _ => {}
            }
        }

        // Drain any buffered completion left by a previous wait_for_agent call
        // that received this agent's completion while waiting for another.
        if let Some(idx) = self
            .completion_buffer
            .iter()
            .position(|c| c.agent_id == agent_id)
        {
            return Ok(self.completion_buffer.remove(idx));
        }

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!("Timeout waiting for agent '{agent_id}'");
            }

            match tokio::time::timeout(remaining, self.completion_rx.recv()).await {
                Ok(Some(completion)) => {
                    let completed_id = completion.agent_id.clone();
                    // Update internal state. Persist files_written into
                    // metrics.files_touched so the early-return path on
                    // a second wait_for_agent call (or list_agents /
                    // get_status / DB query) sees the correct list —
                    // otherwise the next caller would read an empty Vec.
                    if let Some(handle) = self.agents.get_mut(&completed_id) {
                        handle.info.status = completion.status.clone();
                        handle.info.metrics.files_touched = completion.files_written.clone();
                    }
                    if completed_id == agent_id {
                        return Ok(completion);
                    }
                    // Not the one we want — buffer it for later retrieval
                    self.completion_buffer.push(completion);
                }
                Ok(None) => bail!("Completion channel closed"),
                Err(_) => bail!("Timeout waiting for agent '{agent_id}'"),
            }
        }
    }

    /// List all sub-agents with their current info.
    pub fn list_agents(&self) -> Vec<SubAgentInfo> {
        self.agents.values().map(|h| h.info.clone()).collect()
    }

    /// Get status of a specific agent.
    pub fn get_status(&self, agent_id: &str) -> Option<&SubAgentInfo> {
        self.agents.get(agent_id).map(|h| &h.info)
    }

    /// Shut down a specific sub-agent.
    pub async fn shutdown_agent(&mut self, agent_id: &str) -> Result<()> {
        let handle = self
            .agents
            .get_mut(agent_id)
            .ok_or_else(|| anyhow::anyhow!("Agent '{agent_id}' not found"))?;
        handle.cancel.cancel();
        // Wake the pause gate if the agent is parked there — otherwise the
        // cancellation token notification never reaches the select loop.
        handle.is_paused.store(false, Ordering::Release);
        handle.pause_notify.notify_waiters();
        handle.info.status = SubAgentStatus::Shutdown;
        handle.info.ended_at = Some(chrono::Utc::now().timestamp());
        let info_clone = handle.info.clone();

        match crate::db::Database::open().await {
            Ok(db) => {
                if let Err(e) = db
                    .update_sub_agent_status(agent_id, &SubAgentStatus::Shutdown)
                    .await
                {
                    tracing::warn!(agent_id, "failed to update sub-agent shutdown status: {e}");
                }
            }
            Err(e) => {
                tracing::warn!(agent_id, "db open failed during shutdown: {e}");
            }
        }

        self.publish(AgentTreeEventKind::Killed, &info_clone);
        Ok(())
    }

    /// Drain any pending completions without blocking.
    pub fn drain_completions(&mut self) -> Vec<SubAgentCompletion> {
        // Start with any buffered completions from wait_for_agent
        let mut completions: Vec<SubAgentCompletion> = self.completion_buffer.drain(..).collect();
        while let Ok(completion) = self.completion_rx.try_recv() {
            if let Some(handle) = self.agents.get_mut(&completion.agent_id) {
                handle.info.status = completion.status.clone();
                handle.info.metrics.files_touched = completion.files_written.clone();
            }
            completions.push(completion);
        }
        completions
    }

    /// Shut down all sub-agents.
    pub fn shutdown_all(&mut self) {
        for handle in self.agents.values_mut() {
            handle.cancel.cancel();
            handle.is_paused.store(false, Ordering::Release);
            handle.pause_notify.notify_waiters();
            handle.info.status = SubAgentStatus::Shutdown;
        }
    }

    /// Spawn a sub-agent, block until it finishes, and return the completion.
    ///
    /// Collapses the common `spawn_agent` → `wait_for_agent` pattern into a
    /// single in-loop call so the LLM doesn't burn a round-trip to start and
    /// then wait. Same parameters as [`Self::spawn_agent`] plus an explicit
    /// `timeout_secs`.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_and_wait(
        &mut self,
        message: &str,
        context: Option<&str>,
        role: Option<AgentRole>,
        role_kind: AgentRoleKind,
        toolsets: Option<&[String]>,
        nickname: Option<&str>,
        model_override: Option<&str>,
        parent_config: &Config,
        fork_context: Option<&[crate::types::Message]>,
        tools_blocklist: Option<&[&str]>,
        timeout_secs: u64,
    ) -> Result<SubAgentCompletion> {
        let (agent_id, _nickname) = self
            .spawn_agent(
                message,
                context,
                role,
                role_kind,
                toolsets,
                nickname,
                model_override,
                parent_config,
                fork_context,
                tools_blocklist,
            )
            .await?;
        self.wait_for_agent(&agent_id, timeout_secs).await
    }

    /// Batch-spawn multiple tasks in parallel, wait for all of them, and
    /// return completions in the same order as the input tasks.
    ///
    /// Parallelism is naturally bounded by the controller's semaphore
    /// (`max_concurrent`). Completions arrive on the shared channel in
    /// whatever order the children finish; the returned `Vec` is reordered
    /// so index `i` matches `tasks[i]`. Tasks whose spawn fails return
    /// `SubAgentStatus::Errored` rather than aborting the batch.
    ///
    /// **Per-task blocklist.** The blocklist is computed *inside* the loop
    /// using each task's own `role_kind`. A previous version computed a
    /// single batch-wide blocklist outside the loop, which let a leaf task
    /// in a mixed batch inherit the orchestrator carve-out — see the
    /// `mixed_batch_each_task_gets_own_blocklist` regression test.
    pub async fn spawn_batch_and_wait(
        &mut self,
        tasks: Vec<DelegatedTask>,
        parent_config: &Config,
        timeout_secs: u64,
    ) -> Result<Vec<SubAgentCompletion>> {
        let mut agent_ids: Vec<std::result::Result<String, SubAgentCompletion>> =
            Vec::with_capacity(tasks.len());
        let depth = self.current_depth;
        for (i, task) in tasks.iter().enumerate() {
            let role = match task.role_name.as_deref() {
                Some(name) => roles::load_role(name).await,
                None => None,
            };
            // Compute the blocklist using THIS task's role_kind. A mixed
            // batch with an orchestrator must not promote sibling leaves.
            let can_orch = tools::can_orchestrate(parent_config, depth);
            let per_task_blocklist =
                tools::assemble_delegate_blocklist(parent_config, task.role_kind, can_orch);
            let per_task_refs: Vec<&str> = per_task_blocklist.to_vec();
            match self
                .spawn_agent(
                    &task.goal,
                    task.context.as_deref(),
                    role,
                    task.role_kind,
                    task.toolsets.as_deref(),
                    None,
                    task.model_override.as_deref(),
                    parent_config,
                    None,
                    Some(&per_task_refs),
                )
                .await
            {
                Ok((id, _)) => agent_ids.push(Ok(id)),
                Err(e) => agent_ids.push(Err(SubAgentCompletion {
                    // Use diagnosable sentinels so JSON output is clearly
                    // identifiable as a spawn-failure row rather than looking
                    // like a real child that never started.
                    agent_id: format!("spawn-failed-{i}"),
                    nickname: format!("task-{i}"),
                    status: SubAgentStatus::Errored {
                        error: format!("spawn failed: {e}"),
                    },
                    final_response: None,
                    files_written: Vec::new(),
                })),
            }
        }

        // `wait_for_agent` drains completions from the shared channel and
        // buffers out-of-order arrivals so iterating in spawn-order is correct.
        let mut completions: Vec<SubAgentCompletion> = Vec::with_capacity(agent_ids.len());
        for entry in agent_ids {
            match entry {
                Ok(agent_id) => {
                    let completion = self.wait_for_agent(&agent_id, timeout_secs).await?;
                    completions.push(completion);
                }
                Err(err_completion) => completions.push(err_completion),
            }
        }

        Ok(completions)
    }

    /// Returns true if no sub-agents have been spawned.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Test helper: get a clone of the completion sender for injecting test completions.
    #[cfg(test)]
    pub fn test_completion_tx(&self) -> mpsc::Sender<SubAgentCompletion> {
        self.completion_tx.clone()
    }

    /// Test helper: insert a mock SubAgentHandle for testing wait/shutdown operations.
    #[cfg(test)]
    pub fn insert_mock_handle(&mut self, agent_id: &str, nickname: &str) -> CancellationToken {
        self.insert_mock_handle_with_parent(agent_id, nickname, &self.parent_session_id.clone())
    }

    /// Test helper variant that lets a test wire a custom parent_session_id —
    /// needed to build multi-level trees in shutdown_subtree / aggregate tests.
    #[cfg(test)]
    pub fn insert_mock_handle_with_parent(
        &mut self,
        agent_id: &str,
        nickname: &str,
        parent_session_id: &str,
    ) -> CancellationToken {
        let (input_tx, _input_rx) = mpsc::channel::<String>(1);
        let cancel = CancellationToken::new();
        let is_paused = Arc::new(AtomicBool::new(false));
        let pause_notify = Arc::new(Notify::new());
        let handle = SubAgentHandle {
            info: SubAgentInfo {
                id: agent_id.to_string(),
                nickname: nickname.to_string(),
                role: "test".to_string(),
                model: "test-model".to_string(),
                parent_session_id: parent_session_id.to_string(),
                session_id: uuid::Uuid::new_v4().to_string(),
                depth: self.current_depth + 1,
                status: SubAgentStatus::Running,
                created_at: chrono::Utc::now().timestamp(),
                ended_at: None,
                metrics: SubAgentMetrics::default(),
            },
            input_tx,
            cancel: cancel.clone(),
            is_paused,
            pause_notify,
            _join_handle: tokio::spawn(async {}),
        };
        self.agents.insert(agent_id.to_string(), handle);
        cancel
    }

    /// Test helper: seed a specific session_id for a mock handle. Returns the
    /// session_id so callers can chain children's parent_session_id.
    #[cfg(test)]
    pub fn insert_mock_handle_with_session(
        &mut self,
        agent_id: &str,
        nickname: &str,
        parent_session_id: &str,
        session_id: &str,
    ) -> (CancellationToken, String) {
        let cancel = self.insert_mock_handle_with_parent(agent_id, nickname, parent_session_id);
        if let Some(handle) = self.agents.get_mut(agent_id) {
            handle.info.session_id = session_id.to_string();
        }
        (cancel, session_id.to_string())
    }

    /// Test helper: poke metrics on an existing handle.
    #[cfg(test)]
    pub fn set_metrics(&mut self, agent_id: &str, metrics: SubAgentMetrics) {
        if let Some(handle) = self.agents.get_mut(agent_id) {
            handle.info.metrics = metrics;
        }
    }
}

impl Drop for AgentControl {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

/// Compute the effective `tools_allowed` filter for a new sub-agent.
///
/// Layered intersection (each narrows the previous):
/// 1. Start with the role's `tools_allowed` if present, else the parent's
///    full enabled tool surface (core + multi-agent).
/// 2. If `toolsets` is `Some` (per-call narrowing from the LLM), intersect.
/// 3. If `blocklist` is `Some`, subtract.
///
/// Returns `None` only when every layer is unset — i.e. the child inherits
/// all parent tools verbatim. Returns `Some(Vec::new())` when narrowing
/// reduces the set to empty (a legitimate caller intent, e.g.
/// `toolsets: []` means "no tools beyond memory/exit").
async fn compute_tools_filter(
    role: Option<&AgentRole>,
    toolsets: Option<&[String]>,
    blocklist: Option<&[&str]>,
    parent_config: &Config,
) -> Option<Vec<String>> {
    let role_allow = role.and_then(|r| r.tools_allowed.clone());

    // No constraints at all → inherit everything from parent.
    if role_allow.is_none() && toolsets.is_none() && blocklist.is_none() {
        return None;
    }

    // Layer 1: starting set.
    let mut allowed: Vec<String> = match role_allow {
        Some(a) => a,
        None => {
            let mut parent_tools =
                crate::tool::definitions::core_tool_definitions(parent_config).await;
            parent_tools.extend(tools::tool_definitions(0, u32::MAX));
            parent_tools.into_iter().map(|t| t.function.name).collect()
        }
    };

    // Layer 2: intersect with explicit per-call toolsets.
    if let Some(narrow) = toolsets {
        let narrow_set: std::collections::HashSet<&str> =
            narrow.iter().map(String::as_str).collect();
        allowed.retain(|t| narrow_set.contains(t.as_str()));
    }

    // Layer 3: subtract blocklist.
    if let Some(block) = blocklist {
        let block_set: std::collections::HashSet<&str> = block.iter().copied().collect();
        allowed.retain(|t| !block_set.contains(t.as_str()));
    }

    Some(allowed)
}

/// Tests for `/agents`-feature runtime additions: Paused status, pause gate,
/// shutdown_subtree, aggregate_subtree, AgentTreeBus integration, and the
/// pausable/terminal precondition checks. Distinct from the legacy `tests`
/// module above (which is feature-gated and disabled) so these always run.
#[cfg(test)]
mod agents_feature_tests {
    use super::*;
    use crate::config::MultiAgentConfig;
    use std::time::Duration;
    use tokio::sync::broadcast::error::TryRecvError;

    fn ctrl() -> AgentControl {
        let cfg = MultiAgentConfig {
            enabled: true,
            max_spawn_depth: 5,
            max_children_per_agent: 10,
            max_concurrent: 10,
            delegate_timeout_secs: 60,
            ..MultiAgentConfig::default()
        };
        AgentControl::new(&cfg, "root-session", 0)
    }

    /// `set_paused` must flip both the in-memory status AND the
    /// `is_paused` AtomicBool. Asserting both sides catches a regression
    /// where the status display says "paused" but the loop is still busy
    /// (or vice versa) — the dual-write contract the run_sub_agent gate
    /// depends on.
    #[tokio::test]
    async fn set_paused_flips_status_and_atomic() {
        let mut c = ctrl();
        c.insert_mock_handle("a", "atlas");
        assert_eq!(c.get_status("a").unwrap().status, SubAgentStatus::Running);
        c.set_paused("a", true).await.expect("pause running agent");
        assert_eq!(c.get_status("a").unwrap().status, SubAgentStatus::Paused);
        let handle_paused = c.agents.get("a").unwrap().is_paused.load(Ordering::Acquire);
        assert!(handle_paused, "is_paused AtomicBool must be true");

        c.set_paused("a", false).await.expect("resume paused agent");
        assert_eq!(c.get_status("a").unwrap().status, SubAgentStatus::Running);
        let handle_running = c.agents.get("a").unwrap().is_paused.load(Ordering::Acquire);
        assert!(!handle_running, "is_paused must clear on resume");
    }

    /// Driving a fake counter under the gate proves pause is not just
    /// observed but actually parks execution. Resuming must release the
    /// gate — caught a real regression where notify_waiters() was missing.
    ///
    /// Uses an unbounded loop with a per-iteration sleep so the test
    /// remains deterministic even on a heavily-loaded CI box: the
    /// pause/resume windows are large multiples of the iteration delay,
    /// and an explicit `stop` flag terminates the task at end-of-test
    /// rather than relying on the loop counter to bound it.
    #[tokio::test]
    async fn pause_gate_blocks_iteration_and_resume_releases() {
        use std::sync::atomic::AtomicU32;
        let is_paused = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(Notify::new());
        let counter = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let is_paused_t = is_paused.clone();
        let notify_t = notify.clone();
        let counter_t = counter.clone();
        let stop_t = stop.clone();
        let task = tokio::spawn(async move {
            while !stop_t.load(Ordering::Acquire) {
                while is_paused_t.load(Ordering::Acquire) {
                    if stop_t.load(Ordering::Acquire) {
                        return;
                    }
                    notify_t.notified().await;
                }
                counter_t.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        // Let the loop tick a few times — counter should advance.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pre_pause = counter.load(Ordering::SeqCst);
        assert!(
            pre_pause > 0,
            "loop must tick before pause is invoked (got {pre_pause})"
        );

        // Engage pause; observe the counter freeze across a window much
        // longer than the iteration delay.
        is_paused.store(true, Ordering::Release);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let after_pause_window = counter.load(Ordering::SeqCst);
        // Allow up to 2 in-flight ticks (one was past the gate when we
        // flipped, one is statistically possible in the storage-acquire
        // race). Anything more means the gate didn't engage.
        assert!(
            after_pause_window <= pre_pause + 2,
            "counter must not advance more than 2 ticks under pause; \
             pre_pause={pre_pause} after={after_pause_window}"
        );

        // Release the gate; counter must resume advancing.
        is_paused.store(false, Ordering::Release);
        notify.notify_waiters();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let after_resume = counter.load(Ordering::SeqCst);
        assert!(
            after_resume > after_pause_window + 2,
            "counter must advance after resume; \
             paused_at={after_pause_window} after_resume={after_resume}"
        );

        // Clean teardown.
        stop.store(true, Ordering::Release);
        notify.notify_waiters();
        let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
    }

    /// `shutdown_subtree` cancels the named root, its descendants, but
    /// MUST NOT cancel siblings that share the same parent. Negative
    /// assertion is the load-bearing one — false positive would silently
    /// kill innocent peers.
    #[tokio::test]
    async fn shutdown_subtree_cancels_descendants_only() {
        let mut c = ctrl();
        // Layout:  A (root) → B → C ;  A → D (sibling)
        let (cancel_a, sess_a) =
            c.insert_mock_handle_with_session("A", "atlas", "root-session", "sess-A");
        let (cancel_b, sess_b) = c.insert_mock_handle_with_session("B", "bee", &sess_a, "sess-B");
        let (cancel_c, _sess_c) = c.insert_mock_handle_with_session("C", "cee", &sess_b, "sess-C");
        let (cancel_d, _sess_d) = c.insert_mock_handle_with_session("D", "dee", &sess_a, "sess-D");

        let killed = c.shutdown_subtree("B").await.expect("subtree shutdown");
        // Leaves should shut down first; B is the root so it's last.
        assert_eq!(killed.last(), Some(&"B".to_string()));
        assert!(killed.contains(&"C".to_string()));

        assert!(
            cancel_b.is_cancelled(),
            "B (subtree root) must be cancelled"
        );
        assert!(cancel_c.is_cancelled(), "C (descendant) must be cancelled");
        assert!(
            !cancel_a.is_cancelled(),
            "A (ancestor of B) must NOT be cancelled"
        );
        assert!(
            !cancel_d.is_cancelled(),
            "D (sibling of B) must NOT be cancelled"
        );
    }

    /// Terminal agents reject pause/resume with the specific NotPausable
    /// variant — gRPC layer maps this to FailedPrecondition so operators
    /// see "agent is completed" rather than a silent success.
    #[tokio::test]
    async fn pause_on_terminal_agent_returns_not_pausable() {
        let mut c = ctrl();
        c.insert_mock_handle("done", "atlas");
        // Drive to terminal.
        if let Some(h) = c.agents.get_mut("done") {
            h.info.status = SubAgentStatus::Completed {
                result: "ok".into(),
            };
        }

        let err = c.set_paused("done", true).await.expect_err("must reject");
        match err {
            AgentControlError::NotPausable { id, status } => {
                assert_eq!(id, "done");
                assert_eq!(status, "completed");
            }
            other => panic!("expected NotPausable, got {other:?}"),
        }
    }

    /// Event bus emits monotonically increasing seq across kinds, and the
    /// last event's kind matches the final mutation. Encodes the gap-
    /// detection contract the TUI uses to decide whether to re-snapshot.
    #[tokio::test]
    async fn event_bus_seq_monotonic_across_mutations() {
        let bus = AgentTreeBus::new();
        let mut rx = bus.subscribe();
        let mut c = ctrl().with_bus(bus.clone());

        c.insert_mock_handle("x", "xenon");
        // Mock handle bypasses spawn_agent, so manually publish Spawned to
        // exercise the integration path the same way spawn_agent does.
        let info = c.get_status("x").unwrap().clone();
        bus.publish(AgentTreeEventKind::Spawned, &info);

        c.set_paused("x", true).await.unwrap();
        c.set_paused("x", false).await.unwrap();
        c.shutdown_agent("x").await.unwrap();

        // Collect all events.
        let mut events = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(e) => events.push(e),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => break,
                Err(TryRecvError::Lagged(_)) => panic!("unexpected lag in tiny test"),
            }
        }
        assert!(
            events.len() >= 4,
            "expected at least 4 events, got {}",
            events.len()
        );
        for win in events.windows(2) {
            assert!(
                win[1].seq > win[0].seq,
                "seq must be strictly monotonic: {} → {}",
                win[0].seq,
                win[1].seq
            );
        }
        // Last mutation was a shutdown → expect Killed kind.
        assert_eq!(events.last().unwrap().kind, AgentTreeEventKind::Killed);
    }

    /// aggregate_subtree sums metrics across descendants without double-
    /// counting the root's own metrics. Tests both inclusion (root + child)
    /// and exclusion (sibling subtree must not contribute).
    #[tokio::test]
    async fn aggregate_subtree_sums_descendants_only() {
        let mut c = ctrl();
        let (_, sess_a) = c.insert_mock_handle_with_session("A", "a", "root-session", "sess-A");
        let (_, sess_b) = c.insert_mock_handle_with_session("B", "b", &sess_a, "sess-B");
        let (_, _sess_c) = c.insert_mock_handle_with_session("C", "c", &sess_b, "sess-C");
        let (_, _sess_d) = c.insert_mock_handle_with_session("D", "d", "root-session", "sess-D");

        c.set_metrics(
            "A",
            SubAgentMetrics {
                input_tokens: 10,
                cost_usd: 0.10,
                ..Default::default()
            },
        );
        c.set_metrics(
            "B",
            SubAgentMetrics {
                input_tokens: 20,
                cost_usd: 0.20,
                ..Default::default()
            },
        );
        c.set_metrics(
            "C",
            SubAgentMetrics {
                input_tokens: 30,
                cost_usd: 0.30,
                ..Default::default()
            },
        );
        c.set_metrics(
            "D",
            SubAgentMetrics {
                input_tokens: 999,
                cost_usd: 9.99,
                ..Default::default()
            },
        );

        let from_a = c.aggregate_subtree("A").expect("A present");
        assert_eq!(from_a.input_tokens, 60, "A + B + C should be 10 + 20 + 30");
        assert!(
            (from_a.cost_usd - 0.60).abs() < 1e-6,
            "cost should sum to 0.60, got {}",
            from_a.cost_usd
        );

        let from_b = c.aggregate_subtree("B").expect("B present");
        assert_eq!(from_b.input_tokens, 50, "B + C should be 20 + 30");

        let from_c = c.aggregate_subtree("C").expect("C present");
        assert_eq!(from_c.input_tokens, 30, "C leaf — just itself");

        // The `== 60` assertion above already proves exclusion: any bleed
        // from D (999 tokens) would push the sum past 60. The negative
        // case is encoded by the exact-match positive, not a weaker
        // `< 999` check that would pass even on a buggy 998.
    }
}

/// Tests for the sub-agent caller surface: `context` / `toolsets` /
/// `role_kind` schema, orchestrator gating, kill switch, the dedicated
/// preamble builder, file-write tracking, heartbeat config plumbing, and
/// the auto-approve / delegation-model precedence chain in
/// `build_sub_agent_config`. Each test pins a behavior the caller surface
/// promises — none assert on `Default`/`Clone` round-trips.
#[cfg(test)]
mod sub_agent_surface_tests {
    use super::*;
    use crate::config::{Config, MultiAgentConfig};

    fn mk_role(name: &str, tools_allowed: Option<Vec<&str>>) -> AgentRole {
        AgentRole {
            name: name.into(),
            description: "test role".into(),
            model: None,
            provider: None,
            temperature: None,
            system_instructions: None,
            tools_allowed: tools_allowed.map(|v| v.into_iter().map(str::to_string).collect()),
            max_iterations: None,
        }
    }

    /// `<context trust="parent">…</context>` and `<goal>…</goal>` must
    /// land in the preamble verbatim. Downstream prompts and tests grep
    /// for those block delimiters. If a refactor silently drops `context`
    /// between handler and run, the child has no way to know background
    /// facts the parent wanted to share.
    #[test]
    fn preamble_renders_context_and_goal_blocks() {
        let preamble = build_subagent_preamble(
            "atlas",
            "id-1",
            None,
            AgentRoleKind::Leaf,
            "do the thing",
            Some("the user prefers tabs"),
            0,
            3,
        );
        assert!(
            preamble.contains("<context trust=\"parent\">\n"),
            "preamble must open a trust-tagged context block, got:\n{preamble}"
        );
        assert!(
            preamble.contains("the user prefers tabs\n</context>"),
            "preamble must carry the parent context verbatim, got:\n{preamble}"
        );
        assert!(
            preamble.contains("Point-in-time snapshot from when you were spawned"),
            "context block must carry the freshness frame so the child re-verifies stale handoffs, got:\n{preamble}"
        );
        assert!(
            preamble.contains("<goal>\ndo the thing\n</goal>"),
            "preamble must wrap goal in <goal> block, got:\n{preamble}"
        );
    }

    /// Leaf preamble must explicitly tell the child not to delegate;
    /// orchestrator preamble must say it may. Without this signal the child
    /// guesses from its (silent) tool surface — an LLM-visible miscue.
    #[test]
    fn preamble_signals_delegation_capability() {
        let leaf =
            build_subagent_preamble("atlas", "id", None, AgentRoleKind::Leaf, "g", None, 0, 3);
        assert!(
            leaf.contains("leaf — do not attempt to delegate"),
            "leaf preamble must forbid delegation, got:\n{leaf}"
        );

        let orch = build_subagent_preamble(
            "atlas",
            "id",
            None,
            AgentRoleKind::Orchestrator,
            "g",
            None,
            0,
            3,
        );
        assert!(
            orch.contains("may delegate further via spawn_agent"),
            "orchestrator under cap must signal delegation, got:\n{orch}"
        );

        let orch_capped = build_subagent_preamble(
            "atlas",
            "id",
            None,
            AgentRoleKind::Orchestrator,
            "g",
            None,
            2,
            3,
        );
        assert!(
            orch_capped.contains("depth cap reached"),
            "orchestrator AT cap must say cap reached, got:\n{orch_capped}"
        );
    }

    /// Toolsets narrow the parent's surface. A `toolsets: ["a", "b"]`
    /// where "b" is not in the parent's enabled set must NOT inject "b"
    /// into the child — toolsets is an intersection, not an additive
    /// allowlist. Catches a regression where a future refactor flips the
    /// semantic and lets the LLM grant itself tools.
    #[tokio::test]
    async fn toolsets_intersects_with_parent_surface_not_adds() {
        let parent = Config::default();
        let filter = compute_tools_filter(
            None,
            Some(&["read_file".into(), "definitely_fake_tool".into()]),
            None,
            &parent,
        )
        .await;
        let allowed = filter.expect("intersection should yield a filter");
        assert!(
            allowed.iter().any(|t| t == "read_file"),
            "read_file must survive intersection: {allowed:?}"
        );
        assert!(
            !allowed.iter().any(|t| t == "definitely_fake_tool"),
            "names not in parent surface must NOT be injected: {allowed:?}"
        );
    }

    /// Toolsets applied on top of a role allowlist must produce the
    /// intersection. A role that allows `[read_file, write_memory]` plus
    /// `toolsets: [write_memory, run_shell]` must yield exactly
    /// `[write_memory]`. The restrictive semantic — neither side adds
    /// tools the other didn't already permit — keeps the child bound by
    /// the role contract while letting per-call narrowing happen.
    #[tokio::test]
    async fn toolsets_intersect_with_role_allowlist() {
        let role = mk_role("r", Some(vec!["read_file", "write_memory"]));
        let parent = Config::default();
        let filter = compute_tools_filter(
            Some(&role),
            Some(&["write_memory".into(), "run_shell".into()]),
            None,
            &parent,
        )
        .await;
        let allowed = filter.expect("intersection result");
        assert_eq!(
            allowed,
            vec!["write_memory".to_string()],
            "expected pure intersection of role and toolsets, got {allowed:?}"
        );
    }

    /// Empty `toolsets: []` means "no tools" — a legitimate caller intent
    /// (e.g. "summarize this text"). The filter must distinguish this from
    /// `toolsets: None` (inherit parent).
    #[tokio::test]
    async fn empty_toolsets_yields_empty_filter_not_inherit() {
        let parent = Config::default();
        let with_none = compute_tools_filter(None, None, None, &parent).await;
        assert!(
            with_none.is_none(),
            "None toolsets must yield None (inherit), got {with_none:?}"
        );
        let with_empty = compute_tools_filter(None, Some(&[]), None, &parent).await;
        assert_eq!(
            with_empty,
            Some(Vec::<String>::new()),
            "Empty toolsets must yield Some(empty) (no tools), not None"
        );
    }

    /// `can_orchestrate` is the call-site predicate for whether the
    /// blocklist should re-permit spawn_agent. Both the kill switch
    /// (`orchestrator_enabled`) and the numeric depth cap must permit.
    #[test]
    fn can_orchestrate_requires_kill_switch_and_depth_under_cap() {
        let mut cfg = Config::default();
        cfg.agents.max_spawn_depth = 3;

        cfg.agents.orchestrator_enabled = true;
        assert!(
            super::tools::can_orchestrate(&cfg, 0),
            "depth 0 + enabled must permit"
        );
        assert!(
            super::tools::can_orchestrate(&cfg, 1),
            "depth 1 + enabled must permit (1+1 < 3)"
        );
        assert!(
            !super::tools::can_orchestrate(&cfg, 2),
            "depth 2 + enabled MUST NOT permit (2+1 == 3, not <)"
        );

        cfg.agents.orchestrator_enabled = false;
        assert!(
            !super::tools::can_orchestrate(&cfg, 0),
            "kill switch off must veto even at depth 0"
        );
    }

    /// Orchestrator blocklist re-permits spawn/wait/close ONLY when
    /// `can_orchestrate` is true. A leaf NEVER gets them, regardless of
    /// the kill switch. Catches a regression where leaf/orchestrator
    /// gating is wired only at the schema level and not at the blocklist.
    #[test]
    fn assemble_delegate_blocklist_re_permits_orchestrator_tools_iff_allowed() {
        let cfg = Config::default();

        // Leaf, can_orchestrate=true (doesn't matter — leaf is leaf)
        let leaf = super::tools::assemble_delegate_blocklist(&cfg, AgentRoleKind::Leaf, true);
        assert!(
            leaf.contains(&"spawn_agent"),
            "leaf must block spawn_agent regardless of can_orchestrate: {leaf:?}"
        );

        // Orchestrator, can_orchestrate=false (depth maxed OR kill switch off)
        let orch_blocked =
            super::tools::assemble_delegate_blocklist(&cfg, AgentRoleKind::Orchestrator, false);
        assert!(
            orch_blocked.contains(&"spawn_agent"),
            "orchestrator at cap must still be blocked: {orch_blocked:?}"
        );

        // Orchestrator, can_orchestrate=true
        let orch_ok =
            super::tools::assemble_delegate_blocklist(&cfg, AgentRoleKind::Orchestrator, true);
        assert!(
            !orch_ok.contains(&"spawn_agent"),
            "orchestrator under cap with switch on must re-permit spawn_agent: {orch_ok:?}"
        );
        assert!(
            !orch_ok.contains(&"wait_for_agent"),
            "orchestrator must also re-permit wait_for_agent: {orch_ok:?}"
        );
        assert!(
            !orch_ok.contains(&"close_agent"),
            "orchestrator must also re-permit close_agent: {orch_ok:?}"
        );
        // The non-delegation members of the blocklist must still be present.
        assert!(
            orch_ok.contains(&"write_memory"),
            "orchestrator does NOT re-permit memory mutation: {orch_ok:?}"
        );
        assert!(
            orch_ok.contains(&"request_user_input"),
            "orchestrator does NOT re-permit user input: {orch_ok:?}"
        );
    }

    /// Plan mode is a hard read-only guarantee — it unions every
    /// mutating tool into the child blocklist *after* the orchestrator
    /// overlay, so the carve-out can't smuggle delegation tools (which
    /// are themselves mutations) past the plan-mode contract. Both run_shell
    /// AND spawn_agent must be blocked when the parent is in Plan, even
    /// for an orchestrator role.
    #[test]
    fn plan_mode_blocks_all_mutations_including_delegation() {
        let mut cfg = Config::default();
        cfg.conversation.collaboration_mode = crate::config::CollaborationMode::Plan;

        let blocklist =
            super::tools::assemble_delegate_blocklist(&cfg, AgentRoleKind::Orchestrator, true);
        // Every mutating tool blocked — including the delegation surface,
        // even though an orchestrator role would otherwise re-permit it.
        for must_block in ["run_shell", "apply_patch", "spawn_agent", "write_memory"] {
            assert!(
                blocklist.contains(&must_block),
                "Plan mode must block '{must_block}' even for orchestrator: {blocklist:?}"
            );
        }
    }

    /// And the corollary: outside Plan mode, the orchestrator carve-out
    /// IS effective — `spawn_agent` is available to the child even though
    /// it'd be in the default leaf blocklist. This is the test that
    /// proves the overlay-then-union order has a useful effect.
    #[test]
    fn orchestrator_carve_out_works_outside_plan_mode() {
        let cfg = Config::default(); // Execute mode (default)
        let blocklist =
            super::tools::assemble_delegate_blocklist(&cfg, AgentRoleKind::Orchestrator, true);
        assert!(
            !blocklist.contains(&"spawn_agent"),
            "outside Plan mode, orchestrator must NOT have spawn_agent blocked: {blocklist:?}"
        );
        // run_shell wasn't in the default leaf blocklist either — it's
        // only added in Plan mode — so the orchestrator gets it too.
        assert!(
            !blocklist.contains(&"run_shell"),
            "run_shell not in default blocklist, orchestrator should have it: {blocklist:?}"
        );
    }

    /// Sub-agent auto_approve is unconditionally `["*"]` (borg's "No
    /// Approval Prompts" UX invariant — children have no UI to prompt
    /// against, so they must execute without confirmation). The parent's
    /// deny list always carries through; catastrophic commands stop at
    /// the sandbox layer. A previous build gated this on a config knob,
    /// but `ExecutionPolicy::check` (policy.rs) falls through to
    /// AutoApprove when nothing matches — the gate was a silent no-op.
    /// This test pins the actual contract so the misleading knob can't
    /// be reintroduced without flipping the assertion.
    #[test]
    fn build_sub_agent_config_auto_approve_is_unconditional_wildcard() {
        let mut parent = Config::default();
        parent.policy.deny = vec!["dangerous".into()];
        let cfg = MultiAgentConfig {
            enabled: true,
            max_spawn_depth: 2,
            ..MultiAgentConfig::default()
        };

        let ctrl = AgentControl::new(&cfg, "p", 0);
        let child = ctrl.build_sub_agent_config(None, None, &parent);
        assert_eq!(
            child.policy.auto_approve,
            vec!["*".to_string()],
            "auto_approve must be wildcard — borg children have no UI to prompt against"
        );
        assert_eq!(
            child.policy.deny, parent.policy.deny,
            "parent deny list must always carry through"
        );
    }

    /// Model precedence (highest first): explicit `model_override` arg,
    /// then `role.model`, then `agents.delegation_model`, then the parent's
    /// model. A regression that reorders these silently routes the child
    /// to the wrong provider — caught by exact-string assertions at each
    /// layer.
    #[test]
    fn build_sub_agent_config_model_precedence_chain() {
        let mut parent = Config::default();
        parent.llm.model = "parent-model".into();
        parent.agents.delegation_model = Some("delegation-default".into());
        let role_with_model = AgentRole {
            model: Some("role-model".into()),
            ..mk_role("r", None)
        };
        let ctrl = AgentControl::new(&MultiAgentConfig::default(), "p", 0);

        // No role, no override → delegation_model takes over.
        let c1 = ctrl.build_sub_agent_config(None, None, &parent);
        assert_eq!(c1.llm.model, "delegation-default");

        // Role with model → role wins over delegation_model.
        let c2 = ctrl.build_sub_agent_config(Some(&role_with_model), None, &parent);
        assert_eq!(c2.llm.model, "role-model");

        // Explicit override → wins over both.
        let c3 =
            ctrl.build_sub_agent_config(Some(&role_with_model), Some("explicit-model"), &parent);
        assert_eq!(c3.llm.model, "explicit-model");

        // No role, no override, no delegation_model → parent's model.
        let mut parent_no_delegation = parent.clone();
        parent_no_delegation.agents.delegation_model = None;
        let c4 = ctrl.build_sub_agent_config(None, None, &parent_no_delegation);
        assert_eq!(c4.llm.model, "parent-model");
    }

    /// `extract_apply_patch_paths` is what populates `files_written` from
    /// observed tool calls. Every patch DSL header type must produce a
    /// path; non-header lines must NOT (a regression here would surface
    /// bogus paths to the parent).
    #[test]
    fn extract_apply_patch_paths_parses_all_three_headers() {
        let patch = r#"
*** Begin Patch
*** Add File: src/new.rs
+pub fn foo() {}
*** Update File: src/existing.rs
@@
 context
-old
+new
*** Delete File: src/dead.rs
*** End Patch
"#;
        let paths = extract_apply_patch_paths(patch);
        assert_eq!(
            paths,
            vec![
                "src/new.rs".to_string(),
                "src/existing.rs".to_string(),
                "src/dead.rs".to_string(),
            ]
        );
    }

    /// The handler wraps `apply_patch` args as JSON `{"patch": "..."}`.
    /// The extractor must unwrap that without false-positives on the JSON
    /// punctuation.
    #[test]
    fn extract_apply_patch_paths_unwraps_json_args() {
        let json_args = r#"{"patch": "*** Update File: a/b.rs\n@@\n context\n-old\n+new\n"}"#;
        let paths = extract_apply_patch_paths(json_args);
        assert_eq!(paths, vec!["a/b.rs".to_string()]);
    }

    /// Fresh `session_id` per spawn — the CLAUDE.md:149 invariant. Two
    /// consecutive `prepare_sub_agent_info` calls must produce distinct
    /// UUIDs *and* neither must equal the parent's session id.
    #[tokio::test]
    async fn prepare_sub_agent_info_assigns_distinct_session_ids() {
        let cfg = MultiAgentConfig::default();
        let ctrl = AgentControl::new(&cfg, "parent-session-xyz", 0);
        let s1 = uuid::Uuid::new_v4().to_string();
        let s2 = uuid::Uuid::new_v4().to_string();
        let info1 = ctrl
            .prepare_sub_agent_info("id-1", "atlas", "default", "m", &s1, 1)
            .await;
        let info2 = ctrl
            .prepare_sub_agent_info("id-2", "aurora", "default", "m", &s2, 1)
            .await;
        assert_ne!(info1.session_id, info2.session_id);
        assert_ne!(info1.session_id, "parent-session-xyz");
        assert_ne!(info2.session_id, "parent-session-xyz");
    }

    /// `Spawned`, `Heartbeat`, and `Stalled` must each have unique wire
    /// strings — these go into the gRPC stream and the TUI keys events by
    /// `kind.as_str()`. A duplicate string would silently merge two
    /// distinct event kinds in the UI.
    #[test]
    fn agent_tree_event_kind_strings_are_distinct() {
        use std::collections::HashSet;
        let all = [
            AgentTreeEventKind::Spawned,
            AgentTreeEventKind::StatusChanged,
            AgentTreeEventKind::Paused,
            AgentTreeEventKind::Resumed,
            AgentTreeEventKind::Completed,
            AgentTreeEventKind::Errored,
            AgentTreeEventKind::Killed,
            AgentTreeEventKind::Heartbeat,
            AgentTreeEventKind::Stalled,
        ];
        let strings: HashSet<&str> = all.iter().map(|k| k.as_str()).collect();
        assert_eq!(
            strings.len(),
            all.len(),
            "every AgentTreeEventKind must have a unique wire string"
        );
        // The two new ones must actually be reachable through as_str.
        assert!(strings.contains("heartbeat"));
        assert!(strings.contains("stalled"));
    }

    /// `AgentRoleKind::parse` accepts the documented literals and falls
    /// back to Leaf for everything else. The fallback is intentional:
    /// an LLM that types "leaf-only" or a typo'd "orchestratorr" should
    /// degrade safely rather than reject the spawn.
    #[test]
    fn agent_role_kind_parse_known_and_unknown() {
        assert_eq!(AgentRoleKind::parse("leaf"), AgentRoleKind::Leaf);
        assert_eq!(
            AgentRoleKind::parse("orchestrator"),
            AgentRoleKind::Orchestrator
        );
        assert_eq!(
            AgentRoleKind::parse("ORCHESTRATOR"),
            AgentRoleKind::Orchestrator,
            "parse must be case-insensitive"
        );
        assert_eq!(
            AgentRoleKind::parse("  orchestrate  "),
            AgentRoleKind::Orchestrator,
            "parse must trim and accept 'orchestrate' alias"
        );
        // Garbage falls back to Leaf — safer default.
        assert_eq!(AgentRoleKind::parse("bogus"), AgentRoleKind::Leaf);
        assert_eq!(AgentRoleKind::parse(""), AgentRoleKind::Leaf);
    }

    /// The subagent-driven-development skill must be registered. Pure
    /// registration check — the SKILL.md body is markdown for the LLM
    /// and not load-bearing here.
    #[test]
    fn subagent_driven_development_skill_is_registered() {
        assert!(
            crate::skills::is_builtin_skill_name("subagent-driven-development"),
            "the SDD skill must appear in the bundled-skills registry"
        );
    }

    /// REGRESSION: `is_apply_patch_success` is an ALLOWLIST against the
    /// success-prefix shape `format_patch_success` emits, NOT a denylist
    /// against known failure prefixes. A previous denylist version missed
    /// the `"Unknown target: …"` return from `handle_apply_patch_unified`
    /// — credit went to `files_written` for a patch that touched nothing.
    /// This test pins the allowlist semantics so a future denylist
    /// regression (or a new failure prefix) fails fast.
    #[test]
    fn is_apply_patch_success_uses_allowlist() {
        // Success cases — `format_patch_success` always emits the
        // canonical "applied successfully" anchor.
        assert!(is_apply_patch_success(
            "Patch applied successfully.\n--- diff ---"
        ));
        assert!(is_apply_patch_success(
            "Unified patch applied successfully."
        ));
        assert!(is_apply_patch_success("Skill patch applied successfully."));

        // Every failure sentinel must NOT be counted as success. The
        // "Unknown target" case is the one the original denylist missed
        // — listed first to make the regression intent obvious.
        for failure in [
            "Unknown target: workspaze. Use: cwd, skills, channels.",
            "Error applying patch: hunk did not apply",
            "Error applying unified patch: bad context",
            "Access denied: /etc/passwd is in a blocked path",
            "Plan mode is active — apply_patch refused",
            "Rate limit reached for apply_patch",
            "Some other unforeseen failure mode without a known prefix",
            "",    // empty is not success
            "   ", // whitespace-only is not success
        ] {
            assert!(
                !is_apply_patch_success(failure),
                "must reject failure string: {failure:?}"
            );
        }
    }

    /// REGRESSION: `extract_apply_patch_paths` must resolve relative
    /// patch paths against the apply_patch_unified `target` so the
    /// parent gets paths it can actually re-read. A previous version
    /// returned bare strings, so `target: "skills"` writes surfaced
    /// `my_skill/SKILL.md` instead of `~/.borg/skills/my_skill/SKILL.md`.
    #[test]
    fn extract_apply_patch_paths_resolves_skills_target_base() {
        let args = serde_json::json!({
            "target": "skills",
            "patch": "*** Begin Patch\n*** Update File: my_skill/SKILL.md\n@@\n a\n-b\n+c\n*** End Patch"
        })
        .to_string();
        let paths = extract_apply_patch_paths(&args);
        assert_eq!(paths.len(), 1);
        // The skills dir resolves via Config::skills_dir(); whatever its
        // root, the suffix must be the patch-marker path joined to it.
        let skills_dir = Config::skills_dir().expect("skills_dir must resolve in tests");
        let expected = skills_dir
            .join("my_skill/SKILL.md")
            .to_string_lossy()
            .into_owned();
        assert_eq!(paths[0], expected);
    }

    /// REGRESSION: target=cwd (or omitted) must still yield bare relative
    /// paths — operators reading the parent's `files_written` expect the
    /// same shape they passed to apply_patch.
    #[test]
    fn extract_apply_patch_paths_cwd_target_returns_bare_paths() {
        let args = serde_json::json!({
            "patch": "*** Update File: src/foo.rs\n*** Add File: src/bar.rs"
        })
        .to_string();
        let paths = extract_apply_patch_paths(&args);
        assert_eq!(
            paths,
            vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()]
        );

        // Explicit target:"cwd" same as omitted.
        let args_cwd = serde_json::json!({
            "target": "cwd",
            "patch": "*** Update File: a.rs"
        })
        .to_string();
        assert_eq!(
            extract_apply_patch_paths(&args_cwd),
            vec!["a.rs".to_string()]
        );
    }

    /// REGRESSION: a second `wait_for_agent` against a child whose
    /// completion was already drained must STILL return the correct
    /// `files_written`. The early-return branch reads from
    /// `handle.info.metrics.files_touched`; a previous version never
    /// populated that field, so subsequent waiters got `vec![]` even
    /// though the child mutated files. The fix persists files_written
    /// from completion into the handle's metrics on receipt.
    #[tokio::test]
    async fn second_wait_for_agent_returns_persisted_files_written() {
        let cfg = MultiAgentConfig::default();
        let mut ctrl = AgentControl::new(&cfg, "p", 0);
        ctrl.insert_mock_handle("agent-x", "atlas");

        let tx = ctrl.test_completion_tx();
        tx.send(SubAgentCompletion {
            agent_id: "agent-x".to_string(),
            nickname: "atlas".to_string(),
            status: SubAgentStatus::Completed {
                result: "done".to_string(),
            },
            final_response: Some("done".to_string()),
            files_written: vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()],
        })
        .await
        .unwrap();

        let first = ctrl.wait_for_agent("agent-x", 5).await.unwrap();
        assert_eq!(
            first.files_written,
            vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()]
        );

        // Handle is now in Completed state — second call hits the
        // early-return branch. Must surface the same paths.
        let second = ctrl.wait_for_agent("agent-x", 5).await.unwrap();
        assert_eq!(
            second.files_written,
            vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()],
            "second wait_for_agent must read the persisted files_written, \
             not an empty Vec from a never-populated metrics field"
        );
    }

    /// REGRESSION: in a mixed batch, the per-task blocklist must be
    /// computed from THAT task's role_kind — a leaf sibling of an
    /// orchestrator must NOT inherit the orchestrator's spawn_agent
    /// carve-out. Tests the post-review fix to `spawn_batch_and_wait`.
    /// Pre-fix, both tasks shared a single blocklist computed from
    /// `any_orchestrator`, opening the gating bypass.
    #[test]
    fn batch_per_task_blocklist_does_not_promote_leaf_siblings() {
        let mut cfg = Config::default();
        cfg.agents.max_spawn_depth = 3;
        cfg.agents.orchestrator_enabled = true;

        // Simulate what spawn_batch_and_wait computes for each task.
        let depth: u32 = 0;
        let can = super::tools::can_orchestrate(&cfg, depth);
        assert!(can, "test premise: depth 0 + enabled should permit");

        let orch_blocklist =
            super::tools::assemble_delegate_blocklist(&cfg, AgentRoleKind::Orchestrator, can);
        let leaf_blocklist =
            super::tools::assemble_delegate_blocklist(&cfg, AgentRoleKind::Leaf, can);

        assert!(
            !orch_blocklist.contains(&"spawn_agent"),
            "orchestrator task: spawn_agent re-permitted"
        );
        assert!(
            leaf_blocklist.contains(&"spawn_agent"),
            "leaf task in the SAME batch: spawn_agent still blocked, regardless of sibling"
        );
    }
}
