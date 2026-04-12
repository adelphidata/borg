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
use crate::logging::log_message;
use crate::memory::{load_memory_context, load_memory_context_ranked};
use crate::policy::ExecutionPolicy;
use crate::rate_guard::{ActionType, RateDecision, SessionRateGuard};
use crate::secrets::redact_secrets;
use crate::session::Session;
use crate::skills::load_skills_context;
use crate::telemetry::BorgMetrics;
use crate::template::Template;
use crate::tool_handlers;
use crate::truncate::truncate_output;
use crate::types::{ContentPart, FunctionCall, Message, ToolCall, ToolDefinition, ToolOutput};

use std::sync::LazyLock;

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
const COLLAB_MODE_DEFAULT: &str = include_str!("../templates/collaboration_mode/default.md");
const COLLAB_MODE_EXECUTE: &str = include_str!("../templates/collaboration_mode/execute.md");
const COLLAB_MODE_PLAN: &str = include_str!("../templates/collaboration_mode/plan.md");

// Workflow guidance template (injected when workflows are active for current model)
const WORKFLOW_GUIDANCE: &str = include_str!("../templates/workflow_guidance.md");

/// Maximum number of parallel tool calls allowed in a single LLM response.
/// Prevents OOM from malformed stream events with huge indices.
const MAX_TOOL_CALLS: usize = constants::MAX_AGENT_TOOL_CALLS;

const SECURITY_POLICY: &str = include_str!("../templates/security_policy.md");

/// Result of monthly token budget check.
enum BudgetCheck {
    Ok,
    Warning(String),
    Exceeded(String),
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
    let rate_guard = SessionRateGuard::new(config.security.action_limits.clone());
    let session = Session::new();
    let db = match Database::open() {
        Ok(db) => Some(db),
        Err(e) => {
            warn!("Failed to open database on agent init: {e}");
            None
        }
    };
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
    /// Returns a clone suitable for passing to functions that also borrow `&self`.
    fn resolve_credentials_cached(&mut self) -> HashMap<String, String> {
        if let Some(ref cached) = self.cached_credentials {
            return cached.clone();
        }
        let resolved = self.config.resolve_credentials();
        self.cached_credentials = Some(resolved.clone());
        resolved
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
            ghost_commit: None,
            git_repo_root,
            cached_project_docs: None,
            cached_identity: None,
            cached_credentials: None,
            config_arc,
            steer_rx: None,
            recent_tool_names: std::collections::HashSet::new(),
            short_term_memory: crate::short_term_memory::ShortTermMemory::new(session_id, 2000),
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
        }

        self.history.clear();
        self.session = Session::new();

        // Fire SessionStart for the new session
        let hook_ctx = self.hook_ctx(
            HookPoint::SessionStart,
            HookData::SessionStart {
                session_id: self.session.meta.id.clone(),
            },
        );
        self.hook_registry.dispatch(&hook_ctx);
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
        compact_history(
            &mut self.history,
            self.config.conversation.max_history_tokens,
            &llm,
        )
        .await;
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

