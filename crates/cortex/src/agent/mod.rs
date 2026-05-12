use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info_span, instrument, warn, Instrument};

use crate::config::Config;
use crate::constants;
use crate::conversation::{
    compact_history, compact_tool_results, history_tokens, normalize_history, rewind_to_nth_user,
    undo_last_turn,
};
use crate::db::Database;
use crate::hooks::{HookAction, HookData, HookPoint, HookRegistry};
use crate::identity::load_identity;
use crate::internal_tag_filter::InternalTagFilter;
use crate::llm::{LlmClient, StreamEvent, UsageData};
use crate::llm_error::{classify_error_text, FailoverReason};
use crate::logging::log_message;
use crate::policy::ExecutionPolicy;
use crate::rate_guard::{RateDecision, SessionRateGuard};
use crate::security::secrets::redact_secrets;
use crate::session::Session;
use crate::skills::load_skills_context;
use crate::telemetry::BorgMetrics;
use crate::template::Template;
use crate::tool::names as tn;
#[cfg(test)]
use crate::tool_handlers;
use crate::truncate::truncate_output;
use crate::types::{ContentPart, FunctionCall, Message, ToolCall, ToolDefinition, ToolOutput};

use std::sync::LazyLock;

pub mod btw;
mod hooks_dispatch;
mod prompt_assembly;
mod stream_handler;
mod system_prompt;
pub(crate) mod system_prompt_cache;
mod tool_classification;
mod tool_validation;

use stream_handler::{StreamEventOutcome, TextOnlyOutcome};
use system_prompt_cache::SystemPromptCache;

pub use btw::{parse_btw_command, run_btw};
pub use tool_classification::mutating_tool_names;
use tool_classification::{classify_action, is_mutating_tool};

/// Spawn a background task that logs panics instead of silently swallowing them.
pub(crate) fn spawn_logged(
    name: &'static str,
    fut: impl std::future::Future<Output = ()> + Send + 'static,
) {
    let handle = tokio::spawn(fut);
    tokio::spawn(async move {
        if let Err(e) = handle.await {
            tracing::error!("Background task '{name}' panicked: {e}");
        }
    });
}

/// Max tokens for tool output before truncation (head + tail preserved).
const TOOL_OUTPUT_MAX_TOKENS: usize = constants::TOOL_OUTPUT_MAX_TOKENS;

fn parse_template(source: &str) -> Template {
    Template::parse(source).unwrap_or_else(|e| {
        tracing::error!("Failed to parse template: {e} — using fallback");
        // SAFETY: This literal template string is known valid at compile time
        #[allow(clippy::expect_used)]
        Template::parse("{{ memory }}").expect("fallback template must parse")
    })
}

static MEMORY_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    parse_template("\n<long_term_memory trust=\"stored\">\n{{ memory }}\n</long_term_memory>\n")
});

static KNOWLEDGE_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    parse_template("\n<knowledge_files trust=\"curated\">\n{{ knowledge }}\n</knowledge_files>\n")
});

static SKILLS_TEMPLATE: LazyLock<Template> =
    LazyLock::new(|| parse_template("\n<skills trust=\"verified\">\n{{ skills }}\n</skills>\n"));

static SETUP_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    parse_template("\n<first_conversation>\n{{ setup }}\n</first_conversation>\n")
});

// Collaboration mode templates
const COLLAB_MODE_EXECUTE: &str = include_str!("../../templates/collaboration_mode/execute.md");
const COLLAB_MODE_PLAN: &str = include_str!("../../templates/collaboration_mode/plan.md");

/// Maximum number of parallel tool calls allowed in a single LLM response.
/// Prevents OOM from malformed stream events with huge indices.
pub(super) const MAX_TOOL_CALLS: usize = constants::MAX_AGENT_TOOL_CALLS;

const SECURITY_POLICY: &str = include_str!("../../templates/security_policy.md");

/// Result of monthly token budget check.
enum BudgetCheck {
    Ok,
    Warning(String),
    Exceeded(String),
}

/// Result of the post-stream finalization helper. Mirrors the three control-flow
/// branches at the end of `run_agent_loop`.
enum IterationOutcome {
    /// Exit `run_agent_loop` with `Ok(())`. All persistence and TurnComplete
    /// events have already been emitted.
    EndTurn,
    /// Loop again with `nudged_for_response = true`.
    NudgeAndContinue,
    /// Loop again with `needs_response = true` (tool calls were executed).
    ToolCallsExecuted,
}

/// Events emitted by the agent loop during a conversation turn.
pub enum AgentEvent {
    /// Incremental text token from the LLM.
    TextDelta(String),
    /// Incremental thinking/reasoning token from the LLM.
    ThinkingDelta(String),
    /// A tool is about to execute.
    ToolExecuting {
        /// Stable tool call identifier for this lifecycle.
        tool_call_id: String,
        /// Tool name.
        name: String,
        /// Serialized tool arguments.
        args: String,
    },
    /// A tool has finished executing.
    ToolResult {
        /// Stable tool call identifier for this lifecycle.
        tool_call_id: String,
        /// Tool name.
        name: String,
        /// Tool output text.
        result: String,
    },
    /// Request confirmation from the user for a shell command. Send `true` to approve.
    ShellConfirmation {
        command: String,
        respond: oneshot::Sender<bool>,
    },
    /// Real-time output line from a running tool.
    ToolOutputDelta {
        /// Stable tool call identifier for this lifecycle.
        tool_call_id: String,
        name: String,
        delta: String,
        is_stderr: bool,
    },
    /// Token usage stats for the completed LLM call.
    Usage(UsageData),
    /// Progress update from a spawned sub-agent.
    SubAgentUpdate {
        /// Unique identifier for the sub-agent.
        agent_id: String,
        /// Human-readable sub-agent name.
        nickname: String,
        /// Current status description.
        status: String,
    },
    /// A user steer message was received and injected into history at a tool boundary.
    SteerReceived { text: String },
    /// The agent's plan has been updated (structured step tracking).
    PlanUpdated { steps: Vec<crate::types::PlanStep> },
    /// The agent is requesting user input mid-turn — a batch of 1-3 short
    /// multiple-choice questions rendered as a single carousel widget. The UI
    /// auto-appends an "Other" free-form row per question and exposes a
    /// Tab-toggled notes box. Reply with one [`Answer`] per question id;
    /// missing ids are treated as empty answers.
    ///
    /// [`Answer`]: crate::tool_handlers::user_input::Answer
    UserInputRequest {
        questions: Vec<crate::tool_handlers::user_input::Question>,
        respond: oneshot::Sender<
            std::collections::HashMap<String, crate::tool_handlers::user_input::Answer>,
        >,
    },
    /// Emitted between tool result and next LLM stream to indicate preparation work.
    Preparing,
    /// The agent turn has completed (no more tool calls).
    TurnComplete,
    /// History was compacted to stay under the token budget.
    HistoryCompacted {
        /// Number of messages replaced by the summary.
        dropped: usize,
        /// Estimated tokens before compaction.
        before_tokens: usize,
        /// Estimated tokens after compaction.
        after_tokens: usize,
        /// `true` if an iterative update of a prior summary was used.
        iterative: bool,
    },
    /// An error occurred during the agent turn.
    Error(String),
    /// Out-of-band notice from the daemon — not part of the LLM stream.
    /// Clients render this as a system cell / toast (e.g. posture transitions).
    SystemNotice {
        /// Free-form category tag (`"posture"`, `"budget"`, …) used by clients
        /// to route or style the notice.
        category: String,
        /// User-facing text.
        text: String,
    },
}

/// Returns true if the agent should retry the LLM call because it returned
/// empty text after tool execution (safety net for silent completions).
fn should_nudge_for_response(
    text_content: &str,
    needs_response: bool,
    already_nudged: bool,
) -> bool {
    text_content.trim().is_empty() && needs_response && !already_nudged
}

use tool_validation::repeated_tool_call_block_message;
use tool_validation::{
    format_invalid_tool_result, has_unresolved_tool_calls, partition_tool_calls, InvalidToolCall,
    PartialToolCall, ToolCallRepeatTracker,
};

/// Which LLM chain a particular agent instance routes through.
///
/// `Primary` is the default for interactive turns and tool-spawned
/// sub-agents. `Auxiliary` is reserved for background work (memory
/// consolidation, heartbeat, scheduled prompts) that must keep running
/// when the primary provider is degraded — those construct via
/// [`LlmClient::for_auxiliary`] which routes through `[llm.aux]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LlmRole {
    #[default]
    Primary,
    Auxiliary,
}

/// Core conversation agent: manages history, tools, and the LLM interaction loop.
pub struct Agent {
    /// Primary config. Must be kept in sync with `config_arc` — update both in `reload_config`.
    pub(super) config: Config,
    pub(super) history: Vec<Message>,
    pub(super) session: Session,
    pub(super) policy: ExecutionPolicy,
    pub(super) hook_registry: HookRegistry,
    pub(super) turn_count: u32,
    pub(super) rate_guard: SessionRateGuard,
    pub(super) agent_control: Option<crate::multi_agent::AgentControl>,
    pub(super) spawn_depth: u32,
    /// When set, restricts which tools this agent may use (from role's `tools_allowed`).
    pub(super) tools_filter: Option<Vec<String>>,
    /// Env vars declared by skills' `requires.env` — only these credentials are injected into run_shell.
    pub(super) skill_env_allowlist: std::collections::HashSet<String>,
    pub(super) metrics: BorgMetrics,
    pub(super) browser_session: Option<crate::browser::BrowserSession>,
    pub(super) tts_synthesizer: Option<crate::media::tts::TtsSynthesizer>,
    pub(super) config_rx: Option<tokio::sync::watch::Receiver<Config>>,
    pub(super) db: Mutex<Option<Database>>,
    /// System-prompt input caches (identity, credentials, project docs, skills).
    /// All invalidation goes through typed event methods on this struct —
    /// callers do not poke fields directly. See [`SystemPromptCache`].
    pub(super) caches: SystemPromptCache,
    /// Ghost commit created at session start for atomic undo (coding agent).
    pub(super) ghost_commit: Option<crate::git::GhostCommit>,
    /// Per-session workspace execution context used by tools and prompt assembly.
    pub(super) execution_context: crate::execution_context::ExecutionContext,
    /// Git repo root (if CWD is inside a git repo).
    pub(super) git_repo_root: Option<std::path::PathBuf>,
    /// Arc-wrapped config for cheap cloning into background tasks.
    pub(super) config_arc: Arc<Config>,
    /// Channel for receiving user steer messages mid-turn (injected at tool boundaries).
    pub(super) steer_rx: Option<mpsc::UnboundedReceiver<String>>,
    /// Tool names used in recent turns, for conditional tool loading.
    pub(super) recent_tool_names: std::collections::HashSet<String>,
    /// Optional best-effort sink for borgd cost events.
    pub(super) cost_event_sink: Option<Arc<dyn crate::llm::CostEventSink>>,
    /// Short-term working memory for the current session.
    pub(super) short_term_memory: crate::short_term_memory::ShortTermMemory,
    /// Most recent compaction summary text (no marker prefix). Used by
    /// `compact_history_v2` to issue iterative-update prompts rather than
    /// re-summarizing from scratch. Populated either by the current
    /// session's compactions or by scanning loaded history on resume.
    pub(super) previous_summary: Option<String>,
    /// True for exactly one turn: set when `SETUP.md` was just consumed during
    /// system-prompt assembly. The turn that consumed it must not offer tools
    /// to the model (SETUP.md forbids first-message tool calls but weak models
    /// ignore the prompt instruction). Reset after the prompt is built and the
    /// tool list is selected.
    pub(super) setup_just_consumed: bool,
    /// Tracks runs of identical consecutive tool calls within a turn.
    /// Reset at the start of each `run_agent_loop`.
    pub(super) tool_call_repeat: ToolCallRepeatTracker,
    /// Which LLM chain this agent's `make_llm_client_from_config` builds.
    /// `Primary` is the default — interactive turns and tool-spawned
    /// sub-agents always run on the user's primary chain. `Auxiliary` is
    /// set by [`Agent::new_sub_agent_aux`] for background work so a
    /// primary outage doesn't take down nightly cron.
    pub(super) llm_role: LlmRole,
}

/// Common state produced by `build_common`, consumed by both `new()` and `new_sub_agent()`.
struct AgentCommon {
    policy: ExecutionPolicy,
    rate_guard: SessionRateGuard,
    session: Session,
    db: Option<Database>,
    skill_env_allowlist: std::collections::HashSet<String>,
    tts_synthesizer: Option<crate::media::tts::TtsSynthesizer>,
}

/// Build the shared initialization state used by both `Agent::new` and `Agent::new_sub_agent`.
fn build_common(config: &Config) -> Result<AgentCommon> {
    let _ = LlmClient::new(config)?;
    let policy = config.policy.clone();
    let session = Session::new();
    let db = match Database::open() {
        Ok(db) => Some(db),
        Err(e) => {
            warn!("Failed to open database on agent init: {e}");
            None
        }
    };
    // Action limits receive an evolution-stage floor: higher stages unlock
    // additional headroom per `docs/evolution.md#action-limits-by-stage`.
    // Explicit user config (set higher) is preserved via `max`.
    let mut limits = config.security.action_limits.clone();
    if let Some(ref d) = db {
        if let Ok(state) = d.get_evolution_state() {
            let stage = match state.stage {
                crate::evolution::Stage::Base => crate::rate_guard::EvolutionStage::Base,
                crate::evolution::Stage::Evolved => crate::rate_guard::EvolutionStage::Evolved,
                crate::evolution::Stage::Final => crate::rate_guard::EvolutionStage::Final,
            };
            limits.apply_stage(stage);
        }
    }
    let rate_guard = SessionRateGuard::new(limits);
    let resolved_creds = config.resolve_credentials();
    let skill_env_allowlist =
        crate::skills::collect_required_env_vars(&resolved_creds, &config.skills);
    let tts_synthesizer = crate::media::tts::TtsSynthesizer::from_config(config);
    Ok(AgentCommon {
        policy,
        rate_guard,
        session,
        db,
        skill_env_allowlist,
        tts_synthesizer,
    })
}

