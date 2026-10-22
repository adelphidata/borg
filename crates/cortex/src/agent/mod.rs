#[cfg(test)]
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info_span, instrument, warn};

use crate::config::Config;
use crate::constants;
use crate::conversation::{
    compact_tool_results, history_tokens, normalize_history, rewind_to_nth_user, undo_last_turn,
};
use crate::db::Database;
use crate::hooks::{HookData, HookPoint, HookRegistry};
use crate::internal_tag_filter::InternalTagFilter;
use crate::llm::llm_error::{classify_error_text, FailoverReason};
use crate::llm::{LlmClient, StreamEvent, UsageData};
use crate::policy::ExecutionPolicy;
use crate::rate_guard::SessionRateGuard;
use crate::session::Session;
use crate::telemetry::BorgMetrics;
#[cfg(test)]
use crate::tool::names as tn;
#[cfg(test)]
use crate::tool_handlers;
use crate::types::{Message, MessageContent, Role, ToolDefinition};

pub mod btw;
mod credentials;
mod goal_loop;
mod hooks_dispatch;
mod interrupted;
pub mod media_markers;
mod prompt_assembly;
mod session_manager;
pub mod silence;
pub mod steer;
mod stream_handler;
mod system_prompt;
pub(crate) mod system_prompt_cache;
mod title_generator;
mod tool_classification;
mod tool_execution;
mod tool_markers;
mod tool_validation;

use stream_handler::{StreamEventOutcome, TextOnlyOutcome};
use system_prompt_cache::SystemPromptCache;

pub use btw::{parse_btw_command, run_btw};
pub use prompt_assembly::{ContextBreakdown, ContextComponent};
pub use tool_classification::{conditionally_mutating_tool_names, mutating_tool_names};

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

/// Maximum number of parallel tool calls allowed in a single LLM response.
/// Prevents OOM from malformed stream events with huge indices.
pub(super) const MAX_TOOL_CALLS: usize = constants::MAX_AGENT_TOOL_CALLS;

/// Result of monthly token budget check.
enum BudgetCheck {
    Ok,
    Warning(String),
    Exceeded(String),
}

