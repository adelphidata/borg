use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{info_span, instrument, trace, warn, Instrument};

use crate::config::Config;
use crate::constants;
use crate::conversation::{
    compact_history, compact_tool_results, enforce_tool_result_share_limit, history_tokens,
    normalize_history, rewind_to_nth_user, undo_last_turn,
};
use crate::db::Database;
use crate::hooks::{HookAction, HookContext, HookData, HookPoint, HookRegistry};
use crate::identity::load_identity;
use crate::internal_tag_filter::InternalTagFilter;
use crate::llm::{LlmClient, StreamEvent, UsageData};
use crate::llm_error::{classify_error_text, FailoverReason};
use crate::logging::log_message;
use crate::memory::{load_memory_context_db, load_memory_context_db_ranked};
use crate::policy::ExecutionPolicy;
use crate::rate_guard::{RateDecision, SessionRateGuard};
use crate::secrets::redact_secrets;
use crate::session::Session;
use crate::skills::load_skills_context;
use crate::telemetry::BorgMetrics;
use crate::template::Template;
use crate::tool_handlers;
use crate::tool_names as tn;
use crate::truncate::truncate_output;
use crate::types::{ContentPart, FunctionCall, Message, ToolCall, ToolDefinition, ToolOutput};

use std::sync::LazyLock;

mod system_prompt;
mod tool_classification;

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

static SKILLS_TEMPLATE: LazyLock<Template> =
    LazyLock::new(|| parse_template("\n<skills trust=\"verified\">\n{{ skills }}\n</skills>\n"));

static SETUP_TEMPLATE: LazyLock<Template> = LazyLock::new(|| {
    parse_template("\n<first_conversation>\n{{ setup }}\n</first_conversation>\n")
});

// Collaboration mode templates
const COLLAB_MODE_DEFAULT: &str = include_str!("../../templates/collaboration_mode/default.md");
const COLLAB_MODE_EXECUTE: &str = include_str!("../../templates/collaboration_mode/execute.md");
const COLLAB_MODE_PLAN: &str = include_str!("../../templates/collaboration_mode/plan.md");

// Workflow guidance template (injected when workflows are active for current model)
const WORKFLOW_GUIDANCE: &str = include_str!("../../templates/workflow_guidance.md");

/// Maximum number of parallel tool calls allowed in a single LLM response.
/// Prevents OOM from malformed stream events with huge indices.
const MAX_TOOL_CALLS: usize = constants::MAX_AGENT_TOOL_CALLS;

const SECURITY_POLICY: &str = include_str!("../../templates/security_policy.md");

/// Result of monthly token budget check.
enum BudgetCheck {
    Ok,
    Warning(String),
    Exceeded(String),
}

/// Control-flow outcome when `run_agent_loop` encounters a text-only LLM
/// response. The loop either terminates the turn or loops again with a nudge
/// instructing the model to produce a visible response for the user.
enum TextOnlyOutcome {
    /// Exit `run_agent_loop` with `Ok(())`. All persistence and TurnComplete
    /// events have already been emitted by the helper.
    TerminateTurn,
    /// Loop again: the helper persisted a system nudge asking for a response.
    /// Caller must set `nudged_for_response = true` before continuing.
    NudgeAndContinue,
}