    /// Build the `<environment>` section with time, CWD, git context, OS, and runtime info.
    async fn build_environment_section(&self) -> String {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");
        let mut s = String::new();
        s.push_str("<environment>\n");
        s.push_str(&format!("Current Time: {now}\n"));
        if let Ok(cwd) = std::env::current_dir() {
            s.push_str(&format!("Working directory: {}\n", cwd.display()));
        }
        if let Some(ref root) = self.git_repo_root {
            let git_ctx = crate::git::collect_git_context(root).await;
            let formatted = crate::git::format_git_context(&git_ctx);
            if !formatted.is_empty() {
                s.push_str(&formatted);
            }
        }
        // Runtime info line (model, provider, thinking, OS)
        let mut runtime_parts: Vec<String> = Vec::new();
        runtime_parts.push(format!(
            "os={} ({})",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
        if let Some(ref provider) = self.config.llm.provider {
            runtime_parts.push(format!("provider={provider}"));
        }
        if !self.config.llm.model.is_empty() {
            runtime_parts.push(format!("model={}", self.config.llm.model));
        }
        let thinking = if self.config.llm.thinking.is_enabled() {
            match self.config.llm.thinking {
                crate::config::ThinkingLevel::Low => "low",
                crate::config::ThinkingLevel::Medium => "medium",
                crate::config::ThinkingLevel::High => "high",
                crate::config::ThinkingLevel::Xhigh => "xhigh",
                crate::config::ThinkingLevel::Off => "off",
            }
        } else {
            "off"
        };
        runtime_parts.push(format!("thinking={thinking}"));
        if let Some(ref tz) = self.config.user.timezone {
            runtime_parts.push(format!("timezone={tz}"));
        }
        s.push_str(&format!("Runtime: {}\n", runtime_parts.join(" | ")));
        s.push_str("</environment>\n");
        s
    }

    /// Build the `<tooling>` section listing available tools with summaries.
    fn build_tooling_section(&self) -> String {
        let tool_summaries: &[(&str, &str)] = &[
            ("write_memory", "Write/append to memory files"),
            ("read_memory", "Read a memory file"),
            (
                "memory_search",
                "Semantic search across memory and sessions",
            ),
            ("list", "List resources (skills, channels, agents)"),
            ("apply_patch", "Create/update/delete files via patch DSL"),
            (
                "run_shell",
                "Execute shell commands (full system access, not sandboxed)",
            ),
            (
                "read_file",
                "Read file contents with line numbers, images, PDFs",
            ),
            ("list_dir", "List directory contents"),
            ("web_fetch", "Fetch URL content"),
            ("web_search", "Search the web"),
            ("browser", "Control headless Chrome browser"),
            (
                "schedule",
                "Manage scheduled jobs: prompt tasks, cron commands, workflows",
            ),
            (
                "projects",
                "Manage projects (create/list/get/update/archive/delete)",
            ),
            (
                "request_user_input",
                "Ask user for clarification when blocked",
            ),
            ("generate_image", "Generate images from text descriptions"),
            ("text_to_speech", "Convert text to speech audio"),
            ("spawn_agent", "Spawn an isolated sub-agent"),
            ("send_to_agent", "Send message to a running sub-agent"),
            ("wait_for_agent", "Wait for a sub-agent to complete"),
            ("close_agent", "Close a running sub-agent"),
        ];

        // Build the full tool list including multi-agent tools when available
        let mut defs = crate::tool_definitions::core_tool_definitions(&self.config);
        if self.agent_control.is_some() {
            defs.extend(crate::multi_agent::tools::tool_definitions(
                self.spawn_depth,
                self.config.agents.max_spawn_depth,
            ));
        }
        let available: std::collections::HashSet<&str> =
            defs.iter().map(|d| d.function.name.as_str()).collect();

        let mut lines = vec![
            "## Tooling".to_string(),
            "Available tools (filtered by config):".to_string(),
        ];
        for &(name, summary) in tool_summaries {
            if available.contains(name) {
                lines.push(format!("- {name}: {summary}"));
            }
        }
        // Include any tools not in the static list (e.g. integration tools)
        for def in &defs {
            let name = def.function.name.as_str();
            if !tool_summaries.iter().any(|&(n, _)| n == name) {
                let desc = def.function.description.split('.').next().unwrap_or("");
                lines.push(format!("- {name}: {desc}"));
            }
        }
        lines.push(String::new());
        format!("\n<tooling>\n{}\n</tooling>\n", lines.join("\n"))
    }

    /// Build the tool call style guidance section.
    fn build_tool_call_style_section() -> &'static str {
        "\n<tool_call_style>\n\
        Default: do not narrate routine, low-risk tool calls (just call the tool).\n\
        Narrate only when it helps: multi-step work, complex problems, sensitive actions (e.g. deletions), or when the user explicitly asks.\n\
        Keep narration brief and value-dense; avoid repeating obvious steps.\n\
        When a first-class tool exists for an action, use it directly instead of asking the user to run CLI commands.\n\
        Use apply_patch (not run_shell) to create or modify files. Use list_dir and read_file to understand code before editing.\n\
        </tool_call_style>\n"
    }

    /// Build the silent reply protocol section.
    fn build_silent_reply_section() -> String {
        let token = constants::SILENT_REPLY_TOKEN;
        format!(
            "\n<silent_replies>\n\
            When you have nothing to say, respond with ONLY: {token}\n\
            Rules:\n\
            - It must be your ENTIRE message — nothing else\n\
            - Never append it to an actual response\n\
            - Never wrap it in markdown or code blocks\n\
            </silent_replies>\n"
        )
    }

    /// Build the heartbeat ack protocol section.
    fn build_heartbeat_section(&self) -> String {
        let ok_token = constants::HEARTBEAT_OK_TOKEN;
        let interval = &self.config.heartbeat.interval;
        format!(
            "\n<heartbeat_protocol>\n\
            Heartbeat interval: {interval}. \
            If you receive a heartbeat poll (*heartbeat tick*) and there is nothing that needs attention, reply exactly:\n\
            {ok_token}\n\
            If something needs attention, do NOT include \"{ok_token}\"; reply with the alert text instead.\n\
            </heartbeat_protocol>\n"
        )
    }

    /// Build the reply tags section for message threading.
    fn build_reply_tags_section() -> &'static str {
        "\n<reply_tags>\n\
        To request a native reply/quote on supported messaging channels, include one tag in your reply:\n\
        - [[reply_to_current]] replies to the triggering message. Must be the very first token (no leading text/newlines).\n\
        - Prefer [[reply_to_current]]. Use [[reply_to:<id>]] only when an id was explicitly provided.\n\
        Tags are stripped before sending; support depends on the channel.\n\
        </reply_tags>\n"
    }

