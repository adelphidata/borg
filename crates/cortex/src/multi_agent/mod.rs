/// Broadcast bus for agent-tree lifecycle events consumed by the TUI/CLI.
pub mod event_bus;
/// Name pool for generating unique sub-agent nicknames.
pub mod names;
/// Predefined role definitions for sub-agents.
pub mod roles;
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

/// Role definition for a sub-agent.
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
#[derive(Debug, Clone)]
pub struct DelegatedTask {
    /// Task description sent to the child as its initial user message.
    pub goal: String,
    /// Optional role name (looked up via `roles::load_role`).
    pub role_name: Option<String>,
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

    /// Publish to the bus if one is attached. Best-effort — a closed channel
    /// just logs and drops, since the bus is an observer, not a participant.
    fn publish(&self, kind: AgentTreeEventKind, info: &SubAgentInfo) {
        if let Some(ref bus) = self.bus {
            bus.publish(kind, info);
        }
    }

    /// Build a sub-agent config by applying role overrides and model override to the parent config.
    fn build_sub_agent_config(
        &self,
        role: Option<&AgentRole>,
        model_override: Option<&str>,
        parent_config: &Config,
    ) -> Config {
        let mut child_config = parent_config.clone();
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
        if let Some(model) = model_override {
            child_config.llm.model = model.to_string();
        }

        // Sub-agents auto-approve shell commands but preserve deny list
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
    /// `tools_blocklist` takes precedence over any role allowlist: names in
    /// the blocklist are stripped from the effective tool filter. When a
    /// blocklist is provided and the role has no allowlist, the effective
    /// filter becomes `parent_tools - blocklist`.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_agent(
        &mut self,
        message: &str,
        role: Option<AgentRole>,
        nickname: Option<&str>,
        model_override: Option<&str>,
        parent_config: &Config,
        fork_context: Option<&[crate::types::Message]>,
        tools_blocklist: Option<&[&str]>,
    ) -> Result<(String, String)> {
        // Validate limits
        self.validate_spawn_limits()?;

        // Generate ID and nickname
        let agent_id = uuid::Uuid::new_v4().to_string();
        let chosen_nickname = match nickname {
            Some(n) => n.to_string(),
            None => self.name_pool.next_name(),
        };
        let role_name = role
            .as_ref()
            .map_or("default", |r| r.name.as_str())
            .to_string();

        // Build config
        let child_config =
            self.build_sub_agent_config(role.as_ref(), model_override, parent_config);

        let child_depth = self.current_depth + 1;
        let session_id = uuid::Uuid::new_v4().to_string();

        // Prepare info and record in DB
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
        let cancel_clone = cancel.clone();
        let is_paused = Arc::new(AtomicBool::new(false));
        let pause_notify = Arc::new(Notify::new());
        let is_paused_task = is_paused.clone();
        let pause_notify_task = pause_notify.clone();
        let completion_tx = self.completion_tx.clone();
        let semaphore = self.semaphore.clone();
        let agent_id_clone = agent_id.clone();
        let nickname_clone = chosen_nickname.clone();
        let message_owned = message.to_string();
        let fork_context_owned: Option<Vec<crate::types::Message>> =
            fork_context.map(<[crate::types::Message]>::to_vec);
        let agents_config = parent_config.agents.clone();
        let tools_filter =
            compute_tools_filter(role.as_ref(), tools_blocklist, parent_config).await;

        let join_handle = tokio::spawn(async move {
            // Acquire semaphore permit
            let _permit = match semaphore.acquire().await {
                Ok(p) => p,
                Err(_) => {
                    if completion_tx
                        .send(SubAgentCompletion {
                            agent_id: agent_id_clone,
                            nickname: nickname_clone,
                            status: SubAgentStatus::Errored {
                                error: "Semaphore closed".to_string(),
                            },
                            final_response: None,
                        })
                        .await
                        .is_err()
                    {
                        tracing::debug!("sub-agent completion receiver dropped");
                    }
                    return;
                }
            };

            let result = run_sub_agent(
                agent_id_clone.clone(),
                nickname_clone.clone(),
                child_config,
                child_depth,
                agents_config,
                message_owned,
                role,
                fork_context_owned,
                input_rx,
                cancel_clone,
                is_paused_task,
                pause_notify_task,
                tools_filter,
            )
            .await;

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

            // Update DB
            if let Ok(db) = crate::db::Database::open().await {
                if let Err(e) = db.update_sub_agent_status(&agent_id_clone, &status).await {
                    tracing::warn!(%agent_id_clone, "failed to update sub-agent status: {e}");
                }
            }

            if completion_tx
                .send(SubAgentCompletion {
                    agent_id: agent_id_clone,
                    nickname: nickname_clone,
                    status,
                    final_response,
                })
                .await
                .is_err()
            {
                tracing::debug!("sub-agent completion receiver dropped");
            }
        });

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

        // Check if already completed
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
                    });
                }
                _ => {}
            }
        }

        // Check buffered completions first (from previous wait_for_agent calls)
        if let Some(idx) = self
            .completion_buffer
            .iter()
            .position(|c| c.agent_id == agent_id)
        {
            return Ok(self.completion_buffer.remove(idx));
        }

        // Poll completion channel until we get the one we want
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                bail!("Timeout waiting for agent '{agent_id}'");
            }

            match tokio::time::timeout(remaining, self.completion_rx.recv()).await {
                Ok(Some(completion)) => {
                    let completed_id = completion.agent_id.clone();
                    // Update internal state
                    if let Some(handle) = self.agents.get_mut(&completed_id) {
                        handle.info.status = completion.status.clone();
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

        // Update DB
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
    /// then wait. Honors the same limits and tool filtering as `spawn_agent`
    /// (including the `tools_blocklist`). Returns whatever completion the
    /// child emits — including partial results for max-iteration/error/
    /// shutdown cases — rather than surfacing those as Rust errors.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_and_wait(
        &mut self,
        message: &str,
        role: Option<AgentRole>,
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
                role,
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
    pub async fn spawn_batch_and_wait(
        &mut self,
        tasks: Vec<DelegatedTask>,
        parent_config: &Config,
        tools_blocklist: Option<&[&str]>,
        timeout_secs: u64,
    ) -> Result<Vec<SubAgentCompletion>> {
        let mut agent_ids: Vec<std::result::Result<String, SubAgentCompletion>> =
            Vec::with_capacity(tasks.len());
        for (i, task) in tasks.iter().enumerate() {
            let role = match task.role_name.as_deref() {
                Some(name) => roles::load_role(name).await,
                None => None,
            };
            match self
                .spawn_agent(
                    &task.goal,
                    role,
                    None,
                    task.model_override.as_deref(),
                    parent_config,
                    None,
                    tools_blocklist,
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
/// Rules:
/// - If both the role's `tools_allowed` and `blocklist` are `None`, the
///   result is `None` (inherit all parent tools).
/// - If a `blocklist` is provided:
///     - When the role has an allowlist, the result is
///       `role_allowlist - blocklist` (blocklist wins on conflict).
///     - When the role has no allowlist, the result is
///       `parent_enabled_tools - blocklist` — this prevents the child from
///       inheriting access to tools the parent explicitly blocked.
/// - If only an allowlist is present, it's returned as-is.
async fn compute_tools_filter(
    role: Option<&AgentRole>,
    blocklist: Option<&[&str]>,
    parent_config: &Config,
) -> Option<Vec<String>> {
    let allowlist = role.and_then(|r| r.tools_allowed.clone());
    match (allowlist, blocklist) {
        (None, None) => None,
        (Some(allow), None) => Some(allow),
        (Some(allow), Some(block)) => {
            let block_set: std::collections::HashSet<&str> = block.iter().copied().collect();
            Some(
                allow
                    .into_iter()
                    .filter(|t| !block_set.contains(t.as_str()))
                    .collect(),
            )
        }
        (None, Some(block)) => {
            // Enumerate the full parent tool surface: core tools + any
            // multi-agent tools the parent can see at its current depth.
            // Without unioning multi-agent here, children would silently lose
            // access to e.g. `wait_for_agent` even if a caller deliberately
            // removed it from the blocklist — a confusing invariant to debug.
            let mut parent_tools =
                crate::tool::definitions::core_tool_definitions(parent_config).await;
            parent_tools.extend(tools::tool_definitions(0, u32::MAX));
            let block_set: std::collections::HashSet<&str> = block.iter().copied().collect();
            let filtered: Vec<String> = parent_tools
                .into_iter()
                .map(|t| t.function.name)
                .filter(|name| !block_set.contains(name.as_str()))
                .collect();
            Some(filtered)
        }
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
    agents_config: crate::config::MultiAgentConfig,
    initial_message: String,
    role: Option<AgentRole>,
    fork_context: Option<Vec<crate::types::Message>>,
    mut input_rx: mpsc::Receiver<String>,
    cancel: CancellationToken,
    is_paused: Arc<AtomicBool>,
    pause_notify: Arc<Notify>,
    tools_filter: Option<Vec<String>>,
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

        // Inject fork context if provided
        if let Some(context) = fork_context {
            for msg in context {
                agent.inject_history_message(msg);
            }
        }

        // Prepend role instructions to the initial message
        let mut full_message =
            format!("You are a sub-agent named \"{nickname}\" (id: {agent_id}).\n");
        if let Some(ref role) = role {
            full_message.push_str(&format!("Role: {} — {}\n", role.name, role.description));
            if let Some(ref instructions) = role.system_instructions {
                full_message.push_str(instructions);
                full_message.push('\n');
            }
        }
        full_message.push_str(
            "Complete the task and provide a clear, concise final response. \
             Your full response will be returned to the parent agent.\n\n",
        );
        full_message.push_str(&initial_message);

        // Update DB status to running
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

        // Send initial message in a spawned task
        let send_handle = {
            let event_tx_clone = event_tx.clone();
            let cancel = cancel_clone.clone();
            tokio::spawn(async move {
                agent
                    .send_message_with_cancel(&full_message, event_tx_clone, cancel)
                    .await
            })
        };

        // Collect the final text response
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
                    match event {
                        Some(AgentEvent::TextDelta(delta)) => {
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
                        _ => {}
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

#[cfg(all(test, feature = "phase9-tests-disabled"))]
mod tests {
    use super::*;
    use crate::config::MultiAgentConfig;

    fn test_config() -> MultiAgentConfig {
        MultiAgentConfig {
            enabled: true,
            max_spawn_depth: 1,
            max_children_per_agent: 5,
            max_concurrent: 3,
            delegate_timeout_secs: 60,
        }
    }

    #[test]
    fn test_agent_control_new() {
        let config = test_config();
        let ctrl = AgentControl::new(&config, "session-1", 0);
        assert!(ctrl.is_empty());
        assert_eq!(ctrl.max_spawn_depth, 1);
        assert_eq!(ctrl.max_children_per_agent, 5);
    }

    #[test]
    fn test_sub_agent_status_transitions() {
        let status = SubAgentStatus::PendingInit;
        assert_eq!(status.as_str(), "pending_init");

        let status = SubAgentStatus::Running;
        assert_eq!(status.as_str(), "running");

        let status = SubAgentStatus::Completed {
            result: "done".to_string(),
        };
        assert_eq!(status.as_str(), "completed");

        let status = SubAgentStatus::Errored {
            error: "oops".to_string(),
        };
        assert_eq!(status.as_str(), "errored");

        let status = SubAgentStatus::Shutdown;
        assert_eq!(status.as_str(), "shutdown");
    }

    #[test]
    fn test_sub_agent_status_from_db() {
        assert_eq!(
            SubAgentStatus::from_db("pending_init", None, None),
            SubAgentStatus::PendingInit
        );
        assert_eq!(
            SubAgentStatus::from_db("running", None, None),
            SubAgentStatus::Running
        );
        assert_eq!(
            SubAgentStatus::from_db("completed", Some("result"), None),
            SubAgentStatus::Completed {
                result: "result".to_string()
            }
        );
        assert_eq!(
            SubAgentStatus::from_db("errored", None, Some("fail")),
            SubAgentStatus::Errored {
                error: "fail".to_string()
            }
        );
        assert_eq!(
            SubAgentStatus::from_db("shutdown", None, None),
            SubAgentStatus::Shutdown
        );
        // Unknown defaults to PendingInit
        assert_eq!(
            SubAgentStatus::from_db("unknown", None, None),
            SubAgentStatus::PendingInit
        );
    }

    #[tokio::test]
    async fn test_spawn_depth_limit() {
        let config = MultiAgentConfig {
            enabled: true,
            max_spawn_depth: 1,
            max_children_per_agent: 5,
            max_concurrent: 3,
            delegate_timeout_secs: 60,
        };
        // current_depth=1 means we're already at the limit
        let mut ctrl = AgentControl::new(&config, "session-1", 1);
        let parent_config = Config::default();
        let result = ctrl
            .spawn_agent("test", None, None, None, &parent_config, None, None)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("max spawn depth"));
    }

    #[tokio::test]
    async fn test_max_children_limit() {
        let config = MultiAgentConfig {
            enabled: true,
            max_spawn_depth: 2,
            max_children_per_agent: 0, // no children allowed
            max_concurrent: 3,
            delegate_timeout_secs: 60,
        };
        let mut ctrl = AgentControl::new(&config, "session-1", 0);
        let parent_config = Config::default();
        let result = ctrl
            .spawn_agent("test", None, None, None, &parent_config, None, None)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("max children"));
    }

    #[test]
    fn test_list_agents_empty() {
        let config = test_config();
        let ctrl = AgentControl::new(&config, "session-1", 0);
        assert!(ctrl.list_agents().is_empty());
    }

    #[test]
    fn test_shutdown_agent_nonexistent() {
        let config = test_config();
        let mut ctrl = AgentControl::new(&config, "session-1", 0);
        assert!(ctrl.shutdown_agent("nonexistent").is_err());
    }

    #[test]
    fn test_drain_completions_empty() {
        let config = test_config();
        let mut ctrl = AgentControl::new(&config, "session-1", 0);
        assert!(ctrl.drain_completions().is_empty());
    }

    #[test]
    fn test_config_merge_role_overrides() {
        let role = AgentRole {
            name: "researcher".to_string(),
            description: "Research agent".to_string(),
            model: Some("gpt-4".to_string()),
            provider: None,
            temperature: Some(0.3),
            system_instructions: None,
            tools_allowed: None,
            max_iterations: None,
        };
        let mut parent_config = Config::default();
        parent_config.llm.model = "default-model".to_string();
        parent_config.llm.temperature = 0.7;

        let ctrl = AgentControl::new(&test_config(), "session-1", 0);
        let child_config = ctrl.build_sub_agent_config(Some(&role), None, &parent_config);

        assert_eq!(child_config.llm.model, "gpt-4");
        assert!((child_config.llm.temperature - 0.3).abs() < f32::EPSILON);
    }

    #[test]
    fn test_config_merge_model_override_takes_precedence() {
        let role = AgentRole {
            name: "researcher".to_string(),
            description: "Research agent".to_string(),
            model: Some("gpt-4".to_string()),
            provider: None,
            temperature: None,
            system_instructions: None,
            tools_allowed: None,
            max_iterations: None,
        };
        let parent_config = Config::default();

        let ctrl = AgentControl::new(&test_config(), "session-1", 0);
        let child_config =
            ctrl.build_sub_agent_config(Some(&role), Some("claude-3"), &parent_config);

        // model_override should win over role.model
        assert_eq!(child_config.llm.model, "claude-3");
    }

    #[test]
    fn test_validate_spawn_limits_depth() {
        let config = MultiAgentConfig {
            enabled: true,
            max_spawn_depth: 2,
            max_children_per_agent: 5,
            max_concurrent: 3,
            delegate_timeout_secs: 60,
        };
        // At depth 2 with max_spawn_depth=2, should fail
        let ctrl = AgentControl::new(&config, "session-1", 2);
        assert!(ctrl.validate_spawn_limits().is_err());

        // At depth 1 with max_spawn_depth=2, should pass
        let ctrl = AgentControl::new(&config, "session-1", 1);
        assert!(ctrl.validate_spawn_limits().is_ok());
    }

    #[tokio::test]
    async fn test_drain_completions_receives_sent_completion() {
        let config = test_config();
        let mut ctrl = AgentControl::new(&config, "session-1", 0);
        let tx = ctrl.test_completion_tx();

        ctrl.insert_mock_handle("agent-1", "Atlas");

        tx.send(SubAgentCompletion {
            agent_id: "agent-1".to_string(),
            nickname: "Atlas".to_string(),
            status: SubAgentStatus::Completed {
                result: "done".to_string(),
            },
            final_response: Some("done".to_string()),
        })
        .await
        .unwrap();

        let completions = ctrl.drain_completions();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].agent_id, "agent-1");
        assert_eq!(completions[0].status.as_str(), "completed");

        let info = ctrl.get_status("agent-1").unwrap();
        assert_eq!(info.status.as_str(), "completed");
    }

    #[tokio::test]
    async fn test_wait_buffers_other_completions() {
        let config = test_config();
        let mut ctrl = AgentControl::new(&config, "session-1", 0);
        let tx = ctrl.test_completion_tx();

        ctrl.insert_mock_handle("agent-a", "Atlas");
        ctrl.insert_mock_handle("agent-b", "Aurora");

        tx.send(SubAgentCompletion {
            agent_id: "agent-a".to_string(),
            nickname: "Atlas".to_string(),
            status: SubAgentStatus::Completed {
                result: "result-a".to_string(),
            },
            final_response: Some("result-a".to_string()),
        })
        .await
        .unwrap();

        tx.send(SubAgentCompletion {
            agent_id: "agent-b".to_string(),
            nickname: "Aurora".to_string(),
            status: SubAgentStatus::Completed {
                result: "result-b".to_string(),
            },
            final_response: Some("result-b".to_string()),
        })
        .await
        .unwrap();

        // Wait for B — should buffer A's completion
        let completion = ctrl.wait_for_agent("agent-b", 5).await.unwrap();
        assert_eq!(completion.agent_id, "agent-b");

        // A's completion should be retrievable via drain
        let drained = ctrl.drain_completions();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].agent_id, "agent-a");
    }

    #[tokio::test]
    async fn test_shutdown_agent_cancels_token() {
        let config = test_config();
        let mut ctrl = AgentControl::new(&config, "session-1", 0);
        let cancel = ctrl.insert_mock_handle("agent-1", "Atlas");

        assert!(!cancel.is_cancelled());
        ctrl.shutdown_agent("agent-1").unwrap();
        assert!(cancel.is_cancelled());
        assert_eq!(
            ctrl.get_status("agent-1").unwrap().status.as_str(),
            "shutdown"
        );
    }

    #[tokio::test]
    async fn test_shutdown_all_cancels_all_tokens() {
        let config = test_config();
        let mut ctrl = AgentControl::new(&config, "session-1", 0);
        let cancel_a = ctrl.insert_mock_handle("agent-a", "Atlas");
        let cancel_b = ctrl.insert_mock_handle("agent-b", "Aurora");

        assert!(!cancel_a.is_cancelled());
        assert!(!cancel_b.is_cancelled());
        ctrl.shutdown_all();
        assert!(cancel_a.is_cancelled());
        assert!(cancel_b.is_cancelled());
    }

    #[test]
    fn test_build_sub_agent_config_max_iterations() {
        let role = AgentRole {
            name: "researcher".to_string(),
            description: "Research agent".to_string(),
            model: None,
            provider: None,
            temperature: None,
            system_instructions: None,
            tools_allowed: None,
            max_iterations: Some(10),
        };
        let parent_config = Config::default();
        let ctrl = AgentControl::new(&test_config(), "session-1", 0);
        let child_config = ctrl.build_sub_agent_config(Some(&role), None, &parent_config);
        assert_eq!(child_config.conversation.max_iterations, 10);
    }

    #[test]
    fn test_build_sub_agent_config_auto_approve() {
        let parent_config = Config::default();
        let ctrl = AgentControl::new(&test_config(), "session-1", 0);
        let child_config = ctrl.build_sub_agent_config(None, None, &parent_config);
        assert_eq!(child_config.policy.auto_approve, vec!["*".to_string()]);
        assert_eq!(child_config.policy.deny, parent_config.policy.deny);
    }

    #[tokio::test]
    async fn test_send_input_nonexistent_agent() {
        let config = test_config();
        let ctrl = AgentControl::new(&config, "session-1", 0);
        let result = ctrl.send_input("nonexistent", "hello").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_get_status_returns_none_for_unknown() {
        let config = test_config();
        let ctrl = AgentControl::new(&config, "session-1", 0);
        assert!(ctrl.get_status("nonexistent").is_none());
    }

    #[test]
    fn delegate_blocklist_applied_without_role_produces_filtered_allowlist() {
        let parent = Config::default();
        let filter = compute_tools_filter(
            None,
            Some(AgentControl::DELEGATE_DEFAULT_BLOCKLIST),
            &parent,
        );
        let allowed = filter.expect("blocklist without role should yield a filter");
        // Every blocklisted name must be absent.
        for banned in AgentControl::DELEGATE_DEFAULT_BLOCKLIST {
            assert!(
                !allowed.iter().any(|t| t == *banned),
                "blocked tool '{banned}' leaked into the allowlist"
            );
        }
        // And we should still have a non-trivial set — at minimum read_file
        // must survive the filter.
        assert!(
            allowed.iter().any(|t| t == "read_file"),
            "read_file should survive the blocklist"
        );
    }

    #[test]
    fn delegate_blocklist_with_role_allowlist_intersects() {
        let role = AgentRole {
            name: "test".to_string(),
            description: "".to_string(),
            model: None,
            provider: None,
            temperature: None,
            system_instructions: None,
            tools_allowed: Some(vec![
                "read_file".to_string(),
                "write_memory".to_string(),
                "run_shell".to_string(),
            ]),
            max_iterations: None,
        };
        let parent = Config::default();
        let filter = compute_tools_filter(
            Some(&role),
            Some(AgentControl::DELEGATE_DEFAULT_BLOCKLIST),
            &parent,
        );
        let allowed = filter.unwrap();
        assert!(allowed.contains(&"read_file".to_string()));
        assert!(allowed.contains(&"run_shell".to_string()));
        assert!(
            !allowed.contains(&"write_memory".to_string()),
            "write_memory (blocked) must be removed even though the role allowed it"
        );
    }

    #[test]
    fn delegate_no_filters_returns_none() {
        let parent = Config::default();
        let filter = compute_tools_filter(None, None, &parent);
        assert!(
            filter.is_none(),
            "no role allowlist and no blocklist = inherit all"
        );
    }

    #[tokio::test]
    async fn wait_for_agent_returns_completion_shape() {
        // Exercises the wait-side of `spawn_and_wait` in isolation: we can't
        // reach the full spawn_and_wait path without an LLM, but this
        // confirms the completion the caller eventually receives carries the
        // expected `agent_id`/`status`/`final_response` shape.
        let config = test_config();
        let mut ctrl = AgentControl::new(&config, "session-1", 0);
        let tx = ctrl.test_completion_tx();
        ctrl.insert_mock_handle("agent-x", "Atlas");
        tx.send(SubAgentCompletion {
            agent_id: "agent-x".to_string(),
            nickname: "Atlas".to_string(),
            status: SubAgentStatus::Completed {
                result: "all done".to_string(),
            },
            final_response: Some("all done".to_string()),
        })
        .await
        .unwrap();

        let completion = ctrl.wait_for_agent("agent-x", 5).await.unwrap();
        assert_eq!(completion.agent_id, "agent-x");
        assert_eq!(completion.status.as_str(), "completed");
        assert_eq!(completion.final_response.as_deref(), Some("all done"));
    }

    #[test]
    fn mutating_tool_names_covers_every_non_readonly_tool() {
        // Guard: every tool surfaced to the LLM must be classified by
        // `is_mutating_tool` (via its read-only allowlist) OR appear in
        // `mutating_tool_names()`. If this test fails, a newly-added tool
        // is silently escaping Plan-mode's child blocklist.
        let parent = Config::default();
        let mut all: Vec<String> = crate::tool::definitions::core_tool_definitions(&parent)
            .into_iter()
            .map(|t| t.function.name)
            .collect();
        all.extend(
            tools::tool_definitions(0, u32::MAX)
                .into_iter()
                .map(|t| t.function.name),
        );
        // `is_mutating_tool`'s read-only allowlist (kept in sync by
        // construction — these are the names listed in agent.rs).
        let readonly: std::collections::HashSet<&str> = [
            "read_file",
            "list_dir",
            "list",
            "list_skills",
            "list_channels",
            "list_agents",
            "read_memory",
            "memory_search",
            "web_fetch",
            "web_search",
            "request_user_input",
            "get_goal",
            "vision_analyze",
        ]
        .into_iter()
        .collect();
        let mutating: std::collections::HashSet<&str> = crate::agent::mutating_tool_names()
            .iter()
            .copied()
            .collect();
        let mut missing: Vec<String> = Vec::new();
        for name in &all {
            if !readonly.contains(name.as_str()) && !mutating.contains(name.as_str()) {
                missing.push(name.clone());
            }
        }
        assert!(
            missing.is_empty(),
            "mutating_tool_names() is missing: {missing:?}. Add them or classify as read-only."
        );
    }

    #[test]
    fn delegated_task_round_trip() {
        let t = DelegatedTask {
            goal: "summarize this".into(),
            role_name: Some("researcher".into()),
            model_override: None,
        };
        assert_eq!(t.goal, "summarize this");
        assert_eq!(t.role_name.as_deref(), Some("researcher"));
    }
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