/// Events to send for a budget check, and whether the turn must stop.
///
/// A warning is a notice, not an error: the turn goes on and succeeds, and a
/// headless caller maps an error event to a failed exit code.
fn budget_check_events(check: BudgetCheck) -> (Vec<AgentEvent>, bool) {
    match check {
        BudgetCheck::Ok => (Vec::new(), false),
        BudgetCheck::Warning(text) => (
            vec![AgentEvent::SystemNotice {
                category: "budget".to_string(),
                text,
            }],
            false,
        ),
        BudgetCheck::Exceeded(msg) => {
            (vec![AgentEvent::Error(msg), AgentEvent::TurnComplete], true)
        }
    }
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

/// A preview image attached to a [`AgentEvent::ToolResult`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultImage {
    /// MIME type of `data` (JPEG after preview compression, PNG when it already fit).
    pub mime_type: String,
    /// Base64-encoded image bytes.
    pub data: String,
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
        /// Small previews of any images the tool returned (a screenshot from
        /// `computer_use`, an MCP image), for the operator's transcript. The
        /// model gets the originals through the tool-result message; these
        /// are capped so the per-session event ring stays small.
        images: Vec<ToolResultImage>,
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
    /// An LLM request attempt went out on the wire — the first try, a retry, or
    /// a provider-failover leg. Carries the retry index within the current
    /// provider leg (0 for the first try).
    ///
    /// Purely a liveness signal; it says nothing about the response. Consumers
    /// that render progress should ignore it. It exists so the sub-agent stall
    /// watchdog can tell a slow provider from a wedged child — see
    /// [`crate::multi_agent`]'s `StallWatchdog::on_event`.
    LlmAttemptStarted { attempt: u32 },
    /// The agent discarded visible text that it already sent as
    /// [`Self::TextDelta`]: the attempt that produced it failed and a new
    /// attempt sends the full response again.
    ///
    /// `bytes` is the UTF-8 length of the discarded deltas exactly as they
    /// were sent (after the internal-tag filter and redaction). A consumer
    /// that accumulates deltas removes that many bytes from the end of its
    /// buffer — see [`retract_text_tail`]. Never sent for a first attempt, a
    /// new tool iteration, or a retry that discards no visible text.
    TextRetracted { bytes: usize },
    /// The connection to the provider dropped and the client is waiting before
    /// it tries again. Unlike [`Self::LlmAttemptStarted`] this one *is* for
    /// display: the wait is long enough that silence would read as a hang.
    LlmReconnecting {
        /// Retry index within the current provider leg.
        attempt: u32,
        /// How long the client will wait before the next attempt.
        delay_secs: u64,
    },
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

/// Apply [`AgentEvent::TextRetracted`] to a buffer of accumulated text deltas:
/// remove the last `bytes` bytes.
///
/// The cut moves down to a char boundary, and a buffer shorter than `bytes`
/// becomes empty, so a consumer that holds less text than the agent sent (a
/// size cap, a late subscription) never panics.
pub fn retract_text_tail(buffer: &mut String, bytes: usize) {
    let mut cut = buffer.len().saturating_sub(bytes);
    while !buffer.is_char_boundary(cut) {
        cut -= 1;
    }
    buffer.truncate(cut);
}

/// Returns true if the agent should retry the LLM call because it returned
/// no visible text (safety net for silent completions).
///
/// `needs_response` (tool calls ran this turn) is deliberately NOT required:
/// a first-iteration reply that is pure reasoning with no text ended the turn
/// in total silence — no message, no error, nothing for the user to act on.
/// One nudge is the cheapest recovery, and `already_nudged` keeps it
/// single-shot so a persistently-empty model can't loop.
///
/// An intentionally silent autonomous turn is unaffected: those carry a
/// silence marker (`[SILENT]` / `HEARTBEAT_OK`, see [`silence`]) as real text,
/// so they are never empty here.
fn should_nudge_for_response(
    text_content: &str,
    _needs_response: bool,
    already_nudged: bool,
) -> bool {
    text_content.trim().is_empty() && !already_nudged
}

/// Maximum bytes of reasoning retained for the empty-response terminal.
/// Large enough to carry a concluding thought, small enough that a
/// long-thinking model can't balloon the agent's footprint.
pub(super) const REASONING_BUFFER_CAP: usize = 4096;

/// Label prefixing the reasoning excerpt delivered when a turn ends with no
/// visible text. Explicit about provenance so the excerpt is never mistaken
/// for a normal answer.
pub(super) const EMPTY_RESPONSE_REASONING_LABEL: &str =
    "[No visible answer was produced. The model's reasoning ended with:]";

/// Terminal message when a turn produced neither text nor reasoning. Silence
/// leaves the user unable to tell a finished turn from a broken one.
pub(super) const EMPTY_RESPONSE_NO_REASONING: &str =
    "[The model returned an empty response. Try rephrasing, or check the provider status.]";

/// Marker persisted as the assistant turn when the model produced no visible
/// text. The live display may carry a labeled reasoning excerpt
/// ([`EMPTY_RESPONSE_REASONING_LABEL`]), but raw reasoning must NEVER reach
/// persisted assistant content: replayed on later requests it reads to
/// providers as injected chain-of-thought, which can deterministically yield
/// another empty response — re-triggering this very path and wedging the
/// session for good.
pub(super) const EMPTY_RESPONSE_PERSISTED_MARKER: &str =
    "[No visible answer was produced this turn.]";

use tool_validation::UnknownToolTracker;
use tool_validation::{
    partition_tool_calls, ExpensiveToolBudget, PartialToolCall, ToolCallRepeatTracker,
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
    /// Set when a session write failed because durable storage is exhausted
    /// (disk full, quota, read-only mount). Checked by the agent loop, which
    /// aborts the turn rather than let the user believe a reply was saved.
    /// `None` in the normal case; transient DB contention never sets it.
    pub(super) storage_fault: Option<String>,
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
    /// Refuses steers once the turn has made its last check (see `steer`).
    pub(super) steer_gate: Option<Arc<steer::SteerGate>>,
    /// Tool names used in recent turns, for conditional tool loading.
    pub(super) recent_tool_names: std::collections::HashSet<String>,
    /// Optional best-effort sink for borgd cost events.
    pub(super) cost_event_sink: Option<Arc<dyn crate::llm::CostEventSink>>,
    /// Short-term working memory for the current session.
    pub(super) short_term_memory: crate::memory::short_term_memory::ShortTermMemory,
    /// Tracks runs of identical consecutive tool calls within a turn —
    /// drives the exponential-backoff-then-block ladder. Reset at the
    /// start of each `run_agent_loop`.
    pub(super) tool_call_repeat: ToolCallRepeatTracker,
    /// Per-turn call budgets for expensive tools (web search, sub-agent
    /// spawns) whose distinct-argument volume the identical-call guard
    /// cannot bound. Reset at turn start.
    pub(super) expensive_tool_budget: ExpensiveToolBudget,
    /// Per-turn count of attempts on each hallucinated tool name. The first
    /// `unknown_tool_max_per_name - 1` attempts surface the soft
    /// `"Unknown tool"` error so a typo can be corrected; the next strikes
    /// substitute a terminal refusal so the model breaks out of the retry
    /// cycle instead of burning iteration budget. Reset at the start of
    /// each `run_agent_loop`.
    pub(super) unknown_tool_tracker: UnknownToolTracker,
    /// Tail of the reasoning/thinking text streamed during the current
    /// iteration, capped at [`REASONING_BUFFER_CAP`].
    ///
    /// Display-only and never persisted to history — the sole consumer is the
    /// empty-response terminal, where the model produced reasoning but no
    /// visible answer and the reasoning frequently *contains* the answer.
    /// Buffered regardless of `show_thinking` because the terminal excerpt is
    /// a fallback for a broken turn, not a thinking-trace feature.
    pub(super) turn_reasoning: String,
    /// Signed Anthropic reasoning blocks of the current stream, verbatim, in
    /// wire order. Attached to the assistant tool-call message so the next
    /// request of the same tool loop can replay them. Scoped to one stream.
    pub(super) turn_reasoning_blocks: Vec<serde_json::Value>,
    /// Full `reasoning_content` of the current stream, uncapped (unlike
    /// [`Self::turn_reasoning`], which keeps a display tail). Attached to
    /// the assistant tool-call message for providers that require it
    /// echoed. Scoped to one stream.
    pub(super) turn_reasoning_content: String,
    /// The model the current stream's usage record names: the model that
    /// actually served it, after any failover. Tags the stored reasoning
    /// so replay stays bound to the model that produced it.
    pub(super) turn_reasoning_model: Option<String>,
    /// Provider signalled tool use for the current stream (`finish_reason ==
    /// "tool_calls"` / `stop_reason == "tool_use"`). Scoped to one iteration —
    /// cleared where the stream collectors re-bind. Lets the text-only path
    /// tell "the model chose to answer in text" apart from "the model
    /// signalled a tool call whose payload never arrived".
    pub(super) tool_intent_signalled: bool,
    /// UTF-8 length of the text deltas sent to consumers in the current
    /// iteration and not retracted. It is the size of the retraction when
    /// the iteration's output is discarded.
    pub(super) emitted_text_bytes: usize,
    /// Once-per-turn guard for the dropped-tool-call nudge, so a provider
    /// that persistently signals tool use without shipping a call can't
    /// loop the nudge. Reset at turn start with the other loop guards.
    pub(super) nudged_for_dropped_tools: bool,
    /// Which LLM chain this agent's `make_llm_client_from_config` builds.
    /// `Primary` is the default — interactive turns and tool-spawned
    /// sub-agents always run on the user's primary chain. `Auxiliary` is
    /// set by [`Agent::new_sub_agent_aux`] for background work so a
    /// primary outage doesn't take down nightly cron.
    pub(super) llm_role: LlmRole,
    /// Volatile runtime state for the per-session goal: in-memory
    /// accounting baseline, queued steering messages, and the
    /// continuation reservation. The DB row is the source of truth;
    /// this struct caches the slice of state that doesn't survive a
    /// process restart. See `crates/cortex/src/goals/runtime.rs`.
    pub(super) goal_runtime: crate::goals::runtime::GoalRuntimeState,
    /// Optional best-effort sink for goal lifecycle events. The daemon
    /// implements this by forwarding onto the `SubscribeGoalEvents`
    /// broadcast channel; cortex never panics if it is `None`.
    pub(super) goal_event_sink: Option<Arc<dyn crate::goals::GoalEventSink>>,
    /// One-shot flag: set after we've spawned the aux-LLM auto-title task
    /// for this session so a multi-iteration first turn (tool calls,
    /// continuations) doesn't spawn it repeatedly. Reset by `new_session`.
    pub(super) auto_title_spawned: bool,
    /// Provider/model that actually served the most recent response when a
    /// failover occurred, as `(provider, model)`. Reset to `None` at the start
    /// of each iteration's stream and re-set by a [`crate::llm::StreamEvent::Failover`];
    /// read by `build_environment_section` so the model reports the model that
    /// served it rather than the configured primary during an outage.
    /// In-memory only — never persisted.
    pub(super) active_failover_identity: Option<(String, String)>,
    /// Anti-thrash guard for proactive LLM compaction: counts consecutive
    /// passes that failed to bring the prompt under threshold and suspends
    /// further proactive attempts once the cap is hit. The reactive
    /// (provider context-overflow) path is never gated. See
    /// [`prompt_assembly::CompactionStrikes`].
    pub(super) compaction_strikes: prompt_assembly::CompactionStrikes,
    /// Set when the summarizer hit a provider outage that retrying cannot fix
    /// (bad key, exhausted quota). Suspends *proactive* compaction — which
    /// would otherwise re-pay for the identical failure every turn — and
    /// records that the user has already been told. Cleared only by a
    /// compaction pass that actually drops messages (a healthy main stream
    /// proves nothing: `[compaction]` overrides can point the summarizer at
    /// its own provider and key), and re-probed on a cooldown so a restored
    /// credential resumes compaction without a restart. See
    /// [`prompt_assembly::CompactionOutage`]. The reactive (context-overflow)
    /// path stays ungated, so a real overflow still surfaces a hard error
    /// rather than silently stalling.
    pub(super) compaction_outage: Option<prompt_assembly::CompactionOutage>,
    /// Cross-turn stale-stream circuit breaker: counts consecutive streams
    /// killed as stale/timed out with nothing usable received, and
    /// short-circuits turns once tripped. See
    /// [`stream_handler::StreamHealth`].
    pub(super) stream_health: stream_handler::StreamHealth,
    /// When set (tool-spawned sub-agents), used as the OpenAI-family
    /// `prompt_cache_key` instead of this agent's own session ID, so the
    /// parent and its spawns — which share most of their prompt prefix —
    /// route to the same provider cache shard.
    pub(super) prompt_cache_scope: Option<String>,
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
///
/// Credential resolution is deliberately deferred to first-turn
/// `make_llm_client_from_config`. Validating at session-open made the TUI
/// unreachable when no API key was configured (Session.Open fails →
/// `/settings` lives inside the TUI → catch-22). The per-turn construction
/// path bubbles missing-credential errors via `?` up to `run_agent_loop`,
/// where `AgentBackend::run_turn` (in `borgd::session::backend`) catches
/// them and emits an `AgentEvent::Error(chained)` that the TUI renders
/// inline — same channel as any other turn-time failure.
async fn build_common(config: &Config) -> Result<AgentCommon> {
    let policy = config.policy.clone();
    let session = Session::new();
    let db = match Database::open().await {
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
        // Seed the process-wide summarizer-outage mirror from its durable
        // copy so a cooldown latched before a daemon restart keeps counting
        // down instead of re-probing the dead summarizer at full price.
        prompt_assembly::hydrate_compaction_outage(d).await;
        if let Ok(state) = d.get_evolution_state().await {
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
    // No workspace is known yet at construction; `set_execution_context`
    // recomputes the allowlist once the session's workspace arrives, so
    // project-tier skills' `requires.env` still reach `run_shell`.
    let skill_env_allowlist =
        crate::skills::collect_required_env_vars(&resolved_creds, &config.skills, None);
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
    /// Acquire the database lock. With `tokio::sync::Mutex` there is no
    /// poisoning, so this is a thin async wrapper.
    pub(super) async fn db_guard(&self) -> tokio::sync::MutexGuard<'_, Option<Database>> {
        self.db.lock().await
    }

    pub(super) fn latest_user_message_text(&self) -> &str {
        crate::conversation::latest_user_text(&self.history)
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
        let tool_call_repeat = ToolCallRepeatTracker::new(&config.loop_guard);
        let unknown_tool_tracker = UnknownToolTracker::new(&config.loop_guard);
        let expensive_tool_budget = ExpensiveToolBudget::new(&config.loop_guard);
        Self {
            config,
            history: Vec::new(),
            session: common.session,
            policy: common.policy,
            hook_registry: HookRegistry::new(),
            turn_count: 0,
            storage_fault: None,
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
            steer_gate: None,
            recent_tool_names: std::collections::HashSet::new(),
            cost_event_sink: None,
            short_term_memory: crate::memory::short_term_memory::ShortTermMemory::new(
                session_id, 2000,
            ),
            tool_call_repeat,
            expensive_tool_budget,
            unknown_tool_tracker,
            turn_reasoning: String::new(),
            turn_reasoning_blocks: Vec::new(),
            turn_reasoning_content: String::new(),
            turn_reasoning_model: None,
            tool_intent_signalled: false,
            emitted_text_bytes: 0,
            nudged_for_dropped_tools: false,
            llm_role,
            goal_runtime: crate::goals::runtime::GoalRuntimeState::new(),
            goal_event_sink: None,
            auto_title_spawned: false,
            active_failover_identity: None,
            compaction_strikes: prompt_assembly::CompactionStrikes::new(),
            // Inherit any live outage from the process-wide mirror. borgd
            // builds a fresh `Agent` per scheduled/heartbeat turn; starting at
            // `None` would re-probe a known-dead summarizer on every one of
            // them and the cooldown would never elapse.
            compaction_outage: prompt_assembly::load_compaction_outage(),
            stream_health: stream_handler::StreamHealth::new(),
            prompt_cache_scope: None,
        }
    }

    /// Route this agent's LLM requests to the given prompt-cache scope
    /// (the spawning tree's root session) instead of its own session ID.
    /// Also re-scopes this agent's own controller so any grandchildren it
    /// spawns inherit the same scope rather than starting a new one.
    pub fn set_prompt_cache_scope(&mut self, scope: String) {
        if let Some(ref mut control) = self.agent_control {
            control.set_prompt_cache_scope(scope.clone());
        }
        self.prompt_cache_scope = Some(scope);
    }

    /// OpenAI-family `prompt_cache_key` for this agent's requests: the
    /// spawn-time scope when present (sub-agents share the parent session's
    /// key), otherwise this agent's own session ID.
    pub(super) fn prompt_cache_key(&self) -> String {
        self.prompt_cache_scope
            .clone()
            .unwrap_or_else(|| self.session.meta.id.clone())
    }

    pub async fn new(config: Config, metrics: BorgMetrics) -> Result<Self> {
        let common = build_common(&config).await?;
        let agent_control = if config.agents.enabled {
            Some(crate::multi_agent::AgentControl::new(
                &config.agents,
                &common.session.meta.id,
                0,
            ))
        } else {
            None
        };

        Self::prune_embedding_cache_on_startup(&config).await;

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
    async fn prune_embedding_cache_on_startup(config: &Config) {
        let ttl_days = config.memory.cache_ttl_days;
        let max_entries = config.memory.cache_max_entries;

        if ttl_days == 0 && max_entries == 0 {
            return;
        }

        let db = match Database::open().await {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!("Cache pruning: failed to open database: {e}");
                return;
            }
        };

        if ttl_days > 0 {
            let max_age_secs = i64::from(ttl_days) * constants::SECS_PER_DAY;
            match db.prune_embedding_cache(max_age_secs).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(
                    "Pruned {n} stale embedding cache entries (>{ttl_days} days old)"
                ),
                Err(e) => tracing::warn!("Cache TTL pruning failed: {e}"),
            }
        }

        if max_entries > 0 {
            match db.prune_embedding_cache_by_count(max_entries).await {
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

    /// Gate the steer channel on this agent's turns: open while one runs,
    /// closed from its last steer check on. Senders go through
    /// [`steer::SteerGate::send`].
    pub fn set_steer_gate(&mut self, gate: Arc<steer::SteerGate>) {
        self.steer_gate = Some(gate);
    }

    /// Set a config watch receiver for hot reload.
    pub fn set_config_watcher(&mut self, rx: tokio::sync::watch::Receiver<Config>) {
        self.config_rx = Some(rx);
    }

    /// Install a best-effort cost-event sink for all session-backed LLM calls.
    pub fn set_cost_event_sink(&mut self, sink: Arc<dyn crate::llm::CostEventSink>) {
        self.cost_event_sink = Some(sink);
    }

    /// Install a best-effort goal-event sink. The daemon implements this by
    /// forwarding into the `SubscribeGoalEvents` broadcast channel so the
    /// TUI footer and any future remote dashboard can react in real time.
    pub fn set_goal_event_sink(&mut self, sink: Arc<dyn crate::goals::GoalEventSink>) {
        self.goal_event_sink = Some(sink);
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
        // Mirror into the controller so sub-agents inherit this confinement.
        // Sub-agents are granted `auto_approve: ["*"]` precisely because the
        // sandbox is expected to still bound them; without this the child
        // would run with a fresh unrestricted context.
        if let Some(ref mut control) = self.agent_control {
            control.set_execution_context(execution_context.clone());
        }
        self.execution_context = execution_context;
        self.git_repo_root = crate::git::find_repo_root(self.execution_context.workspace_cwd());
        self.caches.clear_project_docs();
        // The workspace decides which project-tier skills exist, and their
        // `requires.env` feeds the run_shell credential allowlist — recompute
        // it so a project skill's declared credential is actually forwarded.
        self.ensure_credentials_cached();
        self.skill_env_allowlist = crate::skills::collect_required_env_vars(
            self.credentials_cached(),
            &self.config.skills,
            Some(self.execution_context.workspace_root()),
        );
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
        // A model/provider switch is the operator's escape hatch for a
        // wedged session — give the new target a clean slate on the
        // stale-stream circuit breaker instead of inheriting the old
        // target's streak.
        if new_config.llm.provider != self.config.llm.provider
            || new_config.llm.model != self.config.llm.model
        {
            self.stream_health.record_provider_switch();
        }
        self.policy = new_config.policy.clone();
        self.rate_guard
            .update_limits(new_config.security.action_limits.clone());
        let resolved_creds = new_config.resolve_credentials();
        self.caches.on_config_reloaded(resolved_creds);
        // A rename rewrites IDENTITY.md (or the default it falls back to).
        if new_config.user.agent_name != self.config.user.agent_name {
            self.caches
                .on_memory_written("global", crate::constants::IDENTITY_FILE);
        }
        self.config_arc = Arc::new(new_config.clone());
        self.config = new_config;
        // skills caches were just cleared by `on_config_reloaded`;
        // recompute the env allowlist now that credentials are fresh.
        self.skill_env_allowlist = crate::skills::collect_required_env_vars(
            self.credentials_cached(),
            &self.config.skills,
            Some(self.execution_context.workspace_root()),
        );
    }

    async fn make_llm_client_from_config(&self, config: &Config) -> Result<LlmClient> {
        // Every client this agent builds — main loop, compaction, aux —
        // carries the session-scoped cache/routing key. Background calls
        // without it route independently of the conversation they serve
        // (no provider stickiness, no shard-stable auto-caching).
        let mut client = match self.llm_role {
            LlmRole::Primary => LlmClient::new(config).await?,
            LlmRole::Auxiliary => LlmClient::for_auxiliary(config).await?,
        }
        .with_prompt_cache_key(self.prompt_cache_key());
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
    pub async fn new_sub_agent(
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
        .await
    }

    /// Create a sub-agent that routes through the auxiliary LLM chain
    /// (`[llm.aux]`). Use for background work — memory consolidation,
    /// scheduled prompts, heartbeat — so a primary-provider outage can't
    /// take down nightly cron. Tool-spawned sub-agents and interactive
    /// turns must keep using [`Agent::new_sub_agent`] (Primary).
    pub async fn new_sub_agent_aux(
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
        .await
    }

    async fn new_sub_agent_with_role(
        config: Config,
        spawn_depth: u32,
        agents_config: &crate::config::MultiAgentConfig,
        metrics: BorgMetrics,
        tools_filter: Option<Vec<String>>,
        llm_role: LlmRole,
    ) -> Result<Self> {
        let common = build_common(&config).await?;
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

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The config the next turn will run with. Hot reload applies only at the
    /// start of a turn, so between turns [`Agent::config`] can trail a
    /// settings change; anything that reports the session's provider or model
    /// to a user must read this instead, or it names a model that will not
    /// answer.
    pub fn next_turn_config(&self) -> Config {
        match &self.config_rx {
            Some(rx) => rx.borrow().clone(),
            None => self.config.clone(),
        }
    }

    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.config
    }

    pub fn metrics(&self) -> &BorgMetrics {
        &self.metrics
    }

    /// Compact conversation history via LLM summarization. Returns (before, after) message counts.
    #[instrument(skip_all, fields(session_id = %self.session.meta.id))]
    pub async fn compact(&mut self) -> (usize, usize) {
        let before = history_tokens(&self.history);
        // Same lane as the automatic paths: the compaction-tuned client
        // (operator overrides + a summary output-cap floor) with the
        // primary-model fallback, so a manual /compact can't truncate the
        // summary mid-section or wedge on a dead override any more than
        // they can.
        let max_hist = self.effective_history_budget();
        let (dropped, failure) = match self.run_compaction_with_fallback(max_hist).await {
            Ok(outcome) => outcome,
            Err(e) => {
                warn!("Manual compaction could not build a summarizer client: {e}");
                return (before, before);
            }
        };
        // No event channel here — the caller renders the before/after counts.
        // Still record the outcome so a persistent outage latches (and gets
        // announced by the next path that can reach the user) instead of
        // silently reporting "compacted 0".
        self.record_compaction_outcome(dropped, failure, None).await;
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

    // `build_system_prompt` and the per-section builders live in
    // `agent::system_prompt`.

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
        self.log_and_persist(msg).await;
        self.turn_count += 1;
        // Reset recent tool names each user turn so conditional loading
        // only considers tools used in the current turn's agent loop.
        self.recent_tool_names.clear();
        self.run_agent_loop(event_tx, cancel).await
    }

    /// Resume the turn that was cut short, without injecting a user message.
    ///
    /// The last history message must be an assistant message ending in an
    /// interruption marker (`[response interrupted]` and friends). The marker
    /// is dropped from the **in-memory** history only, so the model continues
    /// from its own partial text; the persisted row keeps it as the record of
    /// what happened.
    ///
    /// Returns an error naming the next action when there is nothing to
    /// resume, and leaves the history untouched in that case.
    #[instrument(skip_all, fields(session_id = %self.session.meta.id, turn_count = self.turn_count))]
    pub async fn resume_interrupted_turn(
        &mut self,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let last = self
            .history
            .last()
            .ok_or_else(|| anyhow!("This conversation is empty. Send a message to start it."))?;

        // A turn stopped before its first token leaves the user's message last.
        if last.role == Role::User {
            return Err(anyhow!(
                "The turn was stopped before any reply was written, so there is nothing to resume. Send the request again."
            ));
        }
        let partial = match (&last.role, last.text_content()) {
            (&Role::Assistant, Some(text)) => interrupted::strip_interruption_marker(text),
            _ => None,
        }
        .ok_or_else(|| {
            anyhow!("The last reply finished normally, so there is nothing to resume. Send a message to continue.")
        })?;

        if partial.trim().is_empty() {
            return Err(anyhow!(
                "The interrupted reply produced no text to continue from. Send the request again."
            ));
        }

        // In-memory only: the DB row keeps the marker.
        let Some(last) = self.history.last_mut() else {
            return Err(anyhow!(
                "This conversation is empty. Send a message to start it."
            ));
        };
        last.content = Some(MessageContent::Text(partial));

        self.turn_count += 1;
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
        self.log_and_persist(msg).await;
        self.turn_count += 1;
        self.run_agent_loop(event_tx, cancel).await
    }

    #[instrument(skip_all, fields(session_id = %self.session.meta.id, turn_count = self.turn_count))]
    /// One-time setup before entering the agent loop: ghost commit, project doc caching,
    /// and background indexing of extra memory paths and past sessions.
    async fn prepare_loop(&mut self) {
        // A storage fault only aborts the turn it occurred in. Clearing it
        // here means a fault from *between* turns (a failed `auto_save`)
        // doesn't spuriously kill a turn that would have succeeded on a
        // since-freed disk — and if the disk is still full, this turn's first
        // persist re-raises it immediately.
        self.storage_fault = None;

        // Create ghost commit for coding safety (lazy, one-time)
        if self.ghost_commit.is_none() {
            if let Some(ref root) = self.git_repo_root {
                match crate::git::create_ghost_commit(root).await {
                    Ok(gc) => {
                        tracing::info!(commit_id = %gc.commit_id, "Created ghost commit for session");
                        {
                            let guard = self.db.lock().await;
                            if let Some(ref db) = *guard {
                                crate::activity_log::log_activity(
                                    db,
                                    "info",
                                    "session",
                                    &format!("Ghost commit created: {}", gc.commit_id),
                                )
                                .await;
                            }
                        }
                        self.ghost_commit = Some(gc);
                    }
                    Err(e) => {
                        tracing::warn!("Skipping ghost commit (no undo point for this turn): {e:#}")
                    }
                }
            }
        }

        // Cache project docs on first run (the system-prompt builder also
        // calls this lazily; running it here once outside that hot path
        // means the first build_system_prompt call doesn't pay for IO).
        let _ = self.caches.project_docs_or_load(|| {
            crate::project_doc::discover_project_docs(
                self.execution_context.workspace_cwd(),
                &self.config.security,
            )
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
                            if let Err(e) = crate::memory::embeddings::embed_memory_file_chunked(
                                &config, &filename, &content, "extra",
                            )
                            .await
                            {
                                tracing::warn!(
                                    error = %e,
                                    file = %filename,
                                    "embed_memory_file_chunked failed for extra-path file"
                                );
                            }
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
                    if let Err(e) =
                        crate::session_indexer::index_pending_sessions(&config, 10).await
                    {
                        tracing::warn!(error = %e, "index_pending_sessions failed");
                    }
                });
            }
        }
    }

    /// Check monthly token budget. Returns an error message if exceeded, a warning if near limit, or None.
    async fn check_budget(&self) -> BudgetCheck {
        let budget_limit = self.config.budget.monthly_token_limit;
        if budget_limit == 0 {
            return BudgetCheck::Ok;
        }
        let Ok(db) = Database::open().await else {
            warn!("Budget enforcement skipped: database unavailable");
            return BudgetCheck::Ok;
        };
        let Ok(used) = db.monthly_token_total().await else {
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
    ///
    /// Wraps the loop body with turn-scoped subprocess ownership: a baseline
    /// of already-running registered processes is captured at turn start, and
    /// if the turn is ABANDONED (the cancellation token fired — user /stop,
    /// session cancel/replacement, deadline abort) any process spawned during
    /// the turn is reaped so a backgrounded dev server can't run unbounded
    /// with nobody left to drive it. Normal completion leaves them alive —
    /// surviving the turn is the point of backgrounding.
    pub async fn run_agent_loop(
        &mut self,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        // Stamp the owning session id into the execution context so
        // subprocesses registered during this turn are attributable to this
        // session (the abandoned-turn reap only touches its own session's
        // spawns).
        if self.execution_context.session_id() != Some(self.session.meta.id.as_str()) {
            self.execution_context = self
                .execution_context
                .clone()
                .with_session_id(Some(self.session.meta.id.clone()));
        }
        let turn_baseline = self
            .execution_context
            .process_registry()
            .map(|reg| reg.begin_turn(&self.session.meta.id));

        if let Some(gate) = &self.steer_gate {
            gate.open();
        }
        let leftover_tx = event_tx.clone();
        let result = self.run_agent_loop_inner(event_tx, cancel.clone()).await;
        // A normal end already closed the gate at its last check. An error or
        // a stop did not: close it, and keep what was queued in the history
        // rather than leave it for some later turn to inject out of place.
        if let Some(gate) = &self.steer_gate {
            gate.close();
        }
        self.drain_steers(&leftover_tx).await;
        drop(leftover_tx);
        // Before returning: the daemon announces the settle once this turn's
        // event sender drops, and lists re-read the row on that announcement.
        self.record_turn_on_session_row().await;

        if cancel.is_cancelled() {
            if let (Some(reg), Some(baseline)) = (
                self.execution_context.process_registry().cloned(),
                turn_baseline,
            ) {
                let never_kill = crate::process_registry::compile_never_kill_patterns(
                    &self.config.process_registry.never_kill_patterns,
                );
                let grace = self.config.process_registry.sigterm_grace_secs;
                // The kill path blocks (SIGTERM → liveness poll → SIGKILL);
                // keep it off the async workers. Fire-and-forget is safe: the
                // generation guard inside makes a late reap a no-op once a
                // replacement turn claims the session.
                let blocking = tokio::task::spawn_blocking(move || {
                    reg.reap_abandoned_turn(&baseline, &never_kill, grace)
                });
                let session_id = self.session.meta.id.clone();
                tokio::spawn(async move {
                    match blocking.await {
                        Ok(report) if report.stale_generation => tracing::debug!(
                            session_id = %session_id,
                            "abandoned-turn reap skipped: generation advanced"
                        ),
                        Ok(report) => tracing::debug!(
                            session_id = %session_id,
                            killed = report.killed.len(),
                            exempted_by_pin = report.exempted_by_pin,
                            exempted_by_pattern = report.exempted_by_pattern,
                            exempted_by_kind = report.exempted_by_kind,
                            already_gone = report.already_gone,
                            "abandoned-turn reap complete"
                        ),
                        Err(e) => tracing::warn!(
                            session_id = %session_id,
                            error = %e,
                            "abandoned-turn reap task failed"
                        ),
                    }
                });
            }
        }
        result
    }

    async fn run_agent_loop_inner(
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
        let mut output_cap_retried_this_turn = false;

        // Reset per-turn loop guards at the start of each user turn so a
        // fresh request starting with a previously-blocked call or a
        // previously-refused hallucinated name is not immediately blocked.
        self.tool_call_repeat.reset();
        self.unknown_tool_tracker.reset();
        self.expensive_tool_budget.reset();
        self.nudged_for_dropped_tools = false;

        // Cross-turn stale-stream circuit breaker: a session whose streams
        // keep dying as stale would otherwise burn the full stall timeout
        // (× the client's internal retries) on every turn with nothing to
        // show. Short-circuit with an actionable error instead; alternating
        // turns are let through as a single probe so a recovered provider
        // can re-prove itself (see `StreamHealth`).
        if self.stream_health.should_abort_turn() {
            let msg = stream_handler::wedged_session_error(self.stream_health.streak());
            warn!("{msg}");
            let _ = event_tx.send(AgentEvent::Error(msg)).await;
            let _ = event_tx.send(AgentEvent::TurnComplete).await;
            return Ok(());
        }

        self.prepare_loop().await;

        loop {
            self.maybe_reload_config();

            if cancel.is_cancelled() {
                self.shutdown_sub_agents();
                // Best-effort: receiver (UI) may have dropped during cancellation.
                let _ = event_tx.send(AgentEvent::TurnComplete).await;
                return Ok(());
            }

            // Storage is full: stop before burning another LLM call on a turn
            // whose output cannot be saved. `Err` (not `Ok`) so automated
            // lanes — scheduled tasks, gateway channels — can't book this as a
            // success either.
            if let Some(msg) = self.storage_fault.take() {
                warn!("{msg}");
                let _ = event_tx.send(AgentEvent::Error(msg.clone())).await;
                let _ = event_tx.send(AgentEvent::TurnComplete).await;
                return Err(anyhow::anyhow!(msg));
            }

            self.drain_sub_agent_completions().await;

            let (budget_events, budget_stops_turn) = budget_check_events(self.check_budget().await);
            for event in budget_events {
                let _ = event_tx.send(event).await;
            }
            if budget_stops_turn {
                return Ok(());
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

            let crate::agent::prompt_assembly::PreparedLlmRequest {
                messages,
                tool_defs,
                system_stable_prefix,
            } = match self.prepare_llm_request(iteration).await? {
                Some(r) => r,
                None => {
                    // Pre-flight hook (e.g. budget cap) refused this call.
                    // Surface a system-visible message and end the turn —
                    // mid-stream cancellation is forbidden by approvals.md,
                    // and we never started a stream, so this is the right
                    // place to halt.
                    let refusal = "Budget hard cap reached — agent paused. \
                                   Raise cap in /settings → Budgets, or wait for daily reset.";
                    // The turn did no work, and the refusal below is saved
                    // but not streamed. The error event shows it to the user
                    // and lets a headless caller fail its exit code.
                    let _ = event_tx.send(AgentEvent::Error(refusal.to_string())).await;
                    self.finalize_text_response(refusal, &event_tx).await;
                    return Ok(());
                }
            };
            let tools = if tool_defs.is_empty() {
                None
            } else {
                Some(tool_defs.as_slice())
            };

            // Reset the failover identity for this stream: the prompt for this
            // iteration was already built (line above) from the prior value, so
            // clearing now means a clean primary serve leaves it `None` and the
            // next iteration reports the configured primary again. A failover
            // during this stream re-sets it via `StreamEvent::Failover`.
            self.active_failover_identity = None;
            // Reasoning is scoped to one stream: the empty-response terminal
            // must quote the thinking that led to *this* silence, not a
            // previous iteration's.
            self.turn_reasoning.clear();
            self.turn_reasoning_blocks.clear();
            self.turn_reasoning_content.clear();
            self.turn_reasoning_model = None;
            // Tool intent is likewise per-stream: a prior iteration's signal
            // must not make this iteration's text-only answer look dropped.
            self.tool_intent_signalled = false;
            let llm_start = Instant::now();
            self.metrics.llm_requests.add(1, &[]);
            let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(256);
            let tools_clone = tools.map(<[ToolDefinition]>::to_vec);
            let cancel_clone = cancel.clone();
            let stream_handle = {
                let mut llm_client = self
                    .make_llm_client_from_config(&self.config)
                    .await
                    .context("Failed to initialize LLM client")?
                    .with_system_stable_prefix(system_stable_prefix);
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
            // Text of an earlier iteration stays with the consumers.
            self.emitted_text_bytes = 0;
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
                            self.log_and_persist(Message::assistant(&content)).await;
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

            // Stream-health accounting for the cross-turn stale-stream
            // breaker. A stream killed as stale/timed out with nothing
            // usable received counts one strike; demonstrated responsiveness
            // — a completed stream, or a usable partial (chunks arrived) —
            // clears the streak. Definite provider errors (auth, rate limit,
            // overflow) are neutral: not stale, but not proof of health.
            match &stream_error {
                Some(err) if classify_error_text(err) == Some(FailoverReason::Timeout) => {
                    let usable_partial =
                        !tag_filter.full_clean().trim().is_empty() || !tool_calls.is_empty();
                    if usable_partial {
                        self.stream_health.record_responsive();
                    } else {
                        let streak = self.stream_health.record_stale_kill();
                        warn!(
                            consecutive_stale = streak,
                            "LLM stream killed as stale with no usable output"
                        );
                    }
                }
                Some(_) => {}
                None if received_terminal => self.stream_health.record_responsive(),
                None => {}
            }

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
                    self.retract_emitted_text(&event_tx).await;
                    continue;
                }
            }

            // Output-cap recovery: the response hit max_tokens. Compact once
            // and re-issue the iteration instead of persisting a `[stream
            // interrupted]` marker and discarding the turn's tool calls — a
            // smaller input both frees combined-window budget and reins in
            // degenerate long output. Only when compaction actually reclaims
            // tokens: retrying against an unchanged input would just truncate
            // again. One retry per turn; a second truncation falls through to
            // the marker path in `finish_iteration`.
            if truncated_due_to_length && stream_error.is_none() && !output_cap_retried_this_turn {
                output_cap_retried_this_turn = true;
                let before_tokens = history_tokens(&self.history);
                warn!("Response truncated at max_tokens; compacting history and retrying");
                // Unlike the input-overflow path above, a truncated response
                // is still deliverable — so a compaction failure (e.g. the
                // summarizer client can't be built) degrades to the marker
                // path instead of erroring the whole turn.
                match self.reactive_compact_history(&event_tx).await {
                    Ok(()) => {
                        if history_tokens(&self.history) < before_tokens {
                            // Discard the truncated partial (tag_filter,
                            // tool_calls re-bind at the top of the outer
                            // loop) and retry. The consumers hold the
                            // partial already, so tell them to drop it.
                            self.retract_emitted_text(&event_tx).await;
                            continue;
                        }
                        // Nothing reclaimed — surface the truncation as before.
                    }
                    Err(e) => {
                        warn!("output-cap compaction failed ({e:#}); surfacing truncation marker");
                    }
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
                // The loop-top check catches a fault before the *next* LLM
                // call; this one catches the final assistant message — the
                // one whose loss the user actually experiences — in the same
                // turn it happened, instead of a turn late.
                IterationOutcome::EndTurn => {
                    return match self.storage_fault.take() {
                        Some(msg) => {
                            warn!("{msg}");
                            let _ = event_tx.send(AgentEvent::Error(msg.clone())).await;
                            let _ = event_tx.send(AgentEvent::TurnComplete).await;
                            Err(anyhow::anyhow!(msg))
                        }
                        None => Ok(()),
                    };
                }
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
    ///
    /// Wraps [`Self::finish_iteration_inner`] so goal accounting fires *after*
    /// the LLM stream has finished (and the assistant message is in history)
    /// on every return path, and so an `EndTurn` outcome on an Active goal
    /// can be transparently upgraded to a self-continuation. This avoids
    /// having to thread the accounting/continuation calls through three
    /// separate `return` sites inside the inner function.
    #[allow(clippy::too_many_arguments)]
    async fn finish_iteration(
        &mut self,
        tag_filter: InternalTagFilter,
        tool_calls: Vec<PartialToolCall>,
        stream_error: Option<String>,
        truncated_due_to_length: bool,
        needs_response: bool,
        nudged_for_response: bool,
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> IterationOutcome {
        let outcome = self
            .finish_iteration_inner(
                tag_filter,
                tool_calls,
                stream_error,
                truncated_due_to_length,
                needs_response,
                nudged_for_response,
                event_tx,
                cancel,
            )
            .await;
        // Charge per-iteration accounting once, regardless of outcome. The
        // assistant message (and any tool results) are already in history at
        // this point, so `history_tokens` reflects the cost of THIS LLM
        // call. `account_goal_at_turn_end` is a misnomer historically —
        // accounting is per-iteration, not per-turn, so a multi-iteration
        // turn charges incrementally and the budget-limit flip can happen
        // mid-turn.
        let _ = self.account_goal_at_turn_end().await;
        // Self-continuation only applies to what would otherwise be a real
        // `EndTurn`. A continuation injects a synthetic user message and
        // returns `NudgeAndContinue` to re-enter the loop; the
        // `try_reserve_continuation` guard inside ensures no infinite loop
        // even if the goal stays Active forever.
        match outcome {
            IterationOutcome::EndTurn => {
                // A user steer that arrived after the last tool boundary is
                // still buffered — `drain_steers` only runs at tool boundaries,
                // so a text-only final response would otherwise strand it until
                // an unrelated future turn. Absorb it and re-enter the loop so
                // the model acts on the redirect now. This also resolves the
                // case where a pending steer suppressed the goal continuation
                // below (the user's input supersedes the synthetic nudge).
                if self.absorb_pending_steer_at_turn_end(event_tx).await {
                    return IterationOutcome::NudgeAndContinue;
                }
                let turn_id = format!("{}:{}", self.session.meta.id, self.turn_count);
                if self
                    .maybe_inject_goal_continuation(&turn_id, event_tx, cancel)
                    .await
                {
                    IterationOutcome::NudgeAndContinue
                } else if !self.close_steers_unless_pending() {
                    // A steer landed after the check above; answer it now.
                    self.drain_steers(event_tx).await;
                    IterationOutcome::NudgeAndContinue
                } else {
                    self.maybe_spawn_auto_title();
                    IterationOutcome::EndTurn
                }
            }
            other => other,
        }
    }

    /// Spawn a one-shot background task that asks the auxiliary LLM for a
    /// short, human-readable session title and writes it back to the DB +
    /// session JSON. Runs at most once per session, only after the very
    /// first user→assistant exchange completes, and only if a real
    /// auxiliary LLM is wired up (`llm.aux` set). Failures `warn!`-log and
    /// never block the turn.
    fn maybe_spawn_auto_title(&mut self) {
        if self.auto_title_spawned || self.turn_count != 1 {
            return;
        }
        if self.config.llm.aux.is_none() {
            // No aux client configured — fall through to the synchronous
            // placeholder title; we don't want to burn primary-LLM credits
            // on background niceties.
            return;
        }
        let first_user = self
            .history
            .iter()
            .find(|m| matches!(m.role, crate::types::Role::User))
            .and_then(|m| m.text_content())
            .map(str::to_string);
        let last_assistant = self
            .history
            .iter()
            .rev()
            .find(|m| matches!(m.role, crate::types::Role::Assistant))
            .and_then(|m| m.text_content())
            .map(str::to_string);
        let (Some(user_msg), Some(assistant_msg)) = (first_user, last_assistant) else {
            return;
        };
        if user_msg.trim().is_empty() || assistant_msg.trim().is_empty() {
            return;
        }
        self.auto_title_spawned = true;
        let session_id = self.session.meta.id.clone();
        let config = self.config.clone();
        spawn_logged(
            "auto-title",
            title_generator::auto_title_session(session_id, user_msg, assistant_msg, config),
        );
    }

    /// Core body of [`Self::finish_iteration`] — the original logic, kept
    /// free of cross-cutting goal concerns so the multiple `return` arms
    /// don't each need to remember the accounting/continuation hooks.
    #[allow(clippy::too_many_arguments)]
    async fn finish_iteration_inner(
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

        let hook_ctx = self.hook_ctx(
            HookPoint::AfterLlmResponse,
            HookData::LlmResponse {
                has_tool_calls: !tool_calls.is_empty(),
                text_length: text_content.len(),
            },
        );
        self.hook_registry.dispatch(&hook_ctx).await;

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

        // Past the text-only return, `tool_calls` is non-empty. Local and
        // template-driven backends can leak a bare scaffolding token (e.g.
        // `[TOOL_CALLS]`) as *content* alongside a real call; persisting it
        // pollutes the transcript, replays as fallback text, and survives
        // compaction until the model starts imitating it.
        //
        // Scope is deliberately narrow: only turns that carry parsed tool
        // calls. A marker that arrives as the model's *entire* reply (no
        // calls, or a stream error) returns above and is left alone — there
        // it is the whole response, and swallowing it would show the user
        // nothing at all instead of something obviously wrong.
        let text_content = if tool_markers::is_bare_tool_call_marker(&text_content) {
            tracing::debug!(
                dropped = %text_content,
                "dropping bare tool-call marker from assistant content"
            );
            String::new()
        } else {
            text_content
        };

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
    async fn record_usage(&self, usage: &UsageData) {
        let total = usage.prompt_tokens + usage.completion_tokens;
        if total == 0 {
            return;
        }
        self.metrics.llm_tokens.add(total, &[]);
        let guard = self.db_guard().await;
        let Some(ref db) = *guard else { return };
        Self::log_usage(db, usage, Some(self.session.meta.id.as_str())).await;
    }

    /// Record what calls made beside the turn's own stream spent: compaction
    /// and the pre-compaction memory flush. No stream handler sees their
    /// usage events, so without this they never reach `token_usage` and the
    /// monthly token budget does not count them.
    pub(super) async fn record_side_call_usage(&self, llm: &mut LlmClient) {
        for usage in llm.take_usage() {
            self.record_usage(&usage).await;
        }
    }

    /// Persist one call's usage to the token-usage table and reconcile it
    /// against the budget. Takes no agent, so the title generator, which runs
    /// after the turn on its own client, records its call the same way.
    pub(super) async fn log_usage(db: &Database, usage: &UsageData, session_id: Option<&str>) {
        let total = usage.prompt_tokens + usage.completion_tokens;
        if total == 0 {
            return;
        }
        let cost = crate::pricing::estimate_cost(
            &usage.model,
            usage.prompt_tokens,
            usage.completion_tokens,
        );
        if let Err(e) = db
            .log_token_usage_with_cache(
                usage.prompt_tokens,
                usage.completion_tokens,
                total,
                usage.cached_input_tokens,
                usage.cache_creation_tokens,
                &usage.provider,
                &usage.model,
                cost,
            )
            .await
        {
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
            match crate::budgets::RateCard::lookup(db, &provider_id, &usage.model, now).await {
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
                            client_side_cost_estimate = ?cost,
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
            db,
            None, // principal_id wiring is v3
            &provider_id,
            &usage.model,
            rate_card_effective_at,
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.cached_input_tokens,
            actual_usd,
            session_id,
        )
        .await
        {
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

    /// Hand the model the sub-agents that finished since the last iteration
    /// and that no `wait_for_agent` call collected, as one user-role notice
    /// ([`crate::multi_agent::completion_notice`] says why not a tool row).
    async fn drain_sub_agent_completions(&mut self) {
        // A resumed turn continues the assistant's own partial reply, which
        // must stay the last row. The reports wait for the next iteration.
        if self
            .history
            .last()
            .is_some_and(|m| m.role == Role::Assistant)
        {
            return;
        }
        let Some(ctrl) = self.agent_control.as_mut() else {
            return;
        };
        let completions = ctrl.drain_completions();
        let Some(reports) = crate::multi_agent::completion_reports(&completions) else {
            return;
        };
        // The cap and secret redaction a delegate tool result gets: a child's
        // answer can repeat a key it read.
        let reports = self.truncate_and_redact(&reports);
        let notice = crate::multi_agent::completion_notice(&reports);
        self.persist_message(Message::user(notice)).await;
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

        let max_hist = self.effective_history_budget();
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

        // Cheap pruning was insufficient — we are under genuine context-window
        // pressure and about to run LLM-based compaction.
        self.metrics.record_memory_pressure("reactive");

        // Whether this pass updates a prior summary (UX hint only).
        let iterative =
            crate::conversation::extract_last_compaction_summary(&self.history).is_some();

        let (dropped, failure) = self.run_compaction_with_fallback(target).await?;
        // Fold the outcome into the outage latch. This path reports its own
        // context-overflow error below, so it suppresses the generic notice by
        // passing no channel — the message here is strictly more specific.
        self.record_compaction_outcome(dropped, failure, None).await;

        if dropped > 0 {
            // A reactive compaction that actually dropped messages is a
            // demonstrated success — clear the proactive anti-thrash streak.
            self.compaction_strikes.reset();
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
            // follows.
            warn!("reactive_compact_history: nothing dropped, retry will likely re-fail");
            let after_tokens = history_tokens(&self.history);

            // Name the real cause. When the summarizer's provider is rejecting
            // us, "compaction freed nothing" would send the user hunting for a
            // context-size problem they don't have. The latch itself was
            // recorded above; this message is a strictly richer version of the
            // generic notice, so mark the outage announced to suppress it.
            let text = match failure {
                Some(crate::conversation::SummarizeFailure::Persistent(category)) => {
                    if let Some(outage) = self.compaction_outage.as_mut() {
                        outage.announced = true;
                    }
                    format!(
                        "Context overflow, and history compaction could not run — the summarizer \
                         could not reach the model provider ({category}). Check your API key or \
                         billing, then retry."
                    )
                }
                // A transient summarizer error is not a history-size problem:
                // telling the user to raise the budget would send them after
                // a knob that cannot help. Say the summarizer failed.
                Some(crate::conversation::SummarizeFailure::Transient) => format!(
                    "Context overflow, and the history summarizer failed on this attempt, \
                     so nothing was freed ({after_tokens} tokens, target {target}). \
                     Send your message again; if it keeps failing, check the model provider."
                ),
                None => format!(
                    "Context overflow and history compaction freed nothing \
                     ({after_tokens} tokens, target {target}). \
                     Start a new session or raise `conversation.max_history_tokens`."
                ),
            };
            let _ = event_tx.send(AgentEvent::Error(text)).await;
        }

        Ok(())
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
            self.persist_message(Message::user(&steer_text)).await;
            let _ = event_tx
                .send(AgentEvent::SteerReceived { text: steer_text })
                .await;
        }
    }

    /// Close the headless browser session if one is open.
    #[instrument(skip_all)]
    pub async fn close_browser(&mut self) {
        if let Some(session) = self.browser_session.take() {
            let _ = session.close().await; // best-effort cleanup; nothing to recover from a close failure
        }
    }

    /// Close the database connection, releasing all file locks.
    /// Call before deleting the data directory (e.g. during uninstall).
    pub async fn close_db(&self) {
        let mut guard = self.db_guard().await;
        if guard.take().is_some() {
            tracing::info!("Database connection closed for uninstall");
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod budget_event_tests;

#[cfg(test)]
mod goal_integration_tests;