    /// Build the messaging/channel routing guidance section.
    fn build_messaging_section(&self) -> String {
        const CHANNELS: &str =
            "Telegram, Slack, Discord, Teams, Google Chat, Signal, Twilio, iMessage";
        format!(
            "\n<messaging>\n\
            - Reply in current session automatically routes to the source channel ({CHANNELS}).\n\
            - Native integrations are compiled in; do not use run_shell/curl for messaging.\n\
            - Gateway bindings provide per-channel/sender LLM routing overrides.\n\
            - Thread-scoped history: each sender+thread gets its own session.\n\
            </messaging>\n"
        )
    }

    /// Build the reasoning format section (conditional on thinking level).
    fn build_reasoning_section(&self) -> String {
        let level = match self.config.llm.thinking {
            crate::config::ThinkingLevel::Off => return String::new(),
            crate::config::ThinkingLevel::Low => "low",
            crate::config::ThinkingLevel::Medium => "medium",
            crate::config::ThinkingLevel::High => "high",
            crate::config::ThinkingLevel::Xhigh => "xhigh",
        };
        format!(
            "\n<reasoning_format>\n\
            Extended thinking is enabled (level: {level}). \
            Internal reasoning is handled natively by the provider and hidden from the user.\n\
            </reasoning_format>\n"
        )
    }

    /// Build the `<collaboration_mode>` section from the current config.
    fn build_collaboration_section(&self) -> String {
        let mode_template = match self.config.conversation.collaboration_mode {
            crate::config::CollaborationMode::Default => COLLAB_MODE_DEFAULT,
            crate::config::CollaborationMode::Execute => COLLAB_MODE_EXECUTE,
            crate::config::CollaborationMode::Plan => COLLAB_MODE_PLAN,
        };
        format!("\n<collaboration_mode>\n{mode_template}\n</collaboration_mode>\n")
    }

    /// Build the `<workflow_guidance>` section when workflows are active.
    fn build_workflow_guidance_section(&self) -> String {
        if crate::workflow::workflows_active(&self.config) {
            format!("\n<workflow_guidance>\n{WORKFLOW_GUIDANCE}\n</workflow_guidance>\n")
        } else {
            String::new()
        }
    }

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
        system.push_str(&format!(
            "\n<security_policy>\n{SECURITY_POLICY}\n</security_policy>\n"
        ));