/// Events emitted by the agent loop during a conversation turn.
pub enum AgentEvent {
    /// Incremental text token from the LLM.
    TextDelta(String),
    /// Incremental thinking/reasoning token from the LLM.
    ThinkingDelta(String),
    /// A tool is about to execute.
    ToolExecuting {
        /// Tool name.
        name: String,
        /// Serialized tool arguments.
        args: String,
    },
    /// A tool has finished executing.
    ToolResult {
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
    /// The agent is requesting user input mid-turn. Send the user's response via the channel.
    UserInputRequest {
        prompt: String,
        respond: oneshot::Sender<String>,
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

/// Core conversation agent: manages history, tools, and the LLM interaction loop.
pub struct Agent {
    /// Primary config. Must be kept in sync with `config_arc` — update both in `reload_config`.
    config: Config,
    history: Vec<Message>,
    session: Session,
    policy: ExecutionPolicy,
    hook_registry: HookRegistry,
    turn_count: u32,
    rate_guard: SessionRateGuard,
    agent_control: Option<crate::multi_agent::AgentControl>,
    spawn_depth: u32,
    /// When set, restricts which tools this agent may use (from role's `tools_allowed`).
    tools_filter: Option<Vec<String>>,
    /// Env vars declared by skills' `requires.env` — only these credentials are injected into run_shell.
    skill_env_allowlist: std::collections::HashSet<String>,
    metrics: BorgMetrics,
    browser_session: Option<crate::browser::BrowserSession>,
    tts_synthesizer: Option<crate::tts::TtsSynthesizer>,
    config_rx: Option<tokio::sync::watch::Receiver<Config>>,
    db: Mutex<Option<Database>>,
    /// Cached skills context string, invalidated on config reload.
    cached_skills_context: Option<String>,
    /// Sorted set of skill names dropped/omitted on the last budget pass.
    /// Used to suppress duplicate warnings when the dropped set hasn't changed.
    last_skill_drop_signature: Option<Vec<String>>,
    /// Ghost commit created at session start for atomic undo (coding agent).
    ghost_commit: Option<crate::git::GhostCommit>,
    /// Git repo root (if CWD is inside a git repo).
    git_repo_root: Option<std::path::PathBuf>,
    /// Cached project doc contents (AGENTS.md / CLAUDE.md), loaded once.
    cached_project_docs: Option<Option<String>>,
    /// Cached identity string, invalidated on write_memory targeting IDENTITY.md.
    cached_identity: Option<String>,
    /// Cached resolved credentials, invalidated on config reload.
    cached_credentials: Option<HashMap<String, String>>,
    /// Arc-wrapped config for cheap cloning into background tasks.
    config_arc: Arc<Config>,
    /// Channel for receiving user steer messages mid-turn (injected at tool boundaries).
    steer_rx: Option<mpsc::UnboundedReceiver<String>>,
    /// Tool names used in recent turns, for conditional tool loading.
    recent_tool_names: std::collections::HashSet<String>,
    /// Short-term working memory for the current session.
    short_term_memory: crate::short_term_memory::ShortTermMemory,
    /// Most recent compaction summary text (no marker prefix). Used by
    /// `compact_history_v2` to issue iterative-update prompts rather than
    /// re-summarizing from scratch. Populated either by the current
    /// session's compactions or by scanning loaded history on resume.
    previous_summary: Option<String>,
}

/// Common state produced by `build_common`, consumed by both `new()` and `new_sub_agent()`.
struct AgentCommon {
    policy: ExecutionPolicy,
    rate_guard: SessionRateGuard,
    session: Session,
    db: Option<Database>,
    skill_env_allowlist: std::collections::HashSet<String>,
    tts_synthesizer: Option<crate::tts::TtsSynthesizer>,
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
    let tts_synthesizer = crate::tts::TtsSynthesizer::from_config(config);
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
        if self.cached_credentials.is_none() {
            self.cached_credentials = Some(self.config.resolve_credentials());
        }
    }

    /// Borrow the cached credentials, falling back to an empty map if the cache
    /// was not populated yet. Callers inside this module always invoke
    /// [`Self::ensure_credentials_cached`] first; the fallback keeps the
    /// skill-loading path robust if that ordering ever regresses.
    fn credentials_cached(&self) -> &HashMap<String, String> {
        static EMPTY: LazyLock<HashMap<String, String>> = LazyLock::new(HashMap::new);
        self.cached_credentials.as_ref().unwrap_or(&EMPTY)
    }

    /// Assemble an `Agent` from pre-computed parts shared by all constructors.
    fn build_agent(
        config: Config,
        common: AgentCommon,
        agent_control: Option<crate::multi_agent::AgentControl>,
        spawn_depth: u32,
        tools_filter: Option<Vec<String>>,
        metrics: BorgMetrics,
    ) -> Self {
        let git_repo_root = std::env::current_dir()
            .ok()
            .and_then(|cwd| crate::git::find_repo_root(&cwd));
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
            cached_skills_context: None,
            last_skill_drop_signature: None,
            ghost_commit: None,
            git_repo_root,
            cached_project_docs: None,
            cached_identity: None,
            cached_credentials: None,
            config_arc,
            steer_rx: None,
            recent_tool_names: std::collections::HashSet::new(),
            short_term_memory: crate::short_term_memory::ShortTermMemory::new(session_id, 2000),
            previous_summary: None,
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
            let max_age_secs = i64::from(ttl_days) * 86400;
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

    /// Replace config, re-derive dependent state (policy, rate limits).
    fn reload_config(&mut self, new_config: Config) {
        self.policy = new_config.policy.clone();
        self.rate_guard
            .update_limits(new_config.security.action_limits.clone());
        self.cached_skills_context = None; // invalidate on config change
        self.last_skill_drop_signature = None; // re-warn on next budget pass
        let resolved_creds = new_config.resolve_credentials();
        self.cached_credentials = Some(resolved_creds.clone());
        self.skill_env_allowlist =
            crate::skills::collect_required_env_vars(&resolved_creds, &new_config.skills);
        self.config_arc = Arc::new(new_config.clone());
        self.config = new_config;
    }

    /// Create a sub-agent that shares the parent's config but has its own session.
    pub fn new_sub_agent(
        config: Config,
        spawn_depth: u32,
        agents_config: &crate::config::MultiAgentConfig,
        metrics: BorgMetrics,
        tools_filter: Option<Vec<String>>,
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

    /// Log a message and push it to history + SQLite in one step.
    fn log_and_persist(&mut self, msg: Message) {
        log_message(&msg);
        self.persist_message(msg);
    }

    /// Record a skipped/cancelled tool call as a tool_result message.
    fn skip_tool_call(&mut self, tool_call_id: &str, reason: &str) {
        self.log_and_persist(Message::tool_result(tool_call_id, reason));
    }

    /// Build a HookContext for the current agent state.
    fn hook_ctx(&self, point: HookPoint, data: HookData) -> HookContext {
        HookContext {
            point,
            session_id: self.session.meta.id.clone(),
            turn_count: self.turn_count,
            data,
        }
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
        let llm = match LlmClient::new(&self.config) {
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
        // Use cached identity or load + cache it
        let identity = match &self.cached_identity {
            Some(cached) => cached.clone(),
            None => {
                let id = load_identity()?;
                self.cached_identity = Some(id.clone());
                id
            }
        };
        let memory = self.load_memory_with_ranking().await?;
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

        // Safety / security policy
        use std::fmt::Write as _;
        let _ = write!(
            system,
            "\n<security_policy>\n{SECURITY_POLICY}\n</security_policy>\n"
        );

        if self.config.skills.enabled {
            let skills = match &self.cached_skills_context {
                Some(cached) => cached.clone(),
                None => {
                    self.ensure_credentials_cached();
                    let budget = crate::skills::effective_skill_budget(
                        &self.config.skills,
                        Some(self.active_context_window()),
                    );
                    let (rendered, report) = load_skills_context(
                        budget,
                        self.credentials_cached(),
                        &self.config.skills,
                    )?;
                    self.warn_skill_budget_loss(&report);
                    // Populate the cache so subsequent turns reuse this work.
                    // Previously this branch dropped the result on the floor,
                    // forcing every cache-miss path to re-parse SKILL.md files.
                    self.cached_skills_context = Some(rendered.clone());
                    rendered
                }
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
        let project_docs = match &self.cached_project_docs {
            Some(cached) => cached.clone(),
            None => std::env::current_dir().ok().and_then(|cwd| {
                crate::project_doc::discover_project_docs(&cwd)
                    .ok()
                    .flatten()
            }),
        };
        if let Some(ref docs) = project_docs {
            let _ = write!(
                system,
                "\n<project_instructions trust=\"stored\">\n{docs}\n</project_instructions>\n"
            );
        }

        // === DYNAMIC SUFFIX (per-turn, cache-invalidating) ===

        // Short-term working memory (session facts, active project)
        let working_memory = self.short_term_memory.render();
        if !working_memory.is_empty() {
            system.push_str(&working_memory);
        }

        system.push_str(&self.build_collaboration_section());
        system.push_str(&self.build_workflow_guidance_section());
        system.push_str(&self.build_environment_section().await);

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
                }
                let _ = tokio::fs::remove_file(&consumed).await;
            }
        }

        Ok(system)
    }

    /// Load memory context from the DB-backed `memory_entries` store,
    /// using semantic ranking if embeddings are available.
    #[instrument(skip_all)]
    async fn load_memory_with_ranking(&self) -> Result<String> {
        let max_tokens = self.config.memory.max_context_tokens;

        if !self.config.memory.embeddings.enabled {
            return load_memory_context_db(max_tokens);
        }

        // Extract the last user message as the query
        let query = self
            .history
            .iter()
            .rev()
            .find(|m| m.role == crate::types::Role::User)
            .and_then(|m| m.text_content())
            .unwrap_or("")
            .to_string();

        // Short/trivial queries (acks like "ok", "next", "yes") won't produce
        // meaningful semantic rankings — skip the embedding round-trip and fall
        // back to recency ordering. Threshold picked to cover typical one-word
        // affirmatives without suppressing real questions.
        const MIN_QUERY_CHARS_FOR_RANKING: usize = 10;
        let trimmed = query.trim();
        if trimmed.len() < MIN_QUERY_CHARS_FOR_RANKING {
            return load_memory_context_db(max_tokens);
        }

        // Generate query embedding once for ranking
        let (_provider, query_embedding) =
            match crate::embeddings::generate_query_embedding(&self.config, &query).await {
                Ok(result) => result,
                Err(e) => {
                    tracing::debug!("Semantic ranking failed, falling back to recency: {e}");
                    return load_memory_context_db(max_tokens);
                }
            };

        let recency_weight = self.config.memory.embeddings.recency_weight;
        let qe_global = query_embedding;
        let rw = recency_weight;

        let ranking_result = tokio::task::spawn_blocking(move || {
            crate::embeddings::rank_embeddings_by_similarity(&qe_global, "global", rw)
        })
        .await;

        let global_rankings = match ranking_result {
            Ok(Ok(r)) if !r.is_empty() => r,
            Ok(Ok(_)) => return load_memory_context_db(max_tokens),
            Ok(Err(e)) => {
                tracing::debug!("Semantic ranking failed, falling back to recency: {e}");
                return load_memory_context_db(max_tokens);
            }
            Err(e) => {
                tracing::debug!("Ranking task panicked, falling back to recency: {e}");
                return load_memory_context_db(max_tokens);
            }
        };

        load_memory_context_db_ranked(max_tokens, &global_rankings)
    }

    /// Pre-compaction flush: extract durable information from messages about to be dropped
    /// and save to the daily log.
    #[instrument(skip_all)]
    async fn flush_memory_before_compaction(&self, messages: &[crate::types::Message]) {
        let mut transcript = String::new();
        for msg in messages {
            let role = match msg.role {
                crate::types::Role::User => "User",
                crate::types::Role::Assistant => "Assistant",
                crate::types::Role::Tool => "Tool",
                crate::types::Role::System => "System",
            };
            if let Some(content) = msg.text_content() {
                let truncated: String = content
                    .chars()
                    .take(constants::FLUSH_MESSAGE_TRUNCATE_CHARS)
                    .collect();
                transcript.push_str(&format!("{role}: {truncated}\n"));
            }
        }

        if transcript.is_empty() {
            return;
        }

        // Cap transcript
        let transcript: String = transcript
            .chars()
            .take(constants::FLUSH_TRANSCRIPT_CAP_CHARS)
            .collect();

        let flush_prompt = "\
You are a Memory Writing Agent. Your job: extract durable information from conversation \
messages that are about to be dropped from context.

MINIMUM SIGNAL GATE: Before writing anything, ask: 'Will a future agent plausibly act \
better because of what I write here?' If NO — return ONLY the word 'SKIP'. \
Skip when the content is: one-off queries with no reusable insight, generic status updates \
without takeaways, temporary facts that should be re-queried, or obvious/common knowledge.

When there IS signal worth saving, extract structured information:

## User Preferences
- Corrections, repeated requests, or explicit preferences that should become defaults
- Evidence > implication format: 'user said/did X -> suggests they want Y by default'

## Decisions & Facts
- Decisions made, identifiers (preserve exactly), facts learned about the codebase or environment

## Reusable Knowledge
- Commands that worked, failure modes and their fixes, high-leverage shortcuts
- Symptom -> cause -> fix format for failures

## Task Outcomes
- What was attempted, what succeeded/failed, what the user confirmed or rejected

Rules:
- Be concise. Bullet points only. No filler.
- Preserve exact identifiers, paths, commands, and error messages verbatim.
- Never store tokens, keys, passwords, or secrets.
- Overindex on user messages (requests, corrections, interruptions) over assistant messages.
- Omit any section that has no entries.";

        let flush_messages = vec![
            crate::types::Message::system(flush_prompt),
            crate::types::Message::user(format!("Extract key information from:\n\n{transcript}")),
        ];

        let compaction_config = self.config.with_compaction_overrides();
        let llm = match LlmClient::new(&compaction_config) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("Pre-compaction flush LLM init failed: {e}");
                return;
            }
        };

        match llm.chat(&flush_messages, None).await {
            Ok(response) => {
                if let Some(text) = response.text_content() {
                    // Respect the minimum-signal gate: if the LLM says SKIP, don't write
                    if text.trim().eq_ignore_ascii_case("SKIP") {
                        tracing::debug!("Pre-compaction flush: no signal worth saving (SKIP)");
                        return;
                    }
                    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                    // DB entry key matches memory_entries.name (no `.md` suffix).
                    let entry_name = format!("daily/{today}");
                    let header = format!(
                        "\n\n## Pre-compaction flush ({})\n\n",
                        chrono::Local::now().format("%H:%M")
                    );
                    let content = format!("{header}{text}");
                    if let Err(e) = crate::memory::write_memory_db(
                        &entry_name,
                        &content,
                        crate::memory::WriteMode::Append,
                        "global",
                    ) {
                        tracing::warn!("Failed to write pre-compaction flush: {e}");
                    } else if self.config.memory.embeddings.enabled {
                        // Index the daily log so it's immediately searchable.
                        // Read the full entry back from the DB — appends are
                        // additive, so the just-written content plus any
                        // existing entry needs to be re-embedded together.
                        let config = Arc::clone(&self.config_arc);
                        let fname = entry_name;
                        match crate::memory::read_memory_db(&fname, "global") {
                            Ok(Some(full_content)) => {
                                spawn_logged("embed_daily_log", async move {
                                    if let Err(e) = crate::embeddings::embed_memory_file_chunked(
                                        &config,
                                        &fname,
                                        &full_content,
                                        "global",
                                    )
                                    .await
                                    {
                                        tracing::warn!("Failed to embed daily log {fname}: {e}");
                                    }
                                });
                            }
                            Ok(None) => {
                                tracing::debug!(
                                    "daily log {fname} missing after write — skipping embed"
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to read memory entry {fname} for embedding: {e}"
                                );
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Pre-compaction flush LLM call failed: {e}");
            }
        }
    }

    fn build_tool_definitions(&self, user_message: &str) -> Vec<ToolDefinition> {
        let mut tools = crate::tool_definitions::core_tool_definitions(&self.config);
        if self.agent_control.is_some() {
            tools.extend(crate::multi_agent::tools::tool_definitions(
                self.spawn_depth,
                self.config.agents.max_spawn_depth,
            ));
        }

        // Apply role-based tools_filter (from AgentRole.tools_allowed)
        if let Some(ref allowed) = self.tools_filter {
            let allowed_set: std::collections::HashSet<&str> =
                allowed.iter().map(String::as_str).collect();
            tools.retain(|t| allowed_set.contains(t.function.name.as_str()));
        }

        // Apply tool policy filtering (profile + allow/deny)
        let policy = &self.config.tools.policy;
        if self.spawn_depth > 0 {
            tools = crate::tool_policy::filter_subagent_tools(tools, policy);
        } else {
            tools = crate::tool_policy::filter_tools(tools, policy);
        }

        // Conditional tool loading: exclude tool groups not relevant to the
        // current user message or recent tool usage. Saves 500-1500 tokens/turn.
        if self.config.tools.conditional_loading {
            let profile =
                crate::tool_catalog::ToolProfile::from_str_opt(&self.config.tools.policy.profile)
                    .unwrap_or_default();
            let profile_groups = profile.groups();
            tools = crate::tool_catalog::filter_tools_by_relevance(
                tools,
                user_message,
                &self.recent_tool_names,
                &profile_groups,
            );
        }

        // Compact tool schemas: strip redundant metadata to reduce token overhead.
        if self.config.tools.compact_schemas {
            for tool in &mut tools {
                crate::tool_definitions::compact_tool_schema(&mut tool.function.parameters);
            }
        }

        // Sort deterministically so the tools block is byte-identical across turns,
        // preserving the Anthropic prompt cache breakpoint on the tools array.
        tools.sort_by(|a, b| a.function.name.cmp(&b.function.name));
        tools
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

        // Cache project docs on first run
        if self.cached_project_docs.is_none() {
            let docs = std::env::current_dir().ok().and_then(|cwd| {
                crate::project_doc::discover_project_docs(&cwd)
                    .ok()
                    .flatten()
            });
            self.cached_project_docs = Some(docs);
        }

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

            let (messages, tool_defs) = self.prepare_llm_request(iteration).await?;
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
                let mut llm_client = LlmClient::new(&self.config)
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
                        match event {
                            Some(StreamEvent::TextDelta(delta)) => {
                                if let Some(filtered) = tag_filter.push(&delta) {
                                    let redacted = self.maybe_redact(filtered);
                                    let _ = event_tx.send(AgentEvent::TextDelta(redacted)).await;
                                }
                            }
                            Some(StreamEvent::ToolCallDelta {
                                index, id, name, arguments_delta,
                            }) => {
                                if index >= MAX_TOOL_CALLS {
                                    warn!("Tool call index {index} exceeds limit {MAX_TOOL_CALLS}, ignoring");
                                } else {
                                    while tool_calls.len() <= index {
                                        tool_calls.push(PartialToolCall::default());
                                    }
                                    if let Some(id) = id {
                                        tool_calls[index].id = id;
                                    }
                                    if let Some(name) = name {
                                        tool_calls[index].name = name;
                                    }
                                    tool_calls[index].arguments.push_str(&arguments_delta);
                                }
                            }
                            Some(StreamEvent::Usage(usage)) => {
                                self.record_usage(&usage);
                                let _ = event_tx.send(AgentEvent::Usage(usage)).await;
                            }
                            Some(StreamEvent::Done) => {
                                received_terminal = true;
                                break;
                            }
                            Some(StreamEvent::Error(e)) => {
                                received_terminal = true;
                                warn!("LLM stream emitted error mid-response: {e}");
                                stream_error = Some(e.clone());
                                if event_tx.send(AgentEvent::Error(e)).await.is_err() {
                                    trace!("Event channel closed, could not deliver stream error");
                                }
                                break;
                            }
                            Some(StreamEvent::ThinkingDelta(delta)) => {
                                if self.config.conversation.show_thinking {
                                    let _ = event_tx.send(AgentEvent::ThinkingDelta(delta)).await;
                                }
                            }
                            None => break,
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

            if tool_calls.is_empty() {
                match self
                    .handle_text_only_response(
                        &text_content,
                        stream_error.take(),
                        needs_response,
                        nudged_for_response,
                        &event_tx,
                    )
                    .await
                {
                    TextOnlyOutcome::TerminateTurn => return Ok(()),
                    TextOnlyOutcome::NudgeAndContinue => {
                        nudged_for_response = true;
                        continue;
                    }
                }
            }

            let tc = validate_tool_calls(&tool_calls);

            if tc.is_empty() {
                self.persist_discarded_tool_calls(&text_content, stream_error.take(), &event_tx)
                    .await;
                return Ok(());
            }

            // Track tool names for conditional tool loading (recent usage keeps groups active).
            for t in &tc {
                self.recent_tool_names.insert(t.function.name.clone());
            }

            self.execute_tool_calls(&tc, &text_content, &event_tx, &cancel)
                .await;
            needs_response = true;

            // Drain any steer messages from the user at the tool boundary
            self.drain_steers(&event_tx).await;
        }
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
    }

    /// Hot-reload config if the watcher has detected changes.
    fn maybe_reload_config(&mut self) {
        if let Some(ref mut rx) = self.config_rx {
            if rx.has_changed().unwrap_or(false) {
                let new_config = rx.borrow_and_update().clone();
                info_span!("config_reload").in_scope(|| {
                    warn!("Config reloaded from disk");
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

    /// Normalize history and run compaction (tool-result trimming, then LLM-based)
    /// if the token budget is exceeded.
    async fn compact_history_if_needed(
        &mut self,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
        normalize_history(&mut self.history);

        let max_hist = self.config.conversation.max_history_tokens;

        // Age-based degradation: progressively reduce old tool results
        if self.config.conversation.age_based_degradation {
            crate::conversation::age_based_tool_result_degradation(&mut self.history);
        }

        enforce_tool_result_share_limit(&mut self.history, max_hist, 0.5);
        if history_tokens(&self.history) > max_hist {
            compact_tool_results(&mut self.history, max_hist);
        }

        // Only run LLM-based compaction when history still exceeds the token budget
        if history_tokens(&self.history) > max_hist {
            let pre_compaction_len = self.history.len();
            let before_tokens = history_tokens(&self.history);

            // Pre-compaction memory flush: save important info before messages are dropped
            if self.config.memory.flush_before_compaction {
                if let Some(keep_from) =
                    crate::conversation::plan_compaction(&self.history, max_hist)
                {
                    let dropped = &self.history[..keep_from];
                    let dropped_tokens: usize = dropped
                        .iter()
                        .map(|m| match m.text_content() {
                            Some(s) => crate::tokenizer::estimate_tokens(s),
                            None => 0,
                        })
                        .sum();
                    let dropped_count = dropped.len();
                    if dropped_tokens > self.config.memory.flush_soft_threshold_tokens
                        && dropped_count >= self.config.memory.flush_min_messages
                    {
                        let dropped = dropped.to_vec();
                        self.flush_memory_before_compaction(&dropped).await;
                    }
                }
            }

            let compaction_config = self.config.with_compaction_overrides();
            let compaction_llm = LlmClient::new(&compaction_config)?;

            let protect_first_n = self.config.conversation.protect_first_n;
            // Seed the iterative-summary state from any prior compaction
            // marker in the loaded history (session-resume case).
            if self.previous_summary.is_none() {
                self.previous_summary =
                    crate::conversation::extract_last_compaction_summary(&self.history);
            }
            let iterative = self.previous_summary.is_some();

            let mut dropped_count = if protect_first_n > 0 {
                crate::conversation::compact_history_v2(
                    &mut self.history,
                    max_hist,
                    protect_first_n,
                    &mut self.previous_summary,
                    &compaction_llm,
                )
                .await
            } else {
                compact_history(&mut self.history, max_hist, &compaction_llm).await;
                pre_compaction_len.saturating_sub(self.history.len())
            };

            // Fallback: if v2 couldn't compact (protected head consumed the
            // budget, or head clamped out the middle) and history is still
            // over the limit, degrade to non-head-protected compaction so we
            // don't stall indefinitely at an over-budget history.
            if dropped_count == 0 && history_tokens(&self.history) > max_hist {
                let pre_fallback_len = self.history.len();
                compact_history(&mut self.history, max_hist, &compaction_llm).await;
                dropped_count = pre_fallback_len.saturating_sub(self.history.len());
                if dropped_count > 0 {
                    warn!(
                        "v2 compaction yielded no progress; fell back to legacy compaction and dropped {dropped_count} messages"
                    );
                }
            }

            if dropped_count > 0 {
                let after_tokens = history_tokens(&self.history);
                let _ = event_tx
                    .send(AgentEvent::HistoryCompacted {
                        dropped: dropped_count,
                        before_tokens,
                        after_tokens,
                        iterative,
                    })
                    .await;
                if let Ok(guard) = self.db.lock() {
                    if let Some(ref db) = *guard {
                        crate::activity_log::log_activity(
                            db,
                            "info",
                            "agent",
                            &format!("Compaction: dropped {dropped_count} messages"),
                        );
                    }
                }
            }
        }

        Ok(())
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
        let compaction_llm = LlmClient::new(&compaction_config)?;
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
            warn!("reactive_compact_history: nothing dropped, retry will likely re-fail");
        }

        Ok(())
    }

    /// Warm the skills context cache on first call (avoids re-parsing every turn).
    fn warm_skills_cache(&mut self) {
        if self.cached_skills_context.is_none() && self.config.skills.enabled {
            self.ensure_credentials_cached();
            let budget = crate::skills::effective_skill_budget(
                &self.config.skills,
                Some(self.active_context_window()),
            );
            let result =
                load_skills_context(budget, self.credentials_cached(), &self.config.skills);
            match result {
                Ok((ctx, report)) => {
                    self.warn_skill_budget_loss(&report);
                    self.cached_skills_context = Some(ctx);
                }
                Err(e) => warn!("Failed to load skills context: {e}"),
            }
        }
    }

    /// Active model's practical context window in tokens. Resolves through the
    /// model registry; falls back to `DEFAULT_CONTEXT_WINDOW` for unknown models.
    fn active_context_window(&self) -> u32 {
        crate::model_registry::context_window_for(&self.config.llm.model)
    }

    /// Emit a per-session toast/log warning when the skill budget had to drop
    /// or downgrade skills. Re-warns only when the dropped-set actually changes
    /// so a stable budget doesn't spam users every turn.
    fn warn_skill_budget_loss(&mut self, report: &crate::skills::SkillBudgetReport) {
        if !report.is_lossy() {
            return;
        }
        let mut signature: Vec<String> = report
            .dropped_full_body
            .iter()
            .chain(report.omitted_entirely.iter())
            .cloned()
            .collect();
        signature.sort();
        if self
            .last_skill_drop_signature
            .as_ref()
            .is_some_and(|prev| prev == &signature)
        {
            return;
        }
        self.last_skill_drop_signature = Some(signature);
        warn!(
            budget_tokens = report.budget_tokens,
            used_tokens = report.used_tokens,
            dropped_full_body = ?report.dropped_full_body,
            omitted_entirely = ?report.omitted_entirely,
            "Skill budget exceeded — some skills were downgraded or omitted from the system prompt"
        );
    }

    /// Build system prompt, tool definitions, fire pre-LLM hooks, and assemble
    /// the full message list for the LLM request.
    async fn prepare_llm_request(
        &mut self,
        iteration: usize,
    ) -> Result<(Vec<Message>, Vec<ToolDefinition>)> {
        let mut system_prompt = self
            .build_system_prompt()
            .await
            .context("Failed to build system prompt")?;

        // Extract user message early — needed for conditional tool loading and hooks.
        let user_msg = self
            .history
            .iter()
            .rev()
            .find(|m| m.role == crate::types::Role::User)
            .and_then(|m| m.text_content())
            .unwrap_or("")
            .to_string();

        let tool_defs = self.build_tool_definitions(&user_msg);

        // Fire BeforeAgentStart (first iteration) or BeforeLlmCall
        let hook_point = if iteration == 1 {
            HookPoint::BeforeAgentStart
        } else {
            HookPoint::BeforeLlmCall
        };
        let hook_data = if iteration == 1 {
            HookData::AgentStart {
                user_message: user_msg,
            }
        } else {
            HookData::LlmCall {
                message_count: self.history.len(),
            }
        };
        let hook_ctx = self.hook_ctx(hook_point, hook_data);
        if let HookAction::InjectContext(extra) = self.hook_registry.dispatch(&hook_ctx) {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&extra);
        }

        let mut messages = Vec::with_capacity(self.history.len() + 1);
        messages.push(Message::system(&system_prompt));
        messages.extend(self.history.iter().cloned());

        Ok((messages, tool_defs))
    }

    /// Finalize a text-only response: fire TurnComplete hook, persist, auto-save,
    /// and send the TurnComplete event.
    async fn finalize_text_response(
        &mut self,
        text_content: &str,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) {
        let hook_ctx = self.hook_ctx(
            HookPoint::TurnComplete,
            HookData::TurnEnd {
                total_tool_calls: 0,
            },
        );
        self.hook_registry.dispatch(&hook_ctx);

        // Don't persist empty assistant responses — they pollute the context
        // window and cause Gemini to reject subsequent requests ("no parts").
        if !text_content.trim().is_empty() {
            self.log_and_persist(Message::assistant(text_content));
        } else {
            tracing::debug!("Skipping persistence of empty assistant response");
        }
        self.auto_save();
        self.metrics.agent_turns.add(1, &[]);
        let _ = event_tx.send(AgentEvent::TurnComplete).await;
    }

    /// Handle a text-only LLM response (no tool calls). Either terminates the
    /// turn (by finalizing or recording a stream-interrupted marker) or signals
    /// that the caller should nudge the model with a reminder message and loop
    /// again. Centralizes the three exit branches that used to live inline in
    /// `run_agent_loop`.
    async fn handle_text_only_response(
        &mut self,
        text_content: &str,
        stream_error: Option<String>,
        needs_response: bool,
        nudged_for_response: bool,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) -> TextOnlyOutcome {
        // Stream errored out *and* there are no tool calls to recover —
        // persist whatever text arrived with an interruption marker so the
        // next turn (and any later resume) sees that the response was cut
        // short, not a normal completion.
        if let Some(err) = stream_error {
            let content = if text_content.trim().is_empty() {
                format!("[stream interrupted: {err}]")
            } else {
                format!("{text_content}\n\n[stream interrupted: {err}]")
            };
            self.log_and_persist(Message::assistant(&content));
            let _ = event_tx.send(AgentEvent::TurnComplete).await;
            return TextOnlyOutcome::TerminateTurn;
        }

        if should_nudge_for_response(text_content, needs_response, nudged_for_response) {
            self.log_and_persist(Message::system(
                "Respond to the user with a brief confirmation of what you just did.",
            ));
            return TextOnlyOutcome::NudgeAndContinue;
        }

        self.finalize_text_response(text_content, event_tx).await;
        TextOnlyOutcome::TerminateTurn
    }

    /// Persist an interrupted-turn marker when all streamed tool calls failed
    /// validation (names too long, bad JSON, etc.). Emitted as a single
    /// assistant message so the next turn sees the truncation.
    async fn persist_discarded_tool_calls(
        &mut self,
        text_content: &str,
        stream_error: Option<String>,
        event_tx: &mpsc::Sender<AgentEvent>,
    ) {
        let trailer = match stream_error {
            Some(err) => format!("[stream interrupted: {err}; incomplete tool calls discarded]"),
            None => "[incomplete tool calls discarded]".to_string(),
        };
        let content = if text_content.is_empty() {
            format!("[response interrupted — {trailer}]")
        } else {
            format!("{text_content}\n\n{trailer}")
        };
        self.log_and_persist(Message::assistant(&content));
        let _ = event_tx.send(AgentEvent::TurnComplete).await;
    }

    /// Persist the assistant message with tool calls, partition and execute them,
    /// running shell/browser sequentially and other tools in parallel.
    async fn execute_tool_calls(
        &mut self,
        tc: &[ToolCall],
        text_content: &str,
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) {
        let tc_for_msg = tc.to_vec();
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

        // Plan concurrent tool groups. The planner guarantees every group
        // either (a) contains a single Unknown-effect tool or (b) contains
        // only parallel-safe tools with no conflicting file-path writes.
        let concurrent_cfg = &self.config.conversation.concurrent_tools;
        let max_workers = concurrent_cfg.max_workers.max(1);
        let parallel_enabled = concurrent_cfg.enabled && max_workers > 1;

        let groups = crate::tool_effects::plan_groups(tc);
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
                    name: name.clone(),
                    args: args.clone(),
                })
                .await;

            let tool_start = Instant::now();
            let tool_output = self
                .execute_tool(name, args, event_tx)
                .instrument(info_span!("tool.execute", tool.name = %name))
                .await
                .unwrap_or_else(|e| ToolOutput::Text(format!("Error: {e}")));
            let tool_elapsed = tool_start.elapsed().as_secs_f64();
            self.metrics.tool_executions.add(1, &[]);
            self.metrics.tool_duration.record(tool_elapsed, &[]);

            // Sanitize tool name for XML embedding to prevent injection
            let safe_name = crate::xml_util::escape_xml_attr(name);

            let (raw_text, extra_parts) = match tool_output {
                ToolOutput::Text(t) => (t, None),
                ToolOutput::Multimodal { text, parts } => (text, Some(parts)),
            };
            let redacted = self.truncate_and_redact(&raw_text);
            let sanitized = crate::xml_util::sanitize_xml_boundaries(&redacted);
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
                let tx = event_tx.clone();
                let permits = std::sync::Arc::clone(&permits);
                let name_clone = name.clone();
                let args_clone = args.clone();
                join_set.spawn(async move {
                    let _permit = permits.acquire_owned().await.ok();
                    let start = Instant::now();
                    let result =
                        dispatch_parallel_safe_tool(&name_clone, &args_clone, config.as_ref(), &tx)
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
                    name: name.clone(),
                    args: args_for_event,
                })
                .await;

            let tool_output = result.unwrap_or_else(|e| ToolOutput::Text(format!("Error: {e}")));
            self.metrics.tool_executions.add(1, &[]);
            self.metrics.tool_duration.record(elapsed, &[]);

            let safe_name = crate::xml_util::escape_xml_attr(&name);
            let (raw_text, extra_parts) = match tool_output {
                ToolOutput::Text(t) => (t, None),
                ToolOutput::Multimodal { text, parts } => (text, Some(parts)),
            };
            let redacted = self.truncate_and_redact(&raw_text);
            let sanitized = crate::xml_util::sanitize_xml_boundaries(&redacted);
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
    ) -> Result<ToolOutput> {
        let args: serde_json::Value = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolOutput::Text(format!(
                    "Error: Invalid JSON arguments: {e}. Please provide valid JSON."
                )));
            }
        };

        // Tools that return ToolOutput directly (may contain multimodal content)
        match name {
            tn::READ_FILE => return tool_handlers::handle_read_file(&args, &self.config),
            tn::BROWSER => {
                return tool_handlers::handle_browser(
                    &args,
                    &self.config,
                    &mut self.browser_session,
                )
                .await;
            }
            tn::TEXT_TO_SPEECH => {
                if let Some(ref synth) = self.tts_synthesizer {
                    return Ok(tool_handlers::handle_text_to_speech(&args, synth).await);
                }
                return Ok(ToolOutput::Text(
                    "TTS is not configured. Enable it via: borg settings set tts.enabled true"
                        .into(),
                ));
            }
            tn::REQUEST_USER_INPUT => {
                return tool_handlers::handle_request_user_input(&args, event_tx).await;
            }
            _ => {}
        }

        // Tools that return Result<String> (wrapped to ToolOutput::Text below)
        let text_result: Result<String> = match name {
            // Memory
            tn::WRITE_MEMORY => crate::tool_dispatch::handle_write_memory_with_effects(
                &args,
                &self.config,
                &self.config_arc,
                &mut self.cached_identity,
            ),
            tn::READ_MEMORY => tool_handlers::handle_read_memory(&args),
            tn::MEMORY_SEARCH => tool_handlers::handle_memory_search(&args, &self.config).await,
            // Resource listing
            tn::LIST => {
                tool_handlers::handle_list(&args, &self.config, self.agent_control.as_ref())
            }
            tn::LIST_SKILLS => tool_handlers::handle_list_skills(&self.config),
            tn::LIST_CHANNELS => tool_handlers::handle_list_channels(&self.config),
            // File operations
            tn::APPLY_PATCH => tool_handlers::handle_apply_patch_unified(&args, &self.config),
            tn::APPLY_SKILL_PATCH => tool_handlers::handle_apply_skill_patch(&args),
            tn::CREATE_CHANNEL => tool_handlers::handle_create_channel(&args),
            tn::LIST_DIR => tool_handlers::handle_list_dir(&args, &self.config),
            // Shell & web
            tn::RUN_SHELL => {
                tool_handlers::handle_run_shell(
                    &args,
                    &self.config,
                    &self.policy,
                    event_tx,
                    Some(&self.skill_env_allowlist),
                )
                .await
            }
            tn::WEB_FETCH => tool_handlers::handle_web_fetch(&args, &self.config).await,
            tn::WEB_SEARCH => tool_handlers::handle_web_search(&args, &self.config).await,
            // Scheduling & projects
            tn::SCHEDULE | tn::MANAGE_TASKS | tn::MANAGE_CRON => {
                tool_handlers::handle_schedule(&args, &self.config)
            }
            tn::PROJECTS => tool_handlers::handle_projects(&args, &self.config),
            // Media
            tn::GENERATE_IMAGE => tool_handlers::handle_generate_image(&args, &self.config).await,
            // Multi-agent tools
            name @ (tn::SPAWN_AGENT
            | tn::SEND_TO_AGENT
            | tn::WAIT_FOR_AGENT
            | tn::LIST_AGENTS
            | tn::CLOSE_AGENT
            | tn::MANAGE_ROLES) => {
                match crate::tool_dispatch::try_handle_multi_agent_tool(
                    name,
                    &args,
                    &mut self.agent_control,
                    &self.config,
                    &self.history,
                )
                .await
                {
                    Some(result) => result,
                    None => Err(anyhow::anyhow!("Unknown tool: {name}")),
                }
            }
            _ => Err(anyhow::anyhow!("Unknown tool: {name}")),
        };
        text_result.map(ToolOutput::Text)
    }

    fn fire_after_tool_hook(
        hook_registry: &mut HookRegistry,
        session_id: &str,
        turn_count: u32,
        name: &str,
        result: &str,
    ) {
        let hook_ctx = HookContext {
            point: HookPoint::AfterToolCall,
            session_id: session_id.to_string(),
            turn_count,
            data: HookData::ToolResult {
                name: name.to_string(),
                result: result.to_string(),
                is_error: result.starts_with("Error:"),
            },
        };
        hook_registry.dispatch(&hook_ctx);
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
/// This mirrors the handler-selection table in `Agent::execute_tool` but is
/// restricted to tools that (a) take `&Config` or less, (b) do not touch
/// `&mut self`, and (c) only handle effects the planner deems safe to fan
/// out concurrently. The set is kept in sync with `tool_effects::classify`;
/// any mismatch is a bug — unsafe branches return a hard error so the
/// condition is visible in tests rather than silently misbehaving.
async fn dispatch_parallel_safe_tool(
    name: &str,
    args_json: &str,
    config: &Config,
    event_tx: &mpsc::Sender<AgentEvent>,
) -> Result<ToolOutput> {
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(e) => {
            return Ok(ToolOutput::Text(format!(
                "Error: Invalid JSON arguments: {e}. Please provide valid JSON."
            )));
        }
    };

    match name {
        tn::READ_FILE => tool_handlers::handle_read_file(&args, config),
        tn::READ_MEMORY => tool_handlers::handle_read_memory(&args).map(ToolOutput::Text),
        tn::MEMORY_SEARCH => tool_handlers::handle_memory_search(&args, config)
            .await
            .map(ToolOutput::Text),
        tn::LIST_SKILLS => tool_handlers::handle_list_skills(config).map(ToolOutput::Text),
        tn::LIST_CHANNELS => tool_handlers::handle_list_channels(config).map(ToolOutput::Text),
        tn::LIST_DIR => tool_handlers::handle_list_dir(&args, config).map(ToolOutput::Text),
        tn::WEB_FETCH => tool_handlers::handle_web_fetch(&args, config)
            .await
            .map(ToolOutput::Text),
        tn::WEB_SEARCH => tool_handlers::handle_web_search(&args, config)
            .await
            .map(ToolOutput::Text),
        tn::APPLY_PATCH => {
            tool_handlers::handle_apply_patch_unified(&args, config).map(ToolOutput::Text)
        }
        tn::APPLY_SKILL_PATCH => {
            tool_handlers::handle_apply_skill_patch(&args).map(ToolOutput::Text)
        }
        other => {
            // Planner bug — fall back loudly rather than running sequentially
            // via a different code path.
            let _ = event_tx; // silence unused warning if all arms avoid it
            Err(anyhow::anyhow!(
                "dispatch_parallel_safe_tool called with non-parallel-safe tool '{other}' — planner/dispatch mismatch"
            ))
        }
    }
}

/// Validate and convert partial tool calls from the LLM stream into well-formed `ToolCall`s.
/// Drops incomplete, oversized, or malformed entries.
fn validate_tool_calls(tool_calls: &[PartialToolCall]) -> Vec<ToolCall> {
    tool_calls
        .iter()
        .take(constants::MAX_TOOL_CALLS_PER_RESPONSE)
        .filter(|ptc| {
            if ptc.name.is_empty() || ptc.id.is_empty() {
                warn!("Dropping incomplete tool call (missing name or id)");
                return false;
            }
            if ptc.name.len() > constants::MAX_TOOL_NAME_LEN {
                warn!(
                    "Dropping tool call with oversized name ({} bytes)",
                    ptc.name.len()
                );
                return false;
            }
            if ptc.arguments.len() > constants::MAX_TOOL_ARGS_LEN {
                warn!(
                    "Dropping tool call '{}' with oversized arguments ({} bytes)",
                    ptc.name,
                    ptc.arguments.len()
                );
                return false;
            }
            if serde_json::from_str::<serde_json::Value>(&ptc.arguments).is_err() {
                warn!(
                    "Dropping tool call '{}' with incomplete JSON arguments",
                    ptc.name
                );
                return false;
            }
            true
        })
        .map(|ptc| ToolCall {
            id: ptc.id.clone(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: ptc.name.clone(),
                arguments: ptc.arguments.clone(),
            },
        })
        .collect()
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
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
        // Extract ~5KB following the signature — plenty to cover the body.
        let end = (start + 5000).min(AGENT_RS_SRC.len());
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
    fn build_system_prompt_workflow_guidance_after_collaboration() {
        let body = extract_build_system_prompt_body();
        let collab_idx = body
            .find("build_collaboration_section")
            .expect("build_collaboration_section call must exist");
        let wf_idx = body
            .find("build_workflow_guidance_section")
            .expect("build_workflow_guidance_section call must exist");
        assert!(
            wf_idx > collab_idx,
            "workflow guidance must come after collaboration section",
        );
    }

    #[test]
    fn build_system_prompt_workflow_guidance_before_environment() {
        let body = extract_build_system_prompt_body();
        let wf_idx = body
            .find("build_workflow_guidance_section")
            .expect("build_workflow_guidance_section call must exist");
        let env_idx = body
            .find("build_environment_section")
            .expect("build_environment_section call must exist");
        assert!(
            wf_idx < env_idx,
            "workflow guidance must come before environment section",
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
    fn run_agent_loop_persists_interruption_marker() {
        assert!(
            AGENT_RS_SRC.contains("[stream interrupted:"),
            "run_agent_loop must embed a `[stream interrupted: …]` marker in the \
             persisted assistant message when a mid-stream error happens, so the \
             next turn (and any later conversation replay) can tell a truncated \
             response from a completed one"
        );
    }

    #[test]
    fn record_usage_is_the_only_usage_persistence_path() {
        // If the match arm regains an inline db.log_token_usage_with_cache(...)
        // call, the outer future's frame grows again and the record_usage
        // extraction loses its purpose.
        let loop_start = AGENT_RS_SRC
            .find("Some(StreamEvent::Usage(usage))")
            .expect("Usage arm must exist in run_agent_loop");
        let loop_tail_idx = loop_start + 400;
        let loop_tail = &AGENT_RS_SRC[loop_start..loop_tail_idx.min(AGENT_RS_SRC.len())];
        assert!(
            !loop_tail.contains("log_token_usage_with_cache"),
            "StreamEvent::Usage arm must delegate to self.record_usage; do not \
             inline the DB + pricing call back into the tokio::select! body"
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
        // request_user_input blocks execution; must be gated in plan mode.
        assert!(is_mutating_tool("request_user_input"));
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
        let out = dispatch_parallel_safe_tool("read_file", &args, &config, &tx)
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
        let result = dispatch_parallel_safe_tool("run_shell", "{}", &config, &tx).await;
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
        let out = dispatch_parallel_safe_tool("read_file", "not json", &config, &tx)
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
}