impl Agent {
    /// Acquire the database lock, recovering from poison if a prior holder panicked.
    fn db_guard(&self) -> std::sync::MutexGuard<'_, Option<Database>> {
        match self.db.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::error!(
                    "Database mutex poisoned — a prior operation panicked while holding the lock"
                );
                poisoned.into_inner()
            }
        }
    }

    /// Ensure credential cache is populated, resolving on first call or after config reload.
    /// Separated from the getter so the getter can return a borrow while leaving
    /// `&self` free for other immutable accesses.
    fn ensure_credentials_cached(&mut self) {
        let config = &self.config;
        self.caches
            .ensure_credentials(|| config.resolve_credentials());
    }

    fn latest_user_message_text(&self) -> &str {
        self.history
            .iter()
            .rev()
            .find(|m| m.role == crate::types::Role::User)
            .and_then(|m| m.text_content())
            .unwrap_or("")
    }

    fn refresh_skill_runtime_state(&mut self) {
        self.caches.on_skill_modified();
        self.ensure_credentials_cached();
        self.skill_env_allowlist = crate::skills::collect_required_env_vars(
            self.credentials_cached(),
            &self.config.skills,
        );
    }

    fn maybe_refresh_skills_after_tool(
        &mut self,
        tool_name: &str,
        args: &serde_json::Value,
        output: &str,
    ) {
        let touched_skills = match tool_name {
            tn::APPLY_SKILL_PATCH => true,
            tn::APPLY_PATCH => args["target"].as_str() == Some("skills"),
            _ => false,
        };
        if touched_skills && output.contains("applied successfully") {
            self.refresh_skill_runtime_state();
        }
    }

    /// Borrow the cached credentials, falling back to an empty map if the cache
    /// was not populated yet. Callers inside this module always invoke
    /// [`Self::ensure_credentials_cached`] first; the fallback keeps the
    /// skill-loading path robust if that ordering ever regresses.
    fn credentials_cached(&self) -> &HashMap<String, String> {
        self.caches.credentials_or_empty()
    }

    /// Assemble an `Agent` from pre-computed parts shared by all constructors.
    fn build_agent(
        config: Config,
        common: AgentCommon,
        agent_control: Option<crate::multi_agent::AgentControl>,
        spawn_depth: u32,
        tools_filter: Option<Vec<String>>,
        metrics: BorgMetrics,
        llm_role: LlmRole,
    ) -> Self {
        let execution_context = crate::execution_context::ExecutionContext::from_process()
            .unwrap_or_else(|_| {
                crate::execution_context::ExecutionContext::unrestricted(std::path::PathBuf::from(
                    ".",
                ))
            });
        let git_repo_root = crate::git::find_repo_root(execution_context.workspace_cwd());
        let config_arc = Arc::new(config.clone());
        let session_id = common.session.meta.id.clone();
        Self {
            config,
            history: Vec::new(),
            session: common.session,
            policy: common.policy,
            hook_registry: HookRegistry::new(),
            turn_count: 0,
            rate_guard: common.rate_guard,
            agent_control,
            spawn_depth,
            tools_filter,
            skill_env_allowlist: common.skill_env_allowlist,
            tts_synthesizer: common.tts_synthesizer,
            metrics,
            browser_session: None,
            config_rx: None,
            db: Mutex::new(common.db),
            caches: SystemPromptCache::default(),
            ghost_commit: None,
            execution_context,
            git_repo_root,
            config_arc,
            steer_rx: None,
            recent_tool_names: std::collections::HashSet::new(),
            cost_event_sink: None,
            short_term_memory: crate::short_term_memory::ShortTermMemory::new(session_id, 2000),
            previous_summary: None,
            setup_just_consumed: false,
            tool_call_repeat: ToolCallRepeatTracker::default(),
            llm_role,
        }
    }

    /// Create a new agent with the given config and metrics.
    pub fn new(config: Config, metrics: BorgMetrics) -> Result<Self> {
        let common = build_common(&config)?;
        let agent_control = if config.agents.enabled {
            Some(crate::multi_agent::AgentControl::new(
                &config.agents,
                &common.session.meta.id,
                0,
            ))
        } else {
            None
        };

        // Best-effort embedding cache pruning on startup
        Self::prune_embedding_cache_on_startup(&config);

        Ok(Self::build_agent(
            config,
            common,
            agent_control,
            0,
            None,
            metrics,
            LlmRole::Primary,
        ))
    }

    /// Prune stale embedding cache entries on startup based on config.
    fn prune_embedding_cache_on_startup(config: &Config) {
        let ttl_days = config.memory.cache_ttl_days;
        let max_entries = config.memory.cache_max_entries;

        if ttl_days == 0 && max_entries == 0 {
            return;
        }

        let db = match Database::open() {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!("Cache pruning: failed to open database: {e}");
                return;
            }
        };

        if ttl_days > 0 {
            let max_age_secs = i64::from(ttl_days) * constants::SECS_PER_DAY;
            match db.prune_embedding_cache(max_age_secs) {
                Ok(0) => {}
                Ok(n) => tracing::info!(
                    "Pruned {n} stale embedding cache entries (>{ttl_days} days old)"
                ),
                Err(e) => tracing::warn!("Cache TTL pruning failed: {e}"),
            }
        }

        if max_entries > 0 {
            match db.prune_embedding_cache_by_count(max_entries) {
                Ok(0) => {}
                Ok(n) => {
                    tracing::info!("Pruned {n} embedding cache entries (over {max_entries} limit)")
                }
                Err(e) => tracing::warn!("Cache count pruning failed: {e}"),
            }
        }
    }

    /// Set a channel for receiving user steer messages mid-turn.
    pub fn set_steer_channel(&mut self, rx: mpsc::UnboundedReceiver<String>) {
        self.steer_rx = Some(rx);
    }

    /// Set a config watch receiver for hot reload.
    pub fn set_config_watcher(&mut self, rx: tokio::sync::watch::Receiver<Config>) {
        self.config_rx = Some(rx);
    }

    /// Install a best-effort cost-event sink for all session-backed LLM calls.
    pub fn set_cost_event_sink(&mut self, sink: Arc<dyn crate::llm::CostEventSink>) {
        self.cost_event_sink = Some(sink);
    }

    /// Override the session execution context. Use this when the workspace
    /// changes (different cwd / root). Invalidates the project-docs cache and
    /// re-derives `git_repo_root`. To swap *only* the IDE-backed text-file
    /// bridge inside an ACP-backed turn, prefer `set_text_file_bridge` —
    /// otherwise every turn pays for an `AGENTS.md` / `CLAUDE.md` re-read.
    pub fn set_execution_context(
        &mut self,
        execution_context: crate::execution_context::ExecutionContext,
    ) {
        self.execution_context = execution_context;
        self.git_repo_root = crate::git::find_repo_root(self.execution_context.workspace_cwd());
        self.caches.clear_project_docs();
    }

    /// Attach or remove the IDE-backed text-file bridge without invalidating
    /// the workspace-derived caches. Hot path on ACP-backed turns.
    pub fn set_text_file_bridge(
        &mut self,
        bridge: Option<std::sync::Arc<dyn crate::execution_context::TextFileBridge>>,
    ) {
        self.execution_context = self.execution_context.clone().with_text_file_bridge(bridge);
    }

    /// Snapshot the current execution context.
    pub fn execution_context(&self) -> crate::execution_context::ExecutionContext {
        self.execution_context.clone()
    }

    /// Replace config, re-derive dependent state (policy, rate limits).
    fn reload_config(&mut self, new_config: Config) {
        self.policy = new_config.policy.clone();
        self.rate_guard
            .update_limits(new_config.security.action_limits.clone());
        let resolved_creds = new_config.resolve_credentials();
        self.caches.on_config_reloaded(resolved_creds);
        self.config_arc = Arc::new(new_config.clone());
        self.config = new_config;
        // skills caches were just cleared by `on_config_reloaded`;
        // recompute the env allowlist now that credentials are fresh.
        self.skill_env_allowlist = crate::skills::collect_required_env_vars(
            self.credentials_cached(),
            &self.config.skills,
        );
    }

    fn make_llm_client_from_config(&self, config: &Config) -> Result<LlmClient> {
        let mut client = match self.llm_role {
            LlmRole::Primary => LlmClient::new(config)?,
            LlmRole::Auxiliary => LlmClient::for_auxiliary(config)?,
        };
        if let Some(ref sink) = self.cost_event_sink {
            if config.telemetry.cost_events {
                // Tag the sink so dashboards can split spend by role —
                // an aux outage budget is its own line item, not lumped
                // into chat spend.
                let kind = match self.llm_role {
                    LlmRole::Primary => "chat",
                    LlmRole::Auxiliary => "auxiliary",
                };
                client = client.with_cost_event_sink(
                    sink.clone(),
                    crate::llm::CostEventContext {
                        session_id: self.session.meta.id.clone(),
                        kind: kind.to_string(),
                    },
                );
            }
        }
        Ok(client)
    }

    /// Create a sub-agent that shares the parent's config but has its own session.
    pub fn new_sub_agent(
        config: Config,
        spawn_depth: u32,
        agents_config: &crate::config::MultiAgentConfig,
        metrics: BorgMetrics,
        tools_filter: Option<Vec<String>>,
    ) -> Result<Self> {
        Self::new_sub_agent_with_role(
            config,
            spawn_depth,
            agents_config,
            metrics,
            tools_filter,
            LlmRole::Primary,
        )
    }

    /// Create a sub-agent that routes through the auxiliary LLM chain
    /// (`[llm.aux]`). Use for background work — memory consolidation,
    /// scheduled prompts, heartbeat — so a primary-provider outage can't
    /// take down nightly cron. Tool-spawned sub-agents and interactive
    /// turns must keep using [`Agent::new_sub_agent`] (Primary).
    pub fn new_sub_agent_aux(
        config: Config,
        spawn_depth: u32,
        agents_config: &crate::config::MultiAgentConfig,
        metrics: BorgMetrics,
        tools_filter: Option<Vec<String>>,
    ) -> Result<Self> {
        Self::new_sub_agent_with_role(
            config,
            spawn_depth,
            agents_config,
            metrics,
            tools_filter,
            LlmRole::Auxiliary,
        )
    }

    fn new_sub_agent_with_role(
        config: Config,
        spawn_depth: u32,
        agents_config: &crate::config::MultiAgentConfig,
        metrics: BorgMetrics,
        tools_filter: Option<Vec<String>>,
        llm_role: LlmRole,
    ) -> Result<Self> {
        let common = build_common(&config)?;
        let agent_control = if agents_config.enabled && spawn_depth < agents_config.max_spawn_depth
        {
            Some(crate::multi_agent::AgentControl::new(
                agents_config,
                &common.session.meta.id,
                spawn_depth,
            ))
        } else {
            None
        };
        Ok(Self::build_agent(
            config,
            common,
            agent_control,
            spawn_depth,
            tools_filter,
            metrics,
            llm_role,
        ))
    }

    /// Inject a message directly into the conversation history.
    pub fn inject_history_message(&mut self, msg: Message) {
        self.history.push(msg);
    }

    /// Get a mutable reference to the hook registry for registering lifecycle hooks.
    pub fn hook_registry_mut(&mut self) -> &mut HookRegistry {
        &mut self.hook_registry
    }

    /// Restore a session's history into this agent.
    pub fn load_session(&mut self, id: &str) -> Result<()> {
        let session = Session::load(id)?;
        self.history = session.messages.clone();
        self.session = session;
        Ok(())
    }

    /// Start a new session, clearing history.
    pub fn new_session(&mut self) {
        // Fire SessionEnd for the previous session if any turns were executed
        if self.turn_count > 0 {
            let hook_ctx = self.hook_ctx(
                HookPoint::SessionEnd,
                HookData::SessionEnd {
                    session_id: self.session.meta.id.clone(),
                    total_turns: self.turn_count,
                },
            );
            self.hook_registry.dispatch(&hook_ctx);
            self.flush_short_term_memory();
        }

        self.history.clear();
        self.session = Session::new();
        self.short_term_memory =
            crate::short_term_memory::ShortTermMemory::new(self.session.meta.id.clone(), 2000);

        // Fire SessionStart for the new session
        let hook_ctx = self.hook_ctx(
            HookPoint::SessionStart,
            HookData::SessionStart {
                session_id: self.session.meta.id.clone(),
            },
        );
        self.hook_registry.dispatch(&hook_ctx);
    }

    /// Flush accumulated short-term memory facts to the daily log entry in
    /// the database. Called on session end so facts the agent collected during
    /// the session are promoted into long-term memory (via nightly
    /// consolidation).
    ///
    /// Reuses the agent's open DB handle when available so a session-end
    /// flush doesn't race a fresh `Database::open()` against the agent's own
    /// writes (and, in tests, so the flush hits the same in-memory DB the
    /// caller is inspecting).
    ///
    /// Fire-and-forget: background shutdown must not crash on a storage error.
    /// Any failure is logged but otherwise ignored.
    pub fn flush_short_term_memory(&self) {
        let facts = self.short_term_memory.facts_as_text();
        if facts.trim().is_empty() {
            return;
        }
        let guard = self.db_guard();
        if let Err(e) =
            crate::consolidation::flush_short_term_to_daily_with_optional_db(guard.as_ref(), &facts)
        {
            warn!("flush_short_term_memory failed: {e}");
        }
    }

    /// Signal that this agent's owning context is ending (e.g. CLI shutdown,
    /// gateway handler finishing). Fires SessionEnd and flushes short-term
    /// memory even when no `new_session()` call follows. Safe to call multiple
    /// times; a no-op if no turns were executed.
    pub fn end_session(&mut self) {
        if self.turn_count == 0 {
            return;
        }
        let hook_ctx = self.hook_ctx(
            HookPoint::SessionEnd,
            HookData::SessionEnd {
                session_id: self.session.meta.id.clone(),
                total_turns: self.turn_count,
            },
        );
        self.hook_registry.dispatch(&hook_ctx);
        self.flush_short_term_memory();
        // Clear turn_count so a repeat end_session() is a no-op.
        self.turn_count = 0;
    }

    /// Auto-save current session state.
    pub fn auto_save(&mut self) {
        self.session.update_from_history(&self.history);
        if let Err(e) = self.session.save() {
            warn!("Failed to auto-save session: {e}");
        }
    }

    /// Append a system-injected assistant message to the agent's history.
    ///
    /// Used to mirror text the user already saw on this client (e.g. a
    /// heartbeat that was streamed in by the daemon while the TUI was open)
    /// into the active session so the next user reply has it in context.
    /// Without this, the user's "yes please" reply lands in a session whose
    /// history has no record of the heartbeat, and the agent answers the
    /// previous turn instead.
    ///
    /// Refuses to inject if doing so would split an `assistant(tool_calls)`
    /// message from its outstanding `tool` results — both Anthropic and
    /// OpenAI reject orphaned tool_use blocks on the next turn, so a
    /// well-meaning mid-turn mirror would brick the session.
    pub fn append_assistant_message(&mut self, text: &str) {
        if has_unresolved_tool_calls(&self.history) {
            warn!(
                "append_assistant_message: refused — last assistant turn has unresolved \
                 tool_calls; injecting now would orphan them. Dropping the mirrored message."
            );
            return;
        }
        self.persist_message(Message::assistant(text));
    }

    /// Log a message and push it to history + SQLite in one step.
    fn log_and_persist(&mut self, msg: Message) {
        log_message(&msg);
        self.persist_message(msg);
    }

    /// Record a skipped/cancelled tool call as a tool_result message.
    fn skip_tool_call(&mut self, tool_call_id: &str, reason: &str) {
        self.log_and_persist(Message::tool_result(tool_call_id, reason));
    }

    /// Optionally redact secrets from a string based on config.
    fn maybe_redact(&self, s: String) -> String {
        if self.config.security.secret_detection {
            redact_secrets(&s)
        } else {
            s
        }
    }

    /// Truncate and optionally redact secrets from raw tool output.
    fn truncate_and_redact(&self, raw: &str) -> String {
        self.maybe_redact(truncate_output(raw, TOOL_OUTPUT_MAX_TOKENS))
    }

    /// Push a message to history and persist it to SQLite for crash recovery.
    fn persist_message(&mut self, msg: Message) {
        let session_id = self.session.meta.id.clone();
        let role = match msg.role {
            crate::types::Role::System => "system",
            crate::types::Role::User => "user",
            crate::types::Role::Assistant => "assistant",
            crate::types::Role::Tool => "tool",
        };
        let tool_calls_json = msg
            .tool_calls
            .as_ref()
            .and_then(|tc| serde_json::to_string(tc).ok());
        let (content_text, content_parts_json) = match &msg.content {
            Some(crate::types::MessageContent::Parts(parts)) => {
                let full = msg
                    .content
                    .as_ref()
                    .map(crate::types::MessageContent::full_text);
                let parts_json = serde_json::to_string(parts).ok();
                (full, parts_json)
            }
            _ => (msg.text_content().map(str::to_string), None),
        };
        {
            let guard = self.db_guard();
            if let Some(ref db) = *guard {
                if let Err(e) = db.insert_message(
                    &session_id,
                    role,
                    content_text.as_deref(),
                    tool_calls_json.as_deref(),
                    msg.tool_call_id.as_deref(),
                    msg.timestamp.as_deref(),
                    content_parts_json.as_deref(),
                ) {
                    warn!("Failed to persist message to SQLite: {e}");
                }
            }
        }
        self.history.push(msg);
    }

    /// Get a reference to the current session.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Get the current conversation history.
    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Get a reference to the current config.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Get a mutable reference to the current config.
    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.config
    }

    /// Get a reference to the telemetry metrics.
    pub fn metrics(&self) -> &BorgMetrics {
        &self.metrics
    }

    /// Compact conversation history using LLM summarization, returning (before_tokens, after_tokens).
    #[instrument(skip_all, fields(session_id = %self.session.meta.id))]
    /// Compact conversation history to fit within context limits. Returns (before, after) message counts.
    pub async fn compact(&mut self) -> (usize, usize) {
        let before = history_tokens(&self.history);
        let llm = match self.make_llm_client_from_config(&self.config) {
            Ok(l) => l,
            Err(_) => return (before, before),
        };
        let max_hist = self.config.conversation.max_history_tokens;
        let protect_first_n = self.config.conversation.protect_first_n;
        if protect_first_n > 0 {
            if self.previous_summary.is_none() {
                self.previous_summary =
                    crate::conversation::extract_last_compaction_summary(&self.history);
            }
            crate::conversation::compact_history_v2(
                &mut self.history,
                max_hist,
                protect_first_n,
                &mut self.previous_summary,
                &llm,
            )
            .await;
        } else {
            compact_history(&mut self.history, max_hist, &llm).await;
        }
        let after = history_tokens(&self.history);
        (before, after)
    }

    /// Clear all conversation history.
    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    /// Undo the last agent turn: remove everything back to the last user message.
    /// Returns the number of messages removed, or 0 if nothing to undo.
    pub fn undo(&mut self) -> usize {
        undo_last_turn(&mut self.history)
    }

    /// Rewind conversation to the Nth user message (0-indexed, oldest-first).
    /// Removes that message and everything after it.
    /// Returns the number of messages removed.
    pub fn rewind_to_nth_user_message(&mut self, n: usize) -> usize {
        rewind_to_nth_user(&mut self.history, n)
    }

    /// Returns (message_count, estimated_token_count) for the current session.
    pub fn conversation_stats(&self) -> (usize, usize) {
        (self.history.len(), history_tokens(&self.history))
    }

    // Per-section builders live in `agent::system_prompt` (second impl block);
    // `build_system_prompt` remains here because it orchestrates caches.

    #[instrument(skip_all)]
    async fn build_system_prompt(&mut self) -> Result<String> {
        let identity = self.caches.identity_or_load(load_identity)?;
        let memory = self.load_memory_with_ranking().await?;
        let knowledge = self.load_knowledge_with_ranking().await?;
        let mut system = String::with_capacity(16_384);

        // === STABLE PREFIX (cache-friendly) ===
        // Ordered so the largest byte-stable content comes first, enabling
        // implicit prefix caching on providers that hash the prompt (OpenAI,
        // Gemini) and maximizing cache hits on providers with explicit markers
        // (Anthropic). Anything that changes turn-to-turn is pushed to the
        // dynamic suffix below.
        system.push_str("<system_instructions>\n");
        system.push_str(&identity);
        system.push_str("\n</system_instructions>\n\n");

        // Tooling: list available tools with summaries
        system.push_str(&self.build_tooling_section());

        // Tool call style guidance
        system.push_str(Self::build_tool_call_style_section());
        system.push_str(Self::build_skill_usage_section());

        // Safety / security policy
        use std::fmt::Write as _;
        let _ = write!(
            system,
            "\n<security_policy>\n{SECURITY_POLICY}\n</security_policy>\n"
        );

        if self.config.skills.enabled {
            let user_request = self.latest_user_message_text().to_string();
            let skills = if let Some(cached) = self.caches.skills_for_request(&user_request) {
                cached.to_string()
            } else {
                self.ensure_credentials_cached();
                let budget = crate::skills::effective_skill_budget(
                    &self.config.skills,
                    Some(self.active_context_window()),
                );
                let (rendered, report) = load_skills_context(
                    budget,
                    self.credentials_cached(),
                    &self.config.skills,
                    &user_request,
                )?;
                self.warn_skill_budget_loss(&report);
                self.caches.set_skills(user_request, rendered.clone());
                rendered
            };
            if !skills.is_empty() {
                system.push_str(
                    &SKILLS_TEMPLATE
                        .render([("skills", skills.as_str())])
                        .context("skills template render failed")?,
                );
            }
        }

        if !memory.is_empty() {
            system.push_str(
                &MEMORY_TEMPLATE
                    .render([("memory", memory.as_str())])
                    .context("memory template render failed")?,
            );
        }
        if !knowledge.is_empty() {
            system.push_str(
                &KNOWLEDGE_TEMPLATE
                    .render([("knowledge", knowledge.as_str())])
                    .context("knowledge template render failed")?,
            );
        }

        system.push_str("\n<memory_recall>\nWhen answering questions about prior work, past decisions, dates, people, preferences, todos, or anything previously discussed, use the memory_search tool to look up relevant context. Auto-loaded memory above may not contain all relevant information.\n</memory_recall>\n");

        // Reply tags for message threading (channels)
        system.push_str(Self::build_reply_tags_section());

        // Messaging / channel routing guidance
        system.push_str(&self.build_messaging_section());

        // Silent reply protocol
        system.push_str(&Self::build_silent_reply_section());

        // Heartbeat ack protocol
        system.push_str(&self.build_heartbeat_section());

        // Reasoning format (conditional on thinking level)
        system.push_str(&self.build_reasoning_section());

        // Project documentation (AGENTS.md / CLAUDE.md)
        let project_docs = self.caches.project_docs_or_load(|| {
            crate::project_doc::discover_project_docs(self.execution_context.workspace_cwd())
                .ok()
                .flatten()
        });
        if let Some(ref docs) = project_docs {
            let _ = write!(
                system,
                "\n<project_instructions trust=\"stored\">\n{docs}\n</project_instructions>\n"
            );
        }

        // === DYNAMIC SUFFIX (per-turn, cache-invalidating) ===

        // Active memory: per-turn hybrid search of memory + past sessions
        // keyed off the latest user message. Placed at the start of the
        // dynamic suffix so it sits adjacent to <working_memory> and after
        // the stable <long_term_memory> prefix. Empty string when disabled
        // or when the search returns no hits.
        system.push_str(&self.build_active_memory_section().await);

        // Short-term working memory (session facts, active project)
        let working_memory = self.short_term_memory.render();
        if !working_memory.is_empty() {
            system.push_str(&working_memory);
        }

        system.push_str(&self.build_collaboration_section());
        system.push_str(&self.build_environment_section().await);
        system.push_str(&self.build_scheduled_tasks_section());

        // Inject first-conversation instructions from SETUP.md (created during onboarding)
        if let Ok(data_dir) = crate::config::Config::data_dir() {
            let setup_path = data_dir.join("SETUP.md");
            // Atomically rename to prevent duplicate injection from concurrent sessions
            let consumed = setup_path.with_extension("md.consumed");
            if tokio::fs::rename(&setup_path, &consumed).await.is_ok() {
                if let Ok(setup) = tokio::fs::read_to_string(&consumed).await {
                    system.push_str(
                        &SETUP_TEMPLATE
                            .render([("setup", setup.as_str())])
                            .context("setup template render failed")?,
                    );
                    // Signal to the agent turn: on this turn we must not offer
                    // tools to the model. SETUP.md forbids first-message tool
                    // calls in prose, but that is not sufficient for weak
                    // instruction-following models (e.g. Grok-4.20 emits a
                    // malformed `write_memory` call anyway).
                    self.setup_just_consumed = true;
                }
                let _ = tokio::fs::remove_file(&consumed).await;
            }
        }

        Ok(system)
    }

    #[instrument(skip_all, fields(session_id = %self.session.meta.id))]
    /// Send a user message and run the agent loop, emitting events to the channel.
    pub async fn send_message(
        &mut self,
        user_input: &str,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
        self.send_message_with_cancel(user_input, event_tx, CancellationToken::new())
            .await
    }

    #[instrument(skip_all, fields(session_id = %self.session.meta.id, turn_count = self.turn_count))]
    /// Send a user message with cancellation support.
    pub async fn send_message_with_cancel(
        &mut self,
        user_input: &str,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        if user_input.trim().is_empty() {
            tracing::debug!("Ignoring empty user input");
            if let Err(e) = event_tx.send(AgentEvent::TurnComplete).await {
                tracing::warn!("Failed to send TurnComplete for empty input: {e}");
            }
            return Ok(());
        }
        let msg = Message::user(user_input);
        self.log_and_persist(msg);
        self.turn_count += 1;
        // Reset recent tool names each user turn so conditional loading
        // only considers tools used in the current turn's agent loop.
        self.recent_tool_names.clear();
        self.run_agent_loop(event_tx, cancel).await
    }

    /// Send a pre-constructed Message (e.g. multimodal) through the agent loop.
    #[instrument(skip_all, fields(session_id = %self.session.meta.id, turn_count = self.turn_count))]
    /// Send a message and run the agent loop, returning the final assistant text.
    pub async fn send_message_raw(
        &mut self,
        msg: Message,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        self.log_and_persist(msg);
        self.turn_count += 1;
        self.run_agent_loop(event_tx, cancel).await
    }

    #[instrument(skip_all, fields(session_id = %self.session.meta.id, turn_count = self.turn_count))]
    /// One-time setup before entering the agent loop: ghost commit, project doc caching,
    /// and background indexing of extra memory paths and past sessions.
    async fn prepare_loop(&mut self) {
        // Create ghost commit for coding safety (lazy, one-time)
        if self.ghost_commit.is_none() {
            if let Some(ref root) = self.git_repo_root {
                match crate::git::create_ghost_commit(root).await {
                    Ok(gc) => {
                        tracing::info!(commit_id = %gc.commit_id, "Created ghost commit for session");
                        if let Ok(guard) = self.db.lock() {
                            if let Some(ref db) = *guard {
                                crate::activity_log::log_activity(
                                    db,
                                    "info",
                                    "session",
                                    &format!("Ghost commit created: {}", gc.commit_id),
                                );
                            }
                        }
                        self.ghost_commit = Some(gc);
                    }
                    Err(e) => tracing::debug!("Skipping ghost commit: {e}"),
                }
            }
        }

        // Cache project docs on first run (the system-prompt builder also
        // calls this lazily; running it here once outside that hot path
        // means the first build_system_prompt call doesn't pay for IO).
        let _ = self.caches.project_docs_or_load(|| {
            crate::project_doc::discover_project_docs(self.execution_context.workspace_cwd())
                .ok()
                .flatten()
        });

        // Background: index extra paths and pending sessions on first run
        if self.config.memory.embeddings.enabled {
            if !self.config.memory.extra_paths.is_empty() {
                let config = Arc::clone(&self.config_arc);
                spawn_logged("embed_extra_paths", async move {
                    let files = crate::memory::scan_extra_paths(
                        &config.memory.extra_paths,
                        &config.security.blocked_paths,
                    );
                    for (filename, path) in files {
                        if let Ok(content) = tokio::fs::read_to_string(&path).await {
                            let _ = crate::embeddings::embed_memory_file_chunked(
                                &config, &filename, &content, "extra",
                            )
                            .await;
                        }
                    }
                });
            }
            static SESSION_INDEX_RUNNING: AtomicBool = AtomicBool::new(false);
            if SESSION_INDEX_RUNNING
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                let config = Arc::clone(&self.config_arc);
                spawn_logged("index_sessions", async move {
                    struct IndexGuard;
                    impl Drop for IndexGuard {
                        fn drop(&mut self) {
                            SESSION_INDEX_RUNNING.store(false, Ordering::Release);
                        }
                    }
                    let _guard = IndexGuard;
                    let _ = crate::session_indexer::index_pending_sessions(&config, 10).await;
                });
            }
        }
    }

    /// Check monthly token budget. Returns an error message if exceeded, a warning if near limit, or None.
    fn check_budget(&self) -> BudgetCheck {
        let budget_limit = self.config.budget.monthly_token_limit;
        if budget_limit == 0 {
            return BudgetCheck::Ok;
        }
        let Ok(db) = Database::open() else {
            warn!("Budget enforcement skipped: database unavailable");
            return BudgetCheck::Ok;
        };
        let Ok(used) = db.monthly_token_total() else {
            return BudgetCheck::Ok;
        };
        if used >= budget_limit {
            return BudgetCheck::Exceeded(format!(
                "Monthly token budget exceeded ({used}/{budget_limit}). \
                 Increase budget.monthly_token_limit in /settings to continue."
            ));
        }
        let threshold = self.config.budget.warning_threshold;
        let ratio = used as f64 / budget_limit as f64;
        if ratio >= 0.95 || ratio >= threshold {
            let pct = (ratio * 100.0) as u64;
            BudgetCheck::Warning(format!(
                "Warning: {pct}% of monthly token budget used ({used}/{budget_limit})"
            ))
        } else {
            BudgetCheck::Ok
        }
    }

    /// Core agent loop: stream LLM, execute tool calls, repeat until text-only response.
    pub async fn run_agent_loop(
        &mut self,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let max_iterations = self.config.conversation.max_iterations as usize;
        let mut iteration: usize = 0;
        let mut needs_response = false;
        let mut nudged_for_response = false;
        // One-shot reactive compaction per run — prevents a broken provider
        // (always returns context_overflow) from looping forever.
        let mut reactive_compacted_this_turn = false;

        // Reset identical-tool-call streak at the start of each user turn so
        // a fresh request starting with a previously-blocked call is not
        // immediately blocked again.
        self.tool_call_repeat.reset();

        self.prepare_loop().await;

        loop {
            self.maybe_reload_config();

            if cancel.is_cancelled() {
                self.shutdown_sub_agents();
                let _ = event_tx.send(AgentEvent::TurnComplete).await;
                return Ok(());
            }

            self.drain_sub_agent_completions();

            // Budget enforcement
            match self.check_budget() {
                BudgetCheck::Exceeded(msg) => {
                    let _ = event_tx.send(AgentEvent::Error(msg)).await;
                    let _ = event_tx.send(AgentEvent::TurnComplete).await;
                    return Ok(());
                }
                BudgetCheck::Warning(msg) => {
                    let _ = event_tx.send(AgentEvent::Error(msg)).await;
                }
                BudgetCheck::Ok => {}
            }

            iteration += 1;
            self.metrics.agent_iterations.add(1, &[]);
            if iteration > max_iterations {
                let _ = event_tx
                    .send(AgentEvent::Error(format!(
                        "Max iterations ({max_iterations}) reached — stopping agent loop"
                    )))
                    .await;
                let _ = event_tx.send(AgentEvent::TurnComplete).await;
                return Ok(());
            }

            // Signal TUI that we're preparing the next LLM call (iterations 2+)
            if iteration > 1 {
                let _ = event_tx.send(AgentEvent::Preparing).await;
            }

            self.compact_history_if_needed(&event_tx).await?;
            self.warm_skills_cache();

            let (messages, tool_defs) = match self.prepare_llm_request(iteration).await? {
                Some(r) => r,
                None => {
                    // Pre-flight hook (e.g. budget cap) refused this call.
                    // Surface a system-visible message and end the turn —
                    // mid-stream cancellation is forbidden by approvals.md,
                    // and we never started a stream, so this is the right
                    // place to halt.
                    let refusal = "Budget hard cap reached — agent paused. \
                                   Raise cap in /settings → Budgets, or wait for daily reset.";
                    self.finalize_text_response(refusal, &event_tx).await;
                    return Ok(());
                }
            };
            let tools = if tool_defs.is_empty() {
                None
            } else {
                Some(tool_defs.as_slice())
            };

            // --- Stream LLM response ---
            let llm_start = Instant::now();
            self.metrics.llm_requests.add(1, &[]);
            let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(256);
            let tools_clone = tools.map(<[ToolDefinition]>::to_vec);
            let cancel_clone = cancel.clone();
            let stream_handle = {
                let mut llm_client = self
                    .make_llm_client_from_config(&self.config)
                    .context("Failed to initialize LLM client")?
                    .with_prompt_cache_key(self.session.meta.id.clone());
                tokio::spawn(async move {
                    if let Err(e) = llm_client
                        .stream_chat_with_cancel(
                            &messages,
                            tools_clone.as_deref(),
                            stream_tx,
                            cancel_clone,
                        )
                        .await
                    {
                        warn!("LLM stream error: {e:#}");
                    }
                })
            };

            let mut tag_filter = InternalTagFilter::new();
            let mut tool_calls: Vec<PartialToolCall> = Vec::new();
            let mut received_terminal = false;
            let mut stream_error: Option<String> = None;
            let mut truncated_due_to_length = false;

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        let text_content = tag_filter.full_clean();
                        if !text_content.is_empty() {
                            let content = format!("{text_content}\n\n[response interrupted]");
                            self.log_and_persist(Message::assistant(&content));
                        }
                        let _ = event_tx.send(AgentEvent::TurnComplete).await;
                        if let Err(e) = stream_handle.await {
                            if e.is_panic() {
                                tracing::error!("LLM stream task panicked during cancel: {e}");
                            }
                        }
                        return Ok(());
                    }
                    event = stream_rx.recv() => {
                        let Some(event) = event else { break };
                        match self
                            .handle_stream_event(
                                event,
                                &mut tag_filter,
                                &mut tool_calls,
                                &mut stream_error,
                                &mut truncated_due_to_length,
                                &event_tx,
                            )
                            .await
                        {
                            StreamEventOutcome::Continue => {}
                            StreamEventOutcome::Terminal => {
                                received_terminal = true;
                                break;
                            }
                        }
                    }
                }
            }

            // If the stream channel closed without a terminal event and we have
            // no content, surface an error so the user isn't left with silence.
            if !received_terminal {
                warn!("LLM stream channel closed without Done or Error event");
                let text_content = tag_filter.full_clean();
                if text_content.is_empty() && tool_calls.is_empty() {
                    let _ = event_tx
                        .send(AgentEvent::Error(
                            "The response stream ended unexpectedly. Please try again.".into(),
                        ))
                        .await;
                }
            }

            if let Err(e) = stream_handle.await {
                if e.is_panic() {
                    tracing::error!("LLM stream task panicked: {e}");
                    let _ = event_tx
                        .send(AgentEvent::Error(
                            "Internal error: streaming task crashed".into(),
                        ))
                        .await;
                    // Without this TurnComplete the TUI spinner hangs forever:
                    // returning Err propagates up but the caller relies on the
                    // event channel to know the turn is over.
                    let _ = event_tx.send(AgentEvent::TurnComplete).await;
                    return Err(anyhow::anyhow!("LLM stream task panicked"));
                }
            }
            self.metrics
                .llm_duration
                .record(llm_start.elapsed().as_secs_f64(), &[]);

            // Reactive compaction: provider rejected the request because it
            // exceeded the context window. Compact once and retry this
            // iteration rather than surfacing the error. Tool_calls may be
            // mid-stream garbage from the failed call — discard them.
            if let Some(err_msg) = &stream_error {
                if !reactive_compacted_this_turn
                    && classify_error_text(err_msg) == Some(FailoverReason::ContextOverflow)
                {
                    reactive_compacted_this_turn = true;
                    warn!("Reactive compaction triggered by provider context overflow: {err_msg}");
                    self.reactive_compact_history(&event_tx).await?;
                    // Discard partial stream state (stream_error, tool_calls,
                    // tag_filter are re-bound at the top of the outer loop).
                    continue;
                }
            }

            match self
                .finish_iteration(
                    tag_filter,
                    tool_calls,
                    stream_error.take(),
                    truncated_due_to_length,
                    needs_response,
                    nudged_for_response,
                    &event_tx,
                    &cancel,
                )
                .await
            {
                IterationOutcome::EndTurn => return Ok(()),
                IterationOutcome::NudgeAndContinue => {
                    nudged_for_response = true;
                }
                IterationOutcome::ToolCallsExecuted => {
                    needs_response = true;
                }
            }
        }
    }

    /// Resolve a completed LLM stream into the next outer-loop step: persist a
    /// truncation marker on stream error, run text-only handling when there are
    /// no tool calls, or validate + execute tool calls. Returns the control
    /// signal for `run_agent_loop`.
    #[allow(clippy::too_many_arguments)]
    async fn finish_iteration(
        &mut self,
        mut tag_filter: InternalTagFilter,
        tool_calls: Vec<PartialToolCall>,
        mut stream_error: Option<String>,
        truncated_due_to_length: bool,
        needs_response: bool,
        nudged_for_response: bool,
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> IterationOutcome {
        // Flush any remaining buffered text from the internal-tag filter
        if let Some(remaining) = tag_filter.flush() {
            let redacted = self.maybe_redact(remaining);
            let _ = event_tx.send(AgentEvent::TextDelta(redacted)).await;
        }
        let text_content = tag_filter.full_clean();

        // Fire AfterLlmResponse hook
        let hook_ctx = self.hook_ctx(
            HookPoint::AfterLlmResponse,
            HookData::LlmResponse {
                has_tool_calls: !tool_calls.is_empty(),
                text_length: text_content.len(),
            },
        );
        self.hook_registry.dispatch(&hook_ctx);

        // If the LLM stream errored mid-response, treat the turn as failed
        // regardless of whether partial tool calls survived — their JSON
        // arguments may be truncated and executing them risks running with
        // half-parsed input. Persist the interrupted marker and emit
        // TurnComplete so the TUI returns to the composer.
        if stream_error.is_some() {
            self.persist_discarded_tool_calls(&text_content, stream_error.take(), event_tx)
                .await;
            return IterationOutcome::EndTurn;
        }

        if tool_calls.is_empty() {
            // Even with no tool calls, the assistant text itself may have
            // been cut mid-sentence. Treat max-tokens cut-off as a stream
            // error so the existing `[stream interrupted: …]` machinery
            // persists a marker — the next turn / replay can tell a
            // truncated answer from a complete one.
            let effective_err = stream_error.take().or_else(|| {
                truncated_due_to_length.then(|| {
                    "response truncated at max_tokens (raise llm.max_tokens \
                     or ask the assistant to continue)"
                        .to_string()
                })
            });
            return match self
                .handle_text_only_response(
                    &text_content,
                    effective_err,
                    needs_response,
                    nudged_for_response,
                    event_tx,
                )
                .await
            {
                TextOnlyOutcome::TerminateTurn => IterationOutcome::EndTurn,
                TextOnlyOutcome::NudgeAndContinue => IterationOutcome::NudgeAndContinue,
            };
        }

        let (tc, invalid) = partition_tool_calls(&tool_calls, truncated_due_to_length);

        if tc.is_empty() && invalid.is_empty() {
            self.persist_discarded_tool_calls(&text_content, stream_error.take(), event_tx)
                .await;
            return IterationOutcome::EndTurn;
        }

        // Track tool names for conditional tool loading (recent usage keeps groups active).
        for t in &tc {
            self.recent_tool_names.insert(t.function.name.clone());
        }

        self.execute_tool_calls(&tc, &invalid, &text_content, event_tx, cancel)
            .await;

        // Drain any steer messages from the user at the tool boundary
        self.drain_steers(event_tx).await;

        IterationOutcome::ToolCallsExecuted
    }

    /// Account for one `StreamEvent::Usage`: increment counters and persist to the
    /// token-usage table. Extracted from `run_agent_loop` to keep the outer future's
    /// frame small (the inner DB lock + pricing call captures a wide scope).
    fn record_usage(&self, usage: &UsageData) {
        let total = usage.prompt_tokens + usage.completion_tokens;
        if total == 0 {
            return;
        }
        self.metrics.llm_tokens.add(total, &[]);
        let guard = self.db_guard();
        let Some(ref db) = *guard else { return };
        let cost = crate::pricing::estimate_cost(
            &usage.model,
            usage.prompt_tokens,
            usage.completion_tokens,
        );
        if let Err(e) = db.log_token_usage_with_cache(
            usage.prompt_tokens,
            usage.completion_tokens,
            total,
            usage.cached_input_tokens,
            usage.cache_creation_tokens,
            &usage.provider,
            &usage.model,
            cost,
        ) {
            warn!("Failed to log token usage: {e}");
        }

        // Budget post-flight reconcile + provider_call event. Price using
        // the rate card row in effect at call time so a future replay of
        // the audit chain produces the same USD total. If no rate row
        // exists for this (provider, model), warn and skip — recording an
        // f64-derived USD value here would seed the audit chain with a
        // value that drifts on replay. The fix is to seed the rate card
        // (`crates/core/src/pricing.rs::bundled_rate_card_entries` for a
        // built-in, or `borg providers refresh-rates` once verification
        // ships).
        let now = chrono::Utc::now().timestamp();
        let provider_id = usage.provider.to_ascii_lowercase();
        let (actual_usd, rate_card_effective_at) =
            match crate::budgets::RateCard::lookup(db.conn(), &provider_id, &usage.model, now) {
                Ok(Some(row)) => {
                    let priced = crate::budgets::RateCard::price(
                        &row,
                        usage.prompt_tokens,
                        usage.completion_tokens,
                    );
                    (priced, Some(row.effective_at))
                }
                Ok(None) => {
                    // Per-turn warning was a major contributor to log spam for
                    // any session on a model without a rate card. Promote to
                    // WARN once per (provider, model); otherwise stay at DEBUG.
                    // Cap the dedupe set so a misbehaving provider returning a
                    // fresh `usage.model` per turn can't grow it without bound.
                    const MAX_TRACKED: usize = 1024;
                    static WARNED: std::sync::OnceLock<
                        std::sync::Mutex<std::collections::HashSet<(String, String)>>,
                    > = std::sync::OnceLock::new();
                    let key = (provider_id.clone(), usage.model.clone());
                    let first_time = WARNED
                        .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
                        .lock()
                        .map(|mut set| {
                            if set.len() >= MAX_TRACKED {
                                false
                            } else {
                                set.insert(key.clone())
                            }
                        })
                        .unwrap_or(false);
                    if first_time {
                        debug!(
                            provider = %provider_id,
                            model = %usage.model,
                            legacy_cost_estimate = ?cost,
                            "No rate card for provider/model; skipping budget reconcile \
                             (token usage still logged). Subsequent occurrences are silent."
                        );
                    } else {
                        tracing::trace!(
                            provider = %provider_id,
                            model = %usage.model,
                            "No rate card; skipping budget reconcile"
                        );
                    }
                    return;
                }
                Err(e) => {
                    warn!(
                        "Rate-card lookup failed for {provider_id}/{}: {e}",
                        usage.model
                    );
                    return;
                }
            };
        if let Err(e) = crate::budgets::BudgetEngine::post_flight(
            db.conn(),
            None, // principal_id wiring is v3
            &provider_id,
            &usage.model,
            rate_card_effective_at,
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.cached_input_tokens,
            actual_usd,
            Some(self.session.meta.id.as_str()),
        ) {
            warn!("Budget post-flight failed: {e}");
        }
    }

    /// Hot-reload config if the watcher has detected changes.
    fn maybe_reload_config(&mut self) {
        if let Some(ref mut rx) = self.config_rx {
            if rx.has_changed().unwrap_or(false) {
                let new_config = rx.borrow_and_update().clone();
                info_span!("config_reload").in_scope(|| {
                    // Routine event, not an error condition — DEBUG is the
                    // right level. WARN here used to fire on the first turn of
                    // every session and looked like a real warning to operators.
                    debug!("Config reloaded from disk");
                });
                self.reload_config(new_config);
            }
        }
    }

    /// Shut down all sub-agents (called on cancellation).
    fn shutdown_sub_agents(&mut self) {
        if let Some(ref mut ctrl) = self.agent_control {
            ctrl.shutdown_all();
        }
    }

    /// Collect completed sub-agent results and persist them as tool result messages.
    fn drain_sub_agent_completions(&mut self) {
        if let Some(ref mut ctrl) = self.agent_control {
            for completion in ctrl.drain_completions() {
                let ctx = format!(
                    "[Sub-agent \"{}\" (id: {}) status: {}]\n{}",
                    completion.nickname,
                    completion.agent_id,
                    completion.status.as_str(),
                    completion
                        .final_response
                        .as_deref()
                        .unwrap_or("(no output)")
                );
                self.persist_message(Message::tool_result("sub-agent", &ctx));
            }
        }
    }

    /// Force a compaction pass now, bypassing the proactive token-budget gate.
    /// Used by the reactive-recovery path when a provider returns
    /// `ContextOverflow` mid-turn: the agent compacts history, then retries
    /// the same iteration. Target is 60% of `max_history_tokens` so we leave
    /// meaningful headroom for the retry to succeed.
    async fn reactive_compact_history(
        &mut self,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
        normalize_history(&mut self.history);

        let max_hist = self.config.conversation.max_history_tokens;
        let target = (max_hist as f64 * 0.6) as usize;
        let before_tokens = history_tokens(&self.history);
        let pre_len = self.history.len();

        // Try cheap tool-result pruning first — often enough.
        compact_tool_results(&mut self.history, target);

        if history_tokens(&self.history) <= target {
            let dropped = pre_len.saturating_sub(self.history.len());
            if dropped > 0 {
                let _ = event_tx
                    .send(AgentEvent::HistoryCompacted {
                        dropped,
                        before_tokens,
                        after_tokens: history_tokens(&self.history),
                        iterative: false,
                    })
                    .await;
            }
            return Ok(());
        }

        let compaction_config = self.config.with_compaction_overrides();
        let compaction_llm = self.make_llm_client_from_config(&compaction_config)?;
        let protect_first_n = self.config.conversation.protect_first_n;

        if self.previous_summary.is_none() {
            self.previous_summary =
                crate::conversation::extract_last_compaction_summary(&self.history);
        }
        let iterative = self.previous_summary.is_some();

        let mut dropped = if protect_first_n > 0 {
            crate::conversation::compact_history_v2(
                &mut self.history,
                target,
                protect_first_n,
                &mut self.previous_summary,
                &compaction_llm,
            )
            .await
        } else {
            let before_len = self.history.len();
            compact_history(&mut self.history, target, &compaction_llm).await;
            before_len.saturating_sub(self.history.len())
        };

        // If v2 made no progress (protected head alone exceeds target), fall
        // back to non-head-protected so we don't loop on the same overflow.
        if dropped == 0 && history_tokens(&self.history) > target {
            let before_len = self.history.len();
            compact_history(&mut self.history, target, &compaction_llm).await;
            dropped = before_len.saturating_sub(self.history.len());
        }

        if dropped > 0 {
            let _ = event_tx
                .send(AgentEvent::HistoryCompacted {
                    dropped,
                    before_tokens,
                    after_tokens: history_tokens(&self.history),
                    iterative,
                })
                .await;
        } else {
            // Compaction freed nothing — protected head alone exceeds budget,
            // or every message is structurally undroppable. The caller will
            // retry the LLM stream once (`reactive_compacted_this_turn` is
            // already set), and that retry will re-fail with the same
            // context-overflow error. Surface an actionable message *now* so
            // the user sees the cause rather than just the generic
            // `[stream interrupted: context_length_exceeded]` marker that
            // follows. See OpenClaw failure mode for context.
            warn!("reactive_compact_history: nothing dropped, retry will likely re-fail");
            let after_tokens = history_tokens(&self.history);
            let _ = event_tx
                .send(AgentEvent::Error(format!(
                    "Context overflow and history compaction freed nothing \
                     ({after_tokens} tokens, target {target}). \
                     Start a new session or raise `conversation.max_history_tokens`."
                )))
                .await;
        }

        Ok(())
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
    async fn execute_tool_calls(
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
        };
        self.log_and_persist(assistant_msg);

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
                })
                .await;
            self.log_and_persist(Message::tool_result(&inv.id, &body));
        }

        // Plan concurrent tool groups. The planner guarantees every group
        // either (a) contains a single Unknown-effect tool or (b) contains
        // only parallel-safe tools with no conflicting file-path writes.
        let concurrent_cfg = &self.config.conversation.concurrent_tools;
        let max_workers = concurrent_cfg.max_workers.max(1);
        let parallel_enabled = concurrent_cfg.enabled && max_workers > 1;

        let groups = crate::tool::effects::plan_groups(tc);
        for group in groups {
            if cancel.is_cancelled() {
                // Emit cancellation markers for the remaining calls so the LLM
                // sees a tool_result for every tool_call it issued.
                for idx in group {
                    self.skip_tool_call(&tc[idx].id, "[tool call cancelled by user]");
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

    /// Drain pending steer messages from the user and inject them into history.
    #[instrument(skip_all)]
    async fn drain_steers(&mut self, event_tx: &mpsc::Sender<AgentEvent>) {
        // Collect all steers first to avoid borrow conflict with self.persist_message
        let steers: Vec<String> = if let Some(ref mut steer_rx) = self.steer_rx {
            let mut collected = Vec::new();
            while let Ok(text) = steer_rx.try_recv() {
                collected.push(text);
            }
            collected
        } else {
            return;
        };

        for steer_text in steers {
            self.persist_message(Message::user(&steer_text));
            let _ = event_tx
                .send(AgentEvent::SteerReceived { text: steer_text })
                .await;
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
                self.skip_tool_call(&tool_call.id, "[tool call cancelled by user]");
                continue;
            }

            let name = &tool_call.function.name;
            let args = &tool_call.function.arguments;

            // Fire BeforeToolCall hook
            let hook_ctx = self.hook_ctx(
                HookPoint::BeforeToolCall,
                HookData::ToolCall {
                    name: name.clone(),
                    args: args.clone(),
                },
            );
            if matches!(self.hook_registry.dispatch(&hook_ctx), HookAction::Skip) {
                self.skip_tool_call(&tool_call.id, "[tool call skipped by hook]");
                continue;
            }

            // Plan mode: block mutating tool calls
            if self
                .config
                .conversation
                .collaboration_mode
                .blocks_mutations()
                && is_mutating_tool(name)
            {
                self.skip_tool_call(
                    &tool_call.id,
                    "Plan mode: mutating operations are not allowed. Use read-only tools (read_file, list_dir, list, memory_search, web_fetch, web_search) to explore the codebase and formulate your plan.",
                );
                continue;
            }

            // Identical-tool-call loop guard: must run BEFORE rate-guard so a
            // blocked repeat does not consume rate-guard budget.
            if self.tool_call_repeat.observe(name, args) {
                let count = self.tool_call_repeat.consecutive_count;
                warn!("Repeated identical tool call blocked: '{name}' x{count}");
                self.metrics.tool_repeats_blocked.add(1, &[]);
                self.skip_tool_call(
                    &tool_call.id,
                    &repeated_tool_call_block_message(name, count),
                );
                continue;
            }

            // Rate limiting
            let action_type = classify_action(name);
            match self.rate_guard.record(action_type) {
                RateDecision::Block(reason) => {
                    warn!("Rate limit blocked tool call '{name}': {reason}");
                    self.skip_tool_call(&tool_call.id, &format!("Error: {reason}"));
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
                    args: args.clone(),
                })
                .await;

            let tool_start = Instant::now();
            let tool_output = self
                .execute_tool(name, args, event_tx, &tool_call.id)
                .instrument(info_span!("tool.execute", tool.name = %name))
                .await
                .unwrap_or_else(|e| ToolOutput::Text(format!("Error: {e}")));
            let tool_elapsed = tool_start.elapsed().as_secs_f64();
            self.metrics.tool_executions.add(1, &[]);
            self.metrics.tool_duration.record(tool_elapsed, &[]);

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
            );
            let _ = event_tx
                .send(AgentEvent::ToolResult {
                    tool_call_id: tool_call.id.clone(),
                    name: name.clone(),
                    result: redacted,
                })
                .await;
            let msg = if let Some(parts) = extra_parts {
                let mut msg_parts = vec![ContentPart::Text(xml)];
                msg_parts.extend(
                    parts
                        .into_iter()
                        .filter(|p| !matches!(p, ContentPart::Text(_))),
                );
                Message::tool_result_multimodal(&tool_call.id, msg_parts)
            } else {
                Message::tool_result(&tool_call.id, &xml)
            };
            self.log_and_persist(msg);
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
                    reason: "[tool call cancelled by user]".into(),
                });
                continue;
            }

            let name = tool_call.function.name.clone();
            let args = tool_call.function.arguments.clone();

            let hook_ctx = self.hook_ctx(
                HookPoint::BeforeToolCall,
                HookData::ToolCall {
                    name: name.clone(),
                    args: args.clone(),
                },
            );
            if matches!(self.hook_registry.dispatch(&hook_ctx), HookAction::Skip) {
                slots.push(Slot::Skipped {
                    reason: "[tool call skipped by hook]".into(),
                });
                continue;
            }

            if self
                .config
                .conversation
                .collaboration_mode
                .blocks_mutations()
                && is_mutating_tool(&name)
            {
                slots.push(Slot::Skipped {
                    reason: "Plan mode: mutating operations are not allowed. Use read-only tools (read_file, list_dir, list, memory_search, web_fetch, web_search) to explore the codebase and formulate your plan.".into(),
                });
                continue;
            }

            // Identical-tool-call loop guard — must precede rate-guard so a
            // blocked repeat does not consume rate-guard budget.
            if self.tool_call_repeat.observe(&name, &args) {
                let count = self.tool_call_repeat.consecutive_count;
                warn!("Repeated identical tool call blocked: '{name}' x{count}");
                self.metrics.tool_repeats_blocked.add(1, &[]);
                slots.push(Slot::Skipped {
                    reason: repeated_tool_call_block_message(&name, count),
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
                let config = std::sync::Arc::clone(&self.config_arc);
                let execution_context = self.execution_context.clone();
                let tx = event_tx.clone();
                let permits = std::sync::Arc::clone(&permits);
                let name_clone = name.clone();
                let args_clone = args.clone();
                join_set.spawn(async move {
                    let _permit = permits.acquire_owned().await.ok();
                    let start = Instant::now();
                    let result = dispatch_parallel_safe_tool(
                        &name_clone,
                        &args_clone,
                        config.as_ref(),
                        &tx,
                        Some(&execution_context),
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

        // Single in-order pass: emit ToolExecuting, fire hooks, and persist
        // results in the LLM's original call order. Skip slots and completed
        // slots are interleaved at their original positions.
        for (idx, slot) in slots.into_iter().enumerate() {
            let tool_call = tool_calls[idx];
            let (name, elapsed, result, args_for_event) = match slot {
                Slot::Skipped { reason } => {
                    self.skip_tool_call(&tool_call.id, &reason);
                    continue;
                }
                Slot::Approved { name: _, args } => match results[idx].take() {
                    Some((nm, el, res)) => (nm, el, res, args),
                    None => {
                        // Worker panicked — surface a placeholder error so the
                        // LLM still sees a tool_result for this call.
                        self.skip_tool_call(&tool_call.id, "[tool call failed — worker panicked]");
                        continue;
                    }
                },
            };

            let _ = event_tx
                .send(AgentEvent::ToolExecuting {
                    tool_call_id: tool_call.id.clone(),
                    name: name.clone(),
                    args: args_for_event,
                })
                .await;

            let tool_output = result.unwrap_or_else(|e| ToolOutput::Text(format!("Error: {e}")));
            self.metrics.tool_executions.add(1, &[]);
            self.metrics.tool_duration.record(elapsed, &[]);

            let safe_name = crate::security::sanitize::escape_xml_attr(&name);
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
                &name,
                &xml,
            );
            let _ = event_tx
                .send(AgentEvent::ToolResult {
                    tool_call_id: tool_call.id.clone(),
                    name: name.clone(),
                    result: redacted,
                })
                .await;
            let msg = if let Some(parts) = extra_parts {
                let mut msg_parts = vec![ContentPart::Text(xml)];
                msg_parts.extend(
                    parts
                        .into_iter()
                        .filter(|p| !matches!(p, ContentPart::Text(_))),
                );
                Message::tool_result_multimodal(&tool_call.id, msg_parts)
            } else {
                Message::tool_result(&tool_call.id, &xml)
            };
            self.log_and_persist(msg);
        }
    }

    #[instrument(skip_all, fields(tool.name = %name))]
    async fn execute_tool(
        &mut self,
        name: &str,
        args_json: &str,
        event_tx: &mpsc::Sender<AgentEvent>,
        tool_call_id: &str,
    ) -> Result<ToolOutput> {
        let raw_args: serde_json::Value = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolOutput::Text(format!(
                    "Error: Invalid JSON arguments: {e}. Please provide valid JSON."
                )));
            }
        };

        // Single tagged-deser entry point. Schema-validation errors surface
        // here as structured tool output so the LLM can recover; the dispatch
        // arms below see only typed `Args`.
        let tool = match crate::tool::Tool::parse(name, raw_args.clone()) {
            Ok(t) => t,
            Err(crate::tool::ToolParseError::UnknownTool(n)) => {
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
            _ => {}
        }

        // Variants returning Result<String> wrap to ToolOutput::Text below.
        let text_result: Result<String> = match &tool {
            crate::tool::Tool::WriteMemory(args) => crate::tool::write_memory::handle(
                args,
                &self.config,
                &self.config_arc,
                &mut self.caches,
            ),
            crate::tool::Tool::ReadMemory(args) => crate::tool::read_memory::handle(args),
            crate::tool::Tool::MemorySearch(args) => {
                crate::tool::memory_search::handle(args, &self.config).await
            }
            crate::tool::Tool::List(args) => {
                crate::tool::list::handle(args, &self.config, self.agent_control.as_ref())
            }
            crate::tool::Tool::ListSkills => crate::tool::list::handle_list_skills(&self.config),
            crate::tool::Tool::ListChannels => crate::tool::list::handle_list_channels(&self.config),
            crate::tool::Tool::ApplyPatch(args) => {
                crate::tool::apply_patch::handle_apply_patch_with_context(
                    args,
                    &self.config,
                    &self.execution_context,
                )
            }
            crate::tool::Tool::ApplySkillPatch(args) => {
                crate::tool::apply_patch::handle_apply_skill_patch(args)
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
                crate::tool::schedule::handle_schedule(args, &self.config)
            }
            crate::tool::Tool::ManageTasks(args) => {
                crate::tool::schedule::handle_manage_tasks(args, &self.config)
            }
            crate::tool::Tool::ManageCron(args) => {
                crate::tool::schedule::handle_manage_cron(args, &self.config)
            }
            crate::tool::Tool::GenerateImage(args) => {
                crate::tool::generate_image::handle(args, &self.config).await
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
                crate::tool::multi_agent::handle_send_to_agent(args)
            }
            crate::tool::Tool::WaitForAgent(args) => {
                crate::tool::multi_agent::handle_wait_for_agent(args, &mut self.agent_control).await
            }
            crate::tool::Tool::ListAgents(_) => {
                crate::tool::multi_agent::handle_list_agents(self.agent_control.as_ref())
            }
            crate::tool::Tool::CloseAgent(args) => {
                crate::tool::multi_agent::handle_close_agent(args, &mut self.agent_control)
            }
            crate::tool::Tool::ManageRoles(args) => {
                crate::tool::multi_agent::handle_manage_roles(args)
            }
            // Variants handled in the early-return block above.
            crate::tool::Tool::ReadFile(_)
            | crate::tool::Tool::Browser(_)
            | crate::tool::Tool::TextToSpeech(_)
            | crate::tool::Tool::RequestUserInput(_) => unreachable!(
                "ReadFile/Browser/TextToSpeech/RequestUserInput are handled in the early-return match above"
            ),
        };
        let text = text_result?;
        self.maybe_refresh_skills_after_tool(name, &raw_args, &text);
        Ok(ToolOutput::Text(text))
    }

    /// Close the browser session if active.
    #[instrument(skip_all)]
    /// Close the headless browser session if one is open.
    pub async fn close_browser(&mut self) {
        if let Some(session) = self.browser_session.take() {
            let _ = session.close().await;
        }
    }

    /// Close the database connection, releasing all file locks.
    /// Call before deleting the data directory (e.g. during uninstall).
    pub fn close_db(&self) {
        let mut guard = self.db_guard();
        if guard.take().is_some() {
            tracing::info!("Database connection closed for uninstall");
        }
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
async fn dispatch_parallel_safe_tool(
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
    let raw_args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(e) => {
            return Ok(ToolOutput::Text(format!(
                "Error: Invalid JSON arguments: {e}. Please provide valid JSON."
            )));
        }
    };
    let tool = match crate::tool::Tool::parse(name, raw_args) {
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
        crate::tool::Tool::ReadMemory(args) => {
            crate::tool::read_memory::handle(args).map(ToolOutput::Text)
        }
        crate::tool::Tool::MemorySearch(args) => crate::tool::memory_search::handle(args, config)
            .await
            .map(ToolOutput::Text),
        crate::tool::Tool::ListSkills => {
            crate::tool::list::handle_list_skills(config).map(ToolOutput::Text)
        }
        crate::tool::Tool::ListChannels => {
            crate::tool::list::handle_list_channels(config).map(ToolOutput::Text)
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
            crate::tool::apply_patch::handle_apply_skill_patch(args).map(ToolOutput::Text)
        }
        // Anything still reaching here despite `parallel_safe()` returning true
        // is a closed-set inconsistency — fail loudly so tests catch it.
        other => Err(anyhow::anyhow!(
            "dispatch_parallel_safe_tool: variant '{}' is parallel_safe() but lacks a handler arm",
            other.name()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rate_guard::ActionType;

    // ── Prompt cache: stable prefix ordering ──
    //
    // `build_system_prompt` must emit its stable (cache-friendly) sections
    // BEFORE the dynamic environment section, otherwise per-turn content
    // (time, git status) invalidates the cached prefix. These guard tests
    // read the source of `build_system_prompt` and fail if the order is
    // perturbed. The full end-to-end behavior is exercised indirectly
    // through the Anthropic request tests in `llm.rs`.

    const AGENT_RS_SRC: &str = include_str!("mod.rs");

    fn extract_build_system_prompt_body() -> &'static str {
        let start = AGENT_RS_SRC
            .find("async fn build_system_prompt")
            .expect("build_system_prompt must exist");
        // Extract ~6KB following the signature — plenty to cover the body.
        let end = (start + 6000).min(AGENT_RS_SRC.len());
        &AGENT_RS_SRC[start..end]
    }

    #[test]
    fn build_system_prompt_environment_is_after_security_policy() {
        let body = extract_build_system_prompt_body();
        let security_idx = body
            .find("security_policy")
            .expect("security_policy section must exist in build_system_prompt");
        let env_idx = body
            .find("build_environment_section")
            .expect("build_environment_section call must exist in build_system_prompt");
        assert!(
            env_idx > security_idx,
            "build_environment_section must be pushed AFTER security_policy \
             to keep the cached prefix stable across turns \
             (env_idx={env_idx}, security_idx={security_idx})",
        );
    }

    #[test]
    fn build_system_prompt_environment_is_after_identity() {
        let body = extract_build_system_prompt_body();
        let identity_idx = body
            .find("system_instructions")
            .expect("system_instructions (identity) section must exist");
        let env_idx = body
            .find("build_environment_section")
            .expect("build_environment_section call must exist");
        assert!(
            env_idx > identity_idx,
            "dynamic environment section must come after identity",
        );
    }

    #[test]
    fn build_system_prompt_environment_is_after_tooling() {
        let body = extract_build_system_prompt_body();
        let tooling_idx = body
            .find("build_tooling_section")
            .expect("build_tooling_section call must exist");
        let env_idx = body
            .find("build_environment_section")
            .expect("build_environment_section call must exist");
        assert!(
            env_idx > tooling_idx,
            "dynamic environment section must come after tooling",
        );
    }

    #[test]
    fn build_system_prompt_active_memory_in_dynamic_suffix() {
        // Cache-stability invariant: <active_memory> changes per turn (its
        // content is keyed off the latest user message), so it MUST live in
        // the DYNAMIC SUFFIX block — never before, where it would invalidate
        // the prefix cache on every turn.
        let body = extract_build_system_prompt_body();
        let suffix_marker = body
            .find("DYNAMIC SUFFIX")
            .expect("DYNAMIC SUFFIX marker must exist");
        let active_idx = body
            .find("build_active_memory_section")
            .expect("build_active_memory_section call must exist in build_system_prompt");
        assert!(
            active_idx > suffix_marker,
            "active_memory must be pushed AFTER the DYNAMIC SUFFIX marker \
             so it does not invalidate the prefix cache \
             (active_idx={active_idx}, suffix_marker={suffix_marker})",
        );
    }

    // ── Stream error-recovery guards ──
    //
    // `run_agent_loop` preserves partial assistant text *and* includes the
    // upstream error reason in the persisted message when the LLM stream
    // errors mid-response, so a resumed/replayed conversation can see that
    // the turn was cut short rather than appearing as a silent completion.
    // These guards assert the source-level structure so regressions surface
    // in CI instead of in a support ticket six months later.

    #[test]
    fn run_agent_loop_captures_stream_error_reason() {
        assert!(
            AGENT_RS_SRC.contains("let mut stream_error: Option<String> = None;"),
            "run_agent_loop must declare a `stream_error` local — the handler for \
             StreamEvent::Error stores the reason there so the persisted message \
             can name the failure"
        );
        assert!(
            AGENT_RS_SRC.contains("stream_error = Some(e.clone());"),
            "StreamEvent::Error arm must clone the error into `stream_error` before \
             forwarding to the TUI; otherwise the error string is lost by the time \
             we persist the partial message"
        );
    }

    #[test]
    fn run_agent_loop_diverts_on_stream_error_before_tool_exec() {
        // Invariant: when the LLM stream errors mid-response, the loop MUST NOT
        // call execute_tool_calls — partial tool-call JSON may be truncated.
        // Without this divert, a 429 / network blip that happened to flush
        // partial tool deltas hangs the TUI: the loop executes garbage tools
        // and re-loops without ever emitting TurnComplete. The fix is a
        // `stream_error.is_some()` early-return to persist_discarded_tool_calls
        // *before* the tool_calls / validate_tool_calls branches.
        let exec_idx = AGENT_RS_SRC
            .find("self.execute_tool_calls(")
            .expect("execute_tool_calls call must exist in run_agent_loop");
        let preceding = &AGENT_RS_SRC[..exec_idx];
        let divert_idx = preceding
            .rfind("if stream_error.is_some()")
            .expect("run_agent_loop must short-circuit to persist_discarded_tool_calls when stream_error.is_some() — before reaching execute_tool_calls — or the TUI hangs on 429+partial-tool-call");
        let divert_tail = &AGENT_RS_SRC[divert_idx..exec_idx];
        assert!(
            divert_tail.contains("persist_discarded_tool_calls"),
            "the stream_error divert must call persist_discarded_tool_calls (which emits TurnComplete) before any tool execution"
        );
    }

    #[test]
    fn run_agent_loop_emits_turn_complete_on_stream_panic() {
        // Invariant: the LLM-stream-task panic branch must emit TurnComplete
        // before returning Err. The function's caller treats Err as a hard
        // failure and propagates it up, but the TUI runtime listens on the
        // event channel — without this TurnComplete the spinner hangs even
        // though the agent has given up.
        let panic_idx = AGENT_RS_SRC
            .find("LLM stream task panicked: ")
            .expect("panic-branch tracing::error message must exist");
        let panic_tail = &AGENT_RS_SRC[panic_idx..];
        let return_idx = panic_tail
            .find("return Err(anyhow::anyhow!(\"LLM stream task panicked\"))")
            .expect("panic branch must return Err");
        let between = &panic_tail[..return_idx];
        assert!(
            between.contains("AgentEvent::TurnComplete"),
            "panic branch must send AgentEvent::TurnComplete before returning Err"
        );
    }

    #[test]
    fn run_agent_loop_persists_interruption_marker() {
        assert!(
            AGENT_RS_SRC.contains("[stream interrupted:"),
            "run_agent_loop must embed a `[stream interrupted: …]` marker in the \
             persisted assistant message when a mid-stream error happens, so the \
             next turn (and any later conversation replay) can tell a truncated \
             response from a completed one"
        );
    }

    // ── Autocompaction trigger wiring ──
    //
    // Regression guard for "ctx 100% but autocompaction never ran." The fix
    // for the user-visible bug was in the TUI percent calc, but the agent
    // wiring is what makes compaction actually fire — if the call site at
    // run_agent_loop is dropped, history grows unbounded until an LLM error
    // and the user has no way to know why. Source-grep guard, since
    // exercising the full Agent constructor in a unit test is too expensive.

    #[test]
    fn run_agent_loop_calls_compact_history_if_needed_before_each_request() {
        // The dispatch must happen INSIDE the per-iteration loop and BEFORE
        // prepare_llm_request — otherwise an over-budget history hits the
        // LLM unmodified.
        let compact_idx = AGENT_RS_SRC
            .find("self.compact_history_if_needed(&event_tx).await?;")
            .expect(
                "run_agent_loop must call self.compact_history_if_needed(&event_tx).await?; \
                 dropping this call lets history grow past max_history_tokens until the LLM \
                 returns a context-length error, which the user perceives as a hang",
            );
        let prepare_idx = AGENT_RS_SRC
            .find("self.prepare_llm_request(iteration)")
            .expect("prepare_llm_request call must exist in run_agent_loop");
        assert!(
            compact_idx < prepare_idx,
            "compact_history_if_needed must run BEFORE prepare_llm_request \
             (compact_idx={compact_idx}, prepare_idx={prepare_idx})",
        );
    }

    #[test]
    fn require_str_param_present() {
        let args = serde_json::json!({"name": "hello"});
        let val = tool_handlers::require_str_param(&args, "name").unwrap();
        assert_eq!(val, "hello");
    }

    #[test]
    fn require_str_param_missing() {
        let args = serde_json::json!({"other": "value"});
        let err = tool_handlers::require_str_param(&args, "name").unwrap_err();
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn require_str_param_null() {
        let args = serde_json::json!({"name": null});
        assert!(tool_handlers::require_str_param(&args, "name").is_err());
    }

    #[test]
    fn require_str_param_wrong_type() {
        let args = serde_json::json!({"name": 42});
        assert!(tool_handlers::require_str_param(&args, "name").is_err());
    }

    #[test]
    fn update_task_status_not_found() {
        let id = "test-nonexistent-00000000";
        let result = tool_handlers::update_task_status(id, "paused", "paused").unwrap();
        // `with_db` uses `Database::open()` (real filesystem DB) which may be locked
        // during parallel test execution. Only assert when we got a real response.
        if result.contains("Error opening database") || result.contains("database is locked") {
            return; // DB contention — cannot test logic, skip
        }
        assert!(
            result.contains("not found"),
            "expected 'not found' in: {result}"
        );
    }

    #[test]
    fn update_task_status_formats_verb() {
        let id = "test-nonexistent-00000001";
        let result = tool_handlers::update_task_status(id, "cancelled", "cancelled").unwrap();
        if result.contains("Error opening database") || result.contains("database is locked") {
            return; // DB contention — cannot test logic, skip
        }
        assert!(result.contains(id));
    }

    // ── classify_action tests ──

    #[test]
    fn classify_action_known_and_unknown_tools() {
        // classify_action feeds the rate-limiter and vitals events. If the
        // mapping regresses, a shell command could be counted as a generic
        // tool call and bypass the shell-specific rate bucket.
        let cases: &[(&str, ActionType)] = &[
            ("run_shell", ActionType::ShellCommand),
            ("apply_patch", ActionType::FileWrite),
            ("write_memory", ActionType::MemoryWrite),
            ("web_fetch", ActionType::WebRequest),
            // Unknowns must fall through to ToolCall (safe default).
            ("some_unknown_tool", ActionType::ToolCall),
            ("", ActionType::ToolCall),
        ];
        for (tool, expected) in cases {
            let got = classify_action(tool);
            assert!(
                std::mem::discriminant(&got) == std::mem::discriminant(expected),
                "classify_action({tool:?}) = {got:?}, expected {expected:?}"
            );
        }
    }

    // -- is_mutating_tool (allowlist-based) --

    #[test]
    fn mutating_tools_are_blocked_in_plan_mode() {
        // Plan mode security boundary: these tools must be blocked. If a new
        // mutating tool is added and someone forgets to tag it mutating, plan
        // mode turns into execute mode silently.
        assert!(is_mutating_tool("apply_patch"));
        assert!(is_mutating_tool("apply_skill_patch"));
        assert!(is_mutating_tool("create_channel"));
        assert!(is_mutating_tool("run_shell"));
        assert!(is_mutating_tool("write_memory"));
        assert!(is_mutating_tool("browser"));
        assert!(is_mutating_tool("schedule"));
        assert!(is_mutating_tool("generate_image"));
    }

    #[test]
    fn non_mutating_tools_allowed_in_plan_mode() {
        // These should be allowed (non-mutating)
        assert!(!is_mutating_tool("read_file"));
        assert!(!is_mutating_tool("list_dir"));
        assert!(!is_mutating_tool("list"));
        assert!(!is_mutating_tool("read_memory"));
        assert!(!is_mutating_tool("memory_search"));
        assert!(!is_mutating_tool("web_fetch"));
        assert!(!is_mutating_tool("web_search"));
        // request_user_input is now allowlisted: clarification questions don't
        // mutate state and are essential to Plan-mode requirement gathering.
        assert!(!is_mutating_tool("request_user_input"));
    }

    #[test]
    fn skill_tool_write_refreshes_cached_skill_state() {
        let config = Config::default();
        let session = Session::new();
        let session_id = session.meta.id.clone();
        let mut agent = Agent {
            config: config.clone(),
            history: Vec::new(),
            session,
            policy: config.policy.clone(),
            hook_registry: HookRegistry::new(),
            turn_count: 0,
            rate_guard: SessionRateGuard::new(config.security.action_limits.clone()),
            agent_control: None,
            spawn_depth: 0,
            tools_filter: None,
            skill_env_allowlist: std::collections::HashSet::new(),
            metrics: BorgMetrics::default(),
            browser_session: None,
            tts_synthesizer: None,
            config_rx: None,
            db: Mutex::new(None),
            caches: SystemPromptCache::with_test_state(
                None,
                Some(HashMap::new()),
                Some("cached".to_string()),
                Some("old request".to_string()),
                Some(vec!["pdf".to_string()]),
            ),
            ghost_commit: None,
            execution_context: crate::execution_context::ExecutionContext::unrestricted(
                std::path::PathBuf::from("."),
            ),
            git_repo_root: None,
            config_arc: Arc::new(config),
            steer_rx: None,
            recent_tool_names: std::collections::HashSet::new(),
            cost_event_sink: None,
            short_term_memory: crate::short_term_memory::ShortTermMemory::new(session_id, 2000),
            previous_summary: None,
            setup_just_consumed: false,
            tool_call_repeat: ToolCallRepeatTracker::default(),
            llm_role: LlmRole::Primary,
        };

        agent.maybe_refresh_skills_after_tool(
            tn::APPLY_PATCH,
            &serde_json::json!({"target": "skills"}),
            "Patch applied successfully.",
        );

        assert!(agent.caches.skills_context_for_test().is_none());
        assert!(agent.caches.skills_request_for_test().is_none());
        assert!(agent.caches.last_skill_drop_signature_for_test().is_none());
        assert!(
            agent.skill_env_allowlist.contains("SLACK_BOT_TOKEN"),
            "skill env allowlist should be recomputed from loaded skills"
        );
    }

    /// `reload_config` must clear the skills caches (the env-allowlist they
    /// were rendered against is stale) and replace the credentials wholesale,
    /// while preserving identity (config does not affect IDENTITY.md). This
    /// is the Agent-side end-to-end of `SystemPromptCache::on_config_reloaded`.
    #[test]
    fn reload_config_clears_skills_and_replaces_credentials_preserving_identity() {
        let config = Config::default();
        let session = Session::new();
        let session_id = session.meta.id.clone();
        let mut agent = Agent {
            config: config.clone(),
            history: Vec::new(),
            session,
            policy: config.policy.clone(),
            hook_registry: HookRegistry::new(),
            turn_count: 0,
            rate_guard: SessionRateGuard::new(config.security.action_limits.clone()),
            agent_control: None,
            spawn_depth: 0,
            tools_filter: None,
            skill_env_allowlist: std::collections::HashSet::new(),
            metrics: BorgMetrics::default(),
            browser_session: None,
            tts_synthesizer: None,
            config_rx: None,
            db: Mutex::new(None),
            caches: SystemPromptCache::with_test_state(
                Some("identity-snapshot".into()),
                Some(HashMap::from([("OLD_TOKEN".into(), "v".into())])),
                Some("rendered-skills".into()),
                Some("user-request".into()),
                Some(vec!["pdf".into()]),
            ),
            ghost_commit: None,
            execution_context: crate::execution_context::ExecutionContext::unrestricted(
                std::path::PathBuf::from("."),
            ),
            git_repo_root: None,
            config_arc: Arc::new(config.clone()),
            steer_rx: None,
            recent_tool_names: std::collections::HashSet::new(),
            cost_event_sink: None,
            short_term_memory: crate::short_term_memory::ShortTermMemory::new(session_id, 2000),
            previous_summary: None,
            setup_just_consumed: false,
            tool_call_repeat: ToolCallRepeatTracker::default(),
            llm_role: LlmRole::Primary,
        };

        agent.reload_config(Config::default());

        assert_eq!(
            agent.caches.identity_for_test(),
            Some("identity-snapshot"),
            "identity must survive config reload"
        );
        assert!(
            agent.caches.skills_context_for_test().is_none(),
            "skills context must clear on reload"
        );
        assert!(
            agent.caches.skills_request_for_test().is_none(),
            "skills request key must clear on reload"
        );
        assert!(
            agent.caches.last_skill_drop_signature_for_test().is_none(),
            "drop signature must clear on reload"
        );
        assert!(
            !agent.credentials_cached().contains_key("OLD_TOKEN"),
            "old credentials must be replaced wholesale"
        );
    }

    #[test]
    fn unknown_tools_default_to_mutating() {
        // New/unknown tools should be blocked by default (safety)
        assert!(is_mutating_tool("some_new_tool"));
        assert!(is_mutating_tool(""));
    }

    #[tokio::test]
    async fn spawn_logged_completes_normally() {
        // A non-panicking task should complete without issues
        let (tx, rx) = tokio::sync::oneshot::channel();
        spawn_logged("test_normal", async move {
            let _ = tx.send(42);
        });
        let result = rx.await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn spawn_logged_handles_panic() {
        // A panicking task should not propagate the panic to the caller
        spawn_logged("test_panic", async {
            panic!("intentional test panic");
        });
        // Give the spawned task time to run and panic
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // If we get here, the panic was handled (not propagated)
    }

    // -- dispatch_parallel_safe_tool --

    #[tokio::test]
    async fn dispatch_parallel_safe_tool_read_file_returns_content() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.txt");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"hello world")
            .unwrap();

        let config = Config::default();
        let (tx, _rx) = mpsc::channel(8);

        let args = serde_json::json!({"path": path.to_str().unwrap()}).to_string();
        let out = dispatch_parallel_safe_tool("read_file", &args, &config, &tx, None)
            .await
            .unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            ToolOutput::Multimodal { text, .. } => text,
        };
        assert!(text.contains("hello world"), "got: {text}");
    }

    #[tokio::test]
    async fn dispatch_parallel_safe_tool_rejects_unsafe_tool_name() {
        // Guard: the planner should never route an Unknown-effect tool here.
        // If it does, the dispatcher surfaces a loud error rather than
        // silently misbehaving.
        let config = Config::default();
        let (tx, _rx) = mpsc::channel(8);
        let result = dispatch_parallel_safe_tool("run_shell", "{}", &config, &tx, None).await;
        assert!(result.is_err(), "unsafe tool must return Err");
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("non-parallel-safe") || msg.contains("planner/dispatch mismatch"),
            "error should name the mismatch: {msg}"
        );
    }

    #[tokio::test]
    async fn dispatch_parallel_safe_tool_invalid_json_returns_text() {
        let config = Config::default();
        let (tx, _rx) = mpsc::channel(8);
        let out = dispatch_parallel_safe_tool("read_file", "not json", &config, &tx, None)
            .await
            .unwrap();
        let text = match out {
            ToolOutput::Text(t) => t,
            _ => panic!("expected text output"),
        };
        assert!(text.contains("Invalid JSON"), "got: {text}");
    }

    // -- SETUP template tests --

    #[test]
    fn setup_template_wraps_in_first_conversation_tags() {
        let rendered = SETUP_TEMPLATE.render([("setup", "Hello world")]).unwrap();
        assert!(rendered.contains("<first_conversation>"));
        assert!(rendered.contains("</first_conversation>"));
        assert!(rendered.contains("Hello world"));
    }

    #[test]
    fn setup_template_preserves_multiline_content() {
        let content = "# First Boot\n\nLine 1\nLine 2\n- bullet";
        let rendered = SETUP_TEMPLATE.render([("setup", content)]).unwrap();
        assert!(rendered.contains("# First Boot"));
        assert!(rendered.contains("- bullet"));
    }

    #[test]
    fn setup_file_lifecycle_atomic_consume() {
        // Simulates the atomic rename → read → delete lifecycle from build_system_prompt
        let tmp = tempfile::tempdir().unwrap();
        let setup_path = tmp.path().join("SETUP.md");
        let consumed_path = setup_path.with_extension("md.consumed");

        // Write SETUP.md
        std::fs::write(&setup_path, "# First Boot\nTest content").unwrap();
        assert!(setup_path.exists());

        // Atomic rename (simulates line 732)
        assert!(std::fs::rename(&setup_path, &consumed_path).is_ok());
        assert!(!setup_path.exists());
        assert!(consumed_path.exists());

        // Read consumed file (simulates line 733)
        let content = std::fs::read_to_string(&consumed_path).unwrap();
        assert!(content.contains("First Boot"));

        // Delete consumed file (simulates line 740)
        std::fs::remove_file(&consumed_path).unwrap();
        assert!(!consumed_path.exists());
        assert!(!setup_path.exists());
    }

    #[test]
    fn setup_file_missing_does_not_inject() {
        // If SETUP.md doesn't exist, rename fails and nothing is injected
        let tmp = tempfile::tempdir().unwrap();
        let setup_path = tmp.path().join("SETUP.md");
        let consumed_path = setup_path.with_extension("md.consumed");

        assert!(std::fs::rename(&setup_path, &consumed_path).is_err());
    }

    #[test]
    fn setup_file_not_injected_twice() {
        // After first consumption, file is gone — second attempt is a no-op
        let tmp = tempfile::tempdir().unwrap();
        let setup_path = tmp.path().join("SETUP.md");
        let consumed_path = setup_path.with_extension("md.consumed");

        std::fs::write(&setup_path, "content").unwrap();
        std::fs::rename(&setup_path, &consumed_path).unwrap();
        std::fs::remove_file(&consumed_path).unwrap();

        // Second attempt — file is gone
        assert!(std::fs::rename(&setup_path, &consumed_path).is_err());
    }

    // -- should_nudge_for_response --

    #[test]
    fn should_nudge_for_response_matrix() {
        // (text, tools_executed, already_retried, expected_nudge)
        let cases: &[(&str, bool, bool, bool)] = &[
            ("", true, false, true),       // empty after tools → nudge
            ("  \n  ", true, false, true), // whitespace-only counts as empty
            ("Done!", true, false, false), // non-empty text → no nudge
            ("", false, false, false),     // no tools ran → no nudge
            ("", true, true, false),       // already retried once → no nudge
        ];
        for (text, tools, retried, expected) in cases {
            let got = should_nudge_for_response(text, *tools, *retried);
            assert_eq!(
                got, *expected,
                "should_nudge_for_response({text:?}, tools={tools}, retried={retried}) = {got}, expected {expected}"
            );
        }
    }

    // -- collaboration mode template tests --

    // -- new system prompt section tests --

    #[test]
    fn build_system_prompt_has_tooling_section() {
        let body = extract_build_system_prompt_body();
        assert!(
            body.contains("build_tooling_section"),
            "system prompt must include tooling section"
        );
    }

    #[test]
    fn build_system_prompt_has_tool_call_style() {
        let body = extract_build_system_prompt_body();
        assert!(
            body.contains("build_tool_call_style_section"),
            "system prompt must include tool call style guidance"
        );
    }

    #[test]
    fn build_system_prompt_has_silent_reply() {
        let body = extract_build_system_prompt_body();
        assert!(
            body.contains("build_silent_reply_section"),
            "system prompt must include silent reply protocol"
        );
    }

    #[test]
    fn build_system_prompt_has_heartbeat_protocol() {
        let body = extract_build_system_prompt_body();
        assert!(
            body.contains("build_heartbeat_section"),
            "system prompt must include heartbeat ack protocol"
        );
    }

    #[test]
    fn build_system_prompt_has_reply_tags() {
        let body = extract_build_system_prompt_body();
        assert!(
            body.contains("build_reply_tags_section"),
            "system prompt must include reply tags section"
        );
    }

    #[test]
    fn build_system_prompt_has_messaging() {
        let body = extract_build_system_prompt_body();
        assert!(
            body.contains("build_messaging_section"),
            "system prompt must include messaging/channel routing"
        );
    }

    #[test]
    fn build_system_prompt_has_reasoning() {
        let body = extract_build_system_prompt_body();
        assert!(
            body.contains("build_reasoning_section"),
            "system prompt must include reasoning format section"
        );
    }

    #[test]
    fn tool_call_style_section_contains_guidance() {
        let section = Agent::build_tool_call_style_section();
        assert!(section.contains("do not narrate"));
        assert!(section.contains("tool_call_style"));
    }

    #[test]
    fn silent_reply_section_contains_token() {
        let section = Agent::build_silent_reply_section();
        assert!(section.contains(constants::SILENT_REPLY_TOKEN));
        assert!(section.contains("silent_replies"));
    }

    #[test]
    fn reply_tags_section_contains_tag() {
        let section = Agent::build_reply_tags_section();
        assert!(section.contains("[[reply_to_current]]"));
        assert!(section.contains("reply_tags"));
    }

    #[test]
    fn silent_reply_token_is_not_empty() {
        assert!(!constants::SILENT_REPLY_TOKEN.is_empty());
    }

    #[test]
    fn heartbeat_ok_token_is_not_empty() {
        assert!(!constants::HEARTBEAT_OK_TOKEN.is_empty());
    }

    #[test]
    fn build_system_prompt_section_ordering() {
        // Verify: tooling -> security -> memory -> silent -> heartbeat -> environment
        let body = extract_build_system_prompt_body();
        let tooling = body.find("build_tooling_section").unwrap();
        let security = body.find("security_policy").unwrap();
        let silent = body.find("build_silent_reply_section").unwrap();
        let heartbeat = body.find("build_heartbeat_section").unwrap();
        let env = body.find("build_environment_section").unwrap();
        assert!(tooling < security, "tooling before security");
        assert!(security < silent, "security before silent reply");
        assert!(silent < heartbeat, "silent reply before heartbeat");
        assert!(heartbeat < env, "heartbeat before environment");
    }

    #[test]
    fn run_iteration_routes_invalid_calls_through_execute_tool_calls() {
        // Source-grep guard for the contract between run_iteration and
        // execute_tool_calls: invalid (addressable) calls must NOT skip the
        // execute path, otherwise the assistant message persists with a
        // tool_call shell that has no matching tool_result and the LLM API
        // rejects the next turn.
        let iter_idx = AGENT_RS_SRC
            .find("let (tc, invalid) = partition_tool_calls(")
            .expect("partition_tool_calls call site must exist in run_iteration");
        let after = &AGENT_RS_SRC[iter_idx..];
        let early_return = after
            .find("if tc.is_empty() && invalid.is_empty()")
            .expect("early-return guard must check BOTH valid and invalid as empty");
        let exec_call = after
            .find("self.execute_tool_calls(&tc, &invalid,")
            .expect("execute_tool_calls must be invoked with both valid and invalid slices");
        assert!(
            early_return < exec_call,
            "the empty-check must precede the execute call",
        );
    }

    #[test]
    fn tool_call_repeat_guard_runs_before_rate_guard_in_run_tool_calls() {
        // Source-grep guard: the repeat check must precede the rate-guard
        // call so blocked repeats do not consume rate-guard budget. If
        // someone reorders these, the per-session 200/500 tool-call
        // budget burns down on calls that were never going to execute.
        let fn_idx = AGENT_RS_SRC
            .find("async fn run_tool_calls(")
            .expect("run_tool_calls must exist");
        let body = &AGENT_RS_SRC[fn_idx
            ..AGENT_RS_SRC[fn_idx..]
                .find("\n    async fn ")
                .unwrap_or(8000)
                + fn_idx];
        let repeat_idx = body
            .find("self.tool_call_repeat.observe(")
            .expect("run_tool_calls must call self.tool_call_repeat.observe(...)");
        let rate_idx = body
            .find("self.rate_guard.record(action_type)")
            .expect("run_tool_calls must call self.rate_guard.record(...)");
        assert!(
            repeat_idx < rate_idx,
            "tool_call_repeat.observe must run BEFORE rate_guard.record \
             (repeat_idx={repeat_idx}, rate_idx={rate_idx})",
        );
    }

    #[test]
    fn tool_call_repeat_guard_runs_before_rate_guard_in_parallel_path() {
        // Same invariant as above for the parallel-execution path. Both
        // paths share the rate-guard, so both must guard repeats first.
        let fn_idx = AGENT_RS_SRC
            .find("async fn run_tool_group_parallel(")
            .expect("run_tool_group_parallel must exist");
        let body = &AGENT_RS_SRC[fn_idx
            ..AGENT_RS_SRC[fn_idx..]
                .find("\n    async fn ")
                .unwrap_or(12000)
                + fn_idx];
        let repeat_idx = body
            .find("self.tool_call_repeat.observe(")
            .expect("run_tool_group_parallel must call self.tool_call_repeat.observe(...)");
        let rate_idx = body
            .find("self.rate_guard.record(action_type)")
            .expect("run_tool_group_parallel must call self.rate_guard.record(...)");
        assert!(
            repeat_idx < rate_idx,
            "tool_call_repeat.observe must run BEFORE rate_guard.record in parallel path \
             (repeat_idx={repeat_idx}, rate_idx={rate_idx})",
        );
    }

    #[test]
    fn run_agent_loop_resets_tool_call_repeat_tracker() {
        // Source-grep guard: each user turn starts fresh. Without the
        // reset, a turn that ended with a blocked streak would block the
        // first identical call of the next turn — which the user may have
        // explicitly asked for.
        let loop_idx = AGENT_RS_SRC
            .find("pub async fn run_agent_loop(")
            .expect("run_agent_loop must exist");
        let body = &AGENT_RS_SRC
            [loop_idx..AGENT_RS_SRC[loop_idx..].find("\n    fn ").unwrap_or(20_000) + loop_idx];
        assert!(
            body.contains("self.tool_call_repeat.reset()"),
            "run_agent_loop must reset the repeat tracker at start of turn"
        );
    }

    // ── Reactive compaction stalled-state error (Gap 1) ───────────────
    //
    // When reactive compaction frees zero history, the next LLM stream
    // will overflow again. Before this fix, the user only saw a generic
    // `[stream interrupted: <provider error>]` after the second failure.
    // The error must be surfaced *now* with the actionable cause.

    fn extract_reactive_compact_body() -> &'static str {
        let start = AGENT_RS_SRC
            .find("async fn reactive_compact_history(")
            .expect("reactive_compact_history must exist");
        let after = &AGENT_RS_SRC[start..];
        // Skip past our own signature (otherwise we'd match it as the
        // "next fn"), then find the soonest of `\n    async fn ` and
        // `\n    fn ` — whichever comes first marks the end of the body.
        let body_start = 1; // step over the leading `a` of `async fn`
        let search = &after[body_start..];
        let next_async = search.find("\n    async fn ").map(|i| i + body_start);
        let next_sync = search.find("\n    fn ").map(|i| i + body_start);
        let end_rel = match (next_async, next_sync) {
            (Some(a), Some(s)) => a.min(s),
            (Some(a), None) => a,
            (None, Some(s)) => s,
            (None, None) => after.len(),
        };
        &after[..end_rel]
    }

    #[test]
    fn reactive_compact_zero_drop_emits_error_event() {
        // Source-grep guard: the dropped==0 branch must emit
        // AgentEvent::Error so the TUI surfaces an actionable message.
        // Replacing the warn! with only a log regresses to OpenClaw's
        // "silent degradation" failure mode.
        let body = extract_reactive_compact_body();
        // The dropped==0 else branch must contain BOTH the warn! and a
        // user-visible AgentEvent::Error send.
        let else_idx = body
            .find("warn!(\"reactive_compact_history: nothing dropped")
            .expect("zero-drop warn! must exist as the trigger");
        let after_else = &body[else_idx..];
        assert!(
            after_else.contains("AgentEvent::Error"),
            "zero-drop branch must emit AgentEvent::Error after the warn!"
        );
    }

    #[test]
    fn reactive_compact_zero_drop_error_text_is_actionable() {
        // The error text must name the cause AND the actionable knob.
        // A vague message ("compaction failed") leaves the user with no
        // path to recovery. Tokens checked are stable identifiers, not
        // prose — small wording tweaks won't break this.
        let body = extract_reactive_compact_body();
        let err_idx = body
            .find("Context overflow and history compaction freed nothing")
            .expect("zero-drop error must mention 'Context overflow' cause");
        let snippet = &body[err_idx..(err_idx + 400).min(body.len())];
        assert!(
            snippet.contains("conversation.max_history_tokens"),
            "zero-drop error must reference the configurable knob: {snippet}"
        );
    }

    #[test]
    fn reactive_compact_history_compacted_event_remains_gated() {
        // HistoryCompacted must only fire when dropped > 0. If someone
        // hoists the send out of the `if dropped > 0` branch, the TUI
        // will show a misleading "Compacted N messages" notice when
        // nothing actually changed.
        let body = extract_reactive_compact_body();
        // Find the second HistoryCompacted send (the first is the
        // cheap-pruning early-return). It must follow `if dropped > 0`.
        let first = body
            .find("AgentEvent::HistoryCompacted")
            .expect("first HistoryCompacted send must exist");
        let after_first = &body[first + 1..];
        let second_rel = after_first
            .find("AgentEvent::HistoryCompacted")
            .expect("second HistoryCompacted send must exist");
        let second = first + 1 + second_rel;
        // Find the most recent `if dropped > 0` before the second send.
        let preceding_guard = body[..second]
            .rfind("if dropped > 0")
            .expect("second HistoryCompacted send must be guarded by `if dropped > 0`");
        // No `else` should appear between the guard and the send.
        let between = &body[preceding_guard..second];
        let else_count =
            between.matches("\n        } else").count() + between.matches("} else {").count();
        // The guard's own `else` only appears AFTER the send, not between.
        assert_eq!(
            else_count, 0,
            "HistoryCompacted send at offset {second} must be inside the `if dropped > 0` \
             block, not after an else (between snippet: {between:?})"
        );
    }

    // ── No-user-message-replay regression guards (Gap 3) ──────────────
    //
    // The OpenClaw failure mode: on context-overflow retry, the tool
    // re-injected the user's last message, so the agent answered the
    // same prompt over and over. Borg must not regress to that. These
    // are source-grep guards because the invariant ("the retry path
    // does not synthesize Message::user") is structural — types can't
    // express it. Precedent: the TUI mouse-capture and prompt-cache
    // guards.

    fn extract_run_agent_loop_stream_error_block() -> &'static str {
        // The stream-error retry block: from `if let Some(err_msg) =
        // &stream_error {` to its matching `continue;`.
        let start = AGENT_RS_SRC
            .find("if let Some(err_msg) = &stream_error {")
            .expect("run_agent_loop must contain stream_error retry block");
        let after = &AGENT_RS_SRC[start..];
        let end_rel = after
            .find("continue;\n                }")
            .map(|i| i + "continue;\n                }".len())
            .or_else(|| after.find("continue;").map(|i| i + "continue;".len()))
            .expect("stream-error retry block must end with `continue;`");
        &after[..end_rel]
    }

    const FORBIDDEN_USER_REPLAY_TOKENS: &[&str] = &[
        "Message::user(",
        "Role::User",
        "inject_history_message",
        "latest_user_message_text",
    ];

    #[test]
    fn reactive_compact_history_must_not_replay_user_messages() {
        // OpenClaw regression guard: the compaction path reads the
        // user's history but must never *write* a new user message.
        // If a future change adds (e.g.) "preserve last user msg by
        // re-pushing it after summarization", it will trip this test.
        let body = extract_reactive_compact_body();
        for token in FORBIDDEN_USER_REPLAY_TOKENS {
            assert!(
                !body.contains(token),
                "reactive_compact_history must not contain `{token}` \
                 (OpenClaw failure mode: re-injecting user message on retry \
                 causes agent to answer the same prompt repeatedly)"
            );
        }
    }

    #[test]
    fn stream_error_retry_path_must_not_replay_user_messages() {
        // Same invariant for the run_agent_loop stream-error → continue
        // block. The retry must just compact + re-stream; it must not
        // synthesize a fresh user message.
        let body = extract_run_agent_loop_stream_error_block();
        for token in FORBIDDEN_USER_REPLAY_TOKENS {
            assert!(
                !body.contains(token),
                "run_agent_loop stream-error retry block must not contain `{token}`: \
                 see OpenClaw failure mode"
            );
        }
    }

    #[test]
    fn reactive_compact_calls_continue_to_retry_not_replay() {
        // Positive-shape assertion paired with the negative ones above:
        // the retry mechanism is `continue` on the outer loop (re-using
        // the existing history). If someone replaces `continue` with a
        // recursive call to `run_agent_loop` or a re-push of the user
        // message, both this and the negative tests above would fail.
        let body = extract_run_agent_loop_stream_error_block();
        assert!(
            body.contains("continue;"),
            "stream-error retry must use `continue` to re-run the outer loop \
             with the same (compacted) history"
        );
        assert!(
            body.contains("reactive_compact_history"),
            "stream-error retry must invoke reactive_compact_history before retrying"
        );
    }

    #[test]
    fn reactive_compact_history_only_drops_user_messages_never_adds() {
        // Behavioral assertion of the structural invariant: the only
        // calls inside reactive_compact_history that mutate self.history
        // are compact_tool_results, compact_history_v2, and
        // compact_history — all of which are documented to be
        // monotonically non-additive (they only drop or summarize).
        // If a new mutator is introduced, this test trips and forces
        // a deliberate audit.
        let body = extract_reactive_compact_body();
        let approved_mutators = [
            "compact_tool_results(&mut self.history",
            "compact_history_v2(",
            "compact_history(&mut self.history",
            "normalize_history(&mut self.history",
        ];
        // Forbid raw history mutations not in the approved list.
        let forbidden_mutations = [
            "self.history.push(",
            "self.history.insert(",
            "self.history.extend(",
            "self.history.append(",
        ];
        for forbidden in &forbidden_mutations {
            assert!(
                !body.contains(forbidden),
                "reactive_compact_history must not call `{forbidden}` — \
                 history can only be drained/summarized, never extended. \
                 Approved mutators: {approved_mutators:?}"
            );
        }
    }
}