        if self.config.skills.enabled {
            let skills = match &self.cached_skills_context {
                Some(cached) => cached.clone(),
                None => {
                    let resolved_creds = self.resolve_credentials_cached();
                    load_skills_context(
                        self.config.skills.max_context_tokens,
                        &resolved_creds,
                        &self.config.skills,
                    )?
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
            system.push_str(&format!(
                "\n<project_instructions trust=\"stored\">\n{docs}\n</project_instructions>\n"
            ));
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

    /// Load memory context, using semantic ranking if embeddings are available.
    #[instrument(skip_all)]
    async fn load_memory_with_ranking(&self) -> Result<String> {
        let max_tokens = self.config.memory.max_context_tokens;

        if !self.config.memory.embeddings.enabled {
            return load_memory_context(max_tokens);
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

        if query.is_empty() {
            return load_memory_context(max_tokens);
        }

        // Generate query embedding once, reuse for both scopes
        let (_provider, query_embedding) =
            match crate::embeddings::generate_query_embedding(&self.config, &query).await {
                Ok(result) => result,
                Err(e) => {
                    tracing::debug!("Semantic ranking failed, falling back to recency: {e}");
                    return load_memory_context(max_tokens);
                }
            };

        let recency_weight = self.config.memory.embeddings.recency_weight;

        // Run global and local ranking in parallel on blocking threads
        let qe_global = query_embedding.clone();
        let qe_local = query_embedding;
        let rw = recency_weight;

        let (global_result, local_result) = tokio::join!(
            tokio::task::spawn_blocking(move || {
                crate::embeddings::rank_embeddings_by_similarity(&qe_global, "global", rw)
            }),
            tokio::task::spawn_blocking(move || {
                crate::embeddings::rank_embeddings_by_similarity(&qe_local, "local", rw)
            }),
        );

        let global_rankings = match global_result
            .unwrap_or_else(|e| Err(anyhow::anyhow!("spawn_blocking panicked: {e}")))
        {
            Ok(r) if !r.is_empty() => r,
            Ok(_) => return load_memory_context(max_tokens),
            Err(e) => {
                tracing::debug!("Semantic ranking failed, falling back to recency: {e}");
                return load_memory_context(max_tokens);
            }
        };

        let local_rankings: Vec<(String, f32)> = local_result
            .unwrap_or_else(|e| {
                tracing::debug!("Local ranking task failed: {e}");
                Ok(Vec::new())
            })
            .unwrap_or_default();

        load_memory_context_ranked(max_tokens, &global_rankings, &local_rankings)
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
                    let filename = format!("daily/{today}.md");
                    let header = format!(
                        "\n\n## Pre-compaction flush ({})\n\n",
                        chrono::Local::now().format("%H:%M")
                    );
                    let content = format!("{header}{text}");
                    if let Err(e) = crate::memory::write_memory_scoped(
                        &filename,
                        &content,
                        crate::memory::WriteMode::Append,
                        "global",
                    ) {
                        tracing::warn!("Failed to write pre-compaction flush: {e}");
                    } else if self.config.memory.embeddings.enabled {
                        // Index the daily log so it's immediately searchable
                        let config = Arc::clone(&self.config_arc);
                        let fname = filename.clone();
                        match crate::memory::read_memory(&fname) {
                            Ok(full_content) => {
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
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to read memory file {fname} for embedding: {e}"
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

            self.compact_history_if_needed().await?;
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
                                let total = usage.prompt_tokens + usage.completion_tokens;
                                if total > 0 {
                                    self.metrics.llm_tokens.add(total, &[]);
                                    {
                                        let guard = self.db_guard();
                                        if let Some(ref db) = *guard {
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
                                    }
                                }
                                let _ = event_tx.send(AgentEvent::Usage(usage)).await;
                            }
                            Some(StreamEvent::Done) => {
                                received_terminal = true;
                                break;
                            }
                            Some(StreamEvent::Error(e)) => {
                                received_terminal = true;
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
                if should_nudge_for_response(&text_content, needs_response, nudged_for_response) {
                    nudged_for_response = true;
                    self.log_and_persist(Message::system(
                        "Respond to the user with a brief confirmation of what you just did.",
                    ));
                    continue;
                }

                self.finalize_text_response(&text_content, &event_tx).await;
                return Ok(());
            }

            let tc = validate_tool_calls(&tool_calls);

            if tc.is_empty() {
                let content = if text_content.is_empty() {
                    "[response interrupted — incomplete tool calls discarded]".to_string()
                } else {
                    format!("{text_content}\n\n[incomplete tool calls discarded]")
                };
                self.log_and_persist(Message::assistant(&content));
                let _ = event_tx.send(AgentEvent::TurnComplete).await;
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
    async fn compact_history_if_needed(&mut self) -> Result<()> {
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
            compact_history(&mut self.history, max_hist, &compaction_llm).await;

            let dropped_count = pre_compaction_len.saturating_sub(self.history.len());
            if dropped_count > 0 {
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

    /// Warm the skills context cache on first call (avoids re-parsing every turn).
    fn warm_skills_cache(&mut self) {
        if self.cached_skills_context.is_none() && self.config.skills.enabled {
            let resolved_creds = self.resolve_credentials_cached();
            match load_skills_context(
                self.config.skills.max_context_tokens,
                &resolved_creds,
                &self.config.skills,
            ) {
                Ok(ctx) => self.cached_skills_context = Some(ctx),
                Err(e) => warn!("Failed to load skills context: {e}"),
            }
        }
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

        let mut messages = vec![Message::system(&system_prompt)];
        messages.extend(self.history.clone());

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

        let (sequential, parallel): (Vec<_>, Vec<_>) = tc
            .iter()
            .partition(|t| t.function.name == "run_shell" || t.function.name == "browser");

        self.run_tool_calls(&parallel, event_tx, cancel).await;
        self.run_tool_calls(&sequential, event_tx, cancel).await;
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
            "read_file" => return tool_handlers::handle_read_file(&args, &self.config),
            "browser" => {
                return tool_handlers::handle_browser(
                    &args,
                    &self.config,
                    &mut self.browser_session,
                )
                .await;
            }
            "text_to_speech" => {
                if let Some(ref synth) = self.tts_synthesizer {
                    return Ok(tool_handlers::handle_text_to_speech(&args, synth).await);
                }
                return Ok(ToolOutput::Text(
                    "TTS is not configured. Enable it via: borg settings set tts.enabled true"
                        .into(),
                ));
            }
            "request_user_input" => {
                return tool_handlers::handle_request_user_input(&args, event_tx).await;
            }
            _ => {}
        }

        // Tools that return Result<String> (wrapped to ToolOutput::Text below)
        let text_result: Result<String> = match name {
            // Memory
            "write_memory" => crate::tool_dispatch::handle_write_memory_with_effects(
                &args,
                &self.config,
                &self.config_arc,
                &mut self.cached_identity,
            ),
            "read_memory" => tool_handlers::handle_read_memory(&args),
            "memory_search" => tool_handlers::handle_memory_search(&args, &self.config).await,
            // Resource listing
            "list" => tool_handlers::handle_list(&args, &self.config, self.agent_control.as_ref()),
            "list_skills" => tool_handlers::handle_list_skills(&self.config),
            "list_channels" => tool_handlers::handle_list_channels(&self.config),
            // File operations
            "apply_patch" => tool_handlers::handle_apply_patch_unified(&args, &self.config),
            "apply_skill_patch" => tool_handlers::handle_apply_skill_patch(&args),
            "create_channel" => tool_handlers::handle_create_channel(&args),
            "list_dir" => tool_handlers::handle_list_dir(&args, &self.config),
            // Shell & web
            "run_shell" => {
                tool_handlers::handle_run_shell(
                    &args,
                    &self.config,
                    &self.policy,
                    event_tx,
                    Some(&self.skill_env_allowlist),
                )
                .await
            }
            "web_fetch" => tool_handlers::handle_web_fetch(&args, &self.config).await,
            "web_search" => tool_handlers::handle_web_search(&args, &self.config).await,
            // Scheduling & projects
            "schedule" | "manage_tasks" | "manage_cron" => {
                tool_handlers::handle_schedule(&args, &self.config)
            }
            "projects" => tool_handlers::handle_projects(&args, &self.config),
            // Media
            "generate_image" => tool_handlers::handle_generate_image(&args, &self.config).await,
            // Multi-agent tools
            name @ ("spawn_agent" | "send_to_agent" | "wait_for_agent" | "list_agents"
            | "close_agent" | "manage_roles") => {
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

    // ── Prompt cache: stable prefix ordering ──
    //
    // `build_system_prompt` must emit its stable (cache-friendly) sections
    // BEFORE the dynamic environment section, otherwise per-turn content
    // (time, git status) invalidates the cached prefix. These guard tests
    // read the source of `build_system_prompt` and fail if the order is
    // perturbed. The full end-to-end behavior is exercised indirectly
    // through the Anthropic request tests in `llm.rs`.

    const AGENT_RS_SRC: &str = include_str!("agent.rs");

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
    fn workflow_guidance_template_is_not_empty() {
        assert!(
            !WORKFLOW_GUIDANCE.trim().is_empty(),
            "workflow_guidance.md template must not be empty",
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
    fn require_str_param_empty_string() {
        let args = serde_json::json!({"name": ""});
        let val = tool_handlers::require_str_param(&args, "name").unwrap();
        assert_eq!(val, "");
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
    fn classify_action_run_shell() {
        assert!(matches!(
            classify_action("run_shell"),
            ActionType::ShellCommand
        ));
    }

    #[test]
    fn classify_action_apply_patch() {
        assert!(matches!(
            classify_action("apply_patch"),
            ActionType::FileWrite
        ));
    }

    #[test]
    fn classify_action_write_memory() {
        assert!(matches!(
            classify_action("write_memory"),
            ActionType::MemoryWrite
        ));
    }

    #[test]
    fn classify_action_web_fetch() {
        assert!(matches!(
            classify_action("web_fetch"),
            ActionType::WebRequest
        ));
    }

    #[test]
    fn classify_action_unknown_tool() {
        assert!(matches!(
            classify_action("unknown_tool"),
            ActionType::ToolCall
        ));
    }

    // -- is_mutating_tool (allowlist-based) --

    #[test]
    fn mutating_tools_are_blocked_in_plan_mode() {
        // These should be considered mutating
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

    // -- New AgentEvent variants --

    #[test]
    fn steer_received_event_variant_exists() {
        let event = AgentEvent::SteerReceived {
            text: "adjust approach".into(),
        };
        assert!(matches!(event, AgentEvent::SteerReceived { .. }));
    }

    #[test]
    fn plan_updated_event_variant_exists() {
        let event = AgentEvent::PlanUpdated { steps: vec![] };
        assert!(matches!(event, AgentEvent::PlanUpdated { .. }));
    }

    #[test]
    fn user_input_request_event_variant_exists() {
        let (tx, _rx) = tokio::sync::oneshot::channel::<String>();
        let event = AgentEvent::UserInputRequest {
            prompt: "Which DB?".into(),
            respond: tx,
        };
        assert!(matches!(event, AgentEvent::UserInputRequest { .. }));
    }

    // -- update_plan tool in plan mode --

    #[test]
    fn request_user_input_is_mutating() {
        // request_user_input blocks execution, so it should be blocked in plan mode
        assert!(is_mutating_tool("request_user_input"));
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

    // -- should_nudge_for_response tests --

    #[test]
    fn nudge_when_empty_after_tools() {
        assert!(should_nudge_for_response("", true, false));
        assert!(should_nudge_for_response("  \n  ", true, false));
    }

    #[test]
    fn no_nudge_when_text_present() {
        assert!(!should_nudge_for_response("Done!", true, false));
    }

    #[test]
    fn no_nudge_when_no_tools_executed() {
        assert!(!should_nudge_for_response("", false, false));
    }

    #[test]
    fn no_nudge_when_already_retried() {
        assert!(!should_nudge_for_response("", true, true));
    }

    // -- collaboration mode template tests --

    #[test]
    fn default_mode_requires_task_confirmation() {
        let template = include_str!("../templates/collaboration_mode/default.md");
        assert!(
            template.contains("always provide a brief text response"),
            "Default collaboration mode must instruct agent to confirm task completion"
        );
    }

    #[test]
    fn execute_mode_requires_task_confirmation() {
        let template = include_str!("../templates/collaboration_mode/execute.md");
        assert!(
            template.contains("Never end a turn silently"),
            "Execute collaboration mode must instruct agent to never end silently"
        );
    }

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

/// Map a tool name to an action type for rate limiting.
/// Returns true if the tool performs mutations (file writes, shell commands, etc.).
/// Used to block mutating tools in Plan mode.
///
/// Uses an allowlist of known-safe tools so that new tools default to blocked,
/// preventing accidental mutation in plan mode.
fn is_mutating_tool(name: &str) -> bool {
    !matches!(
        name,
        "read_file"
            | "list_dir"
            | "list"
            | "list_skills"
            | "list_channels"
            | "list_agents"
            | "read_memory"
            | "memory_search"
            | "web_fetch"
            | "web_search"
    )
}

fn classify_action(tool_name: &str) -> ActionType {
    match tool_name {
        "run_shell" => ActionType::ShellCommand,
        "apply_patch" | "apply_skill_patch" | "create_channel" => ActionType::FileWrite,
        "write_memory" => ActionType::MemoryWrite,
        "memory_search" | "read_memory" => ActionType::ToolCall,
        "web_fetch" | "web_search" | "browser" | "text_to_speech" | "generate_image" => {
            ActionType::WebRequest
        }
        _ => ActionType::ToolCall,
    }
}
