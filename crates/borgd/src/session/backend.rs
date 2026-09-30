//! Pluggable per-session backend.
//!
//! Production uses [`AgentBackend`] which owns a real `cortex::Agent`.
//! Tests provide a stub backend that emits canned events without needing an
//! LLM provider — keeps the daemon's session machinery testable in isolation.

use async_trait::async_trait;
use cortex::agent::{Agent, AgentEvent};
use cortex::config::Config;
use cortex::llm::CostEventSink;
use cortex::telemetry::BorgMetrics;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use tokio::sync::watch;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

/// How long a per-session lease stays valid without renewal. Re-claimed at the
/// start of every turn, so it only matters across a crash: another *live*
/// worker can take over a dead worker's session this long after its last turn,
/// and the next daemon boot reclaims every orphaned lease unconditionally
/// (`reclaim_all_session_leases`). Generous so a long single turn never lets
/// the lease lapse under a healthy daemon.
const SESSION_LEASE_TTL_SECS: i64 = 3600;

/// Outcome of taking the per-session lease for a turn.
enum LeaseStatus {
    /// This worker owns the session — proceed.
    Held,
    /// Another live worker owns the session — refuse the turn.
    Conflict,
    /// The lease couldn't be evaluated (DB hiccup). Proceed best-effort so a
    /// transient error never wedges a healthy single-process daemon.
    Unavailable,
}

/// Acquire the agent mutex from a sync trait method that may be invoked
/// from an async gRPC handler (i.e. from inside the tokio runtime).
///
/// `tokio::sync::Mutex::blocking_lock` panics with "Cannot block the current
/// thread from within a runtime" when called on a worker thread, and the
/// panic kills that worker mid-turn — which is how `/poke`-triggered
/// heartbeats end up surfacing as `*poked Borggundy*` and nothing else: the
/// `Session.GetState` snapshot RPC racing the in-flight turn panicked the
/// thread that was streaming events back through the bus.
///
/// `block_in_place` lets the multi-thread runtime move other tasks off this
/// worker so we can block on the lock without starving the scheduler. The
/// trait surface stays synchronous (and stub backends stay simple); only
/// `AgentBackend`'s production path needs the async-lock bridge.
fn lock_agent(agent: &Mutex<Agent>) -> tokio::sync::MutexGuard<'_, Agent> {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(agent.lock()))
}

/// RAII guard that detaches an attached IDE-backed text-file bridge when it
/// drops — including the panic-unwind path — so the next turn never inherits
/// a stale bridge. Owns the sole live `&mut Agent` for the turn; agent calls
/// go through `guard.agent`, and `Drop` reuses the same reference to detach.
struct TextFileBridgeAutoDetach<'a> {
    agent: &'a mut Agent,
    attached: bool,
}

impl<'a> TextFileBridgeAutoDetach<'a> {
    fn new(agent: &'a mut Agent, attached: bool) -> Self {
        Self { agent, attached }
    }
}

impl Drop for TextFileBridgeAutoDetach<'_> {
    fn drop(&mut self) {
        if self.attached {
            self.agent.set_text_file_bridge(None);
        }
    }
}

/// Snapshot of the agent's user-facing state for `Session.GetState`.
#[derive(Debug, Clone)]
pub struct SessionStateSnapshot {
    /// The cortex session id (used for resume).
    pub agent_session_id: String,
    /// Human-friendly title of the active session.
    pub title: String,
    /// Total messages in history.
    pub message_count: u64,
    /// Number of user-authored messages.
    pub user_message_count: u64,
    /// Estimated token count of current history.
    pub token_estimate: u64,
    /// Active LLM provider name.
    pub llm_provider: String,
    /// Active model id.
    pub llm_model: String,
    /// Collaboration mode label (default/execute/plan).
    pub collab_mode: String,
}

/// Result of forking the active agent session.
#[derive(Debug, Clone)]
pub struct ForkOutcome {
    /// New cortex session id created by the fork.
    pub forked_agent_session_id: String,
    /// Title of the new session (typically `"<parent> (fork)"`).
    pub title: String,
    /// Number of messages copied into the fork.
    pub message_count: u64,
}

pub use cortex::session_types::InlineImage;

/// Per-session backend that runs one agent turn at a time.
#[async_trait]
pub trait SessionBackend: Send + Sync + 'static {
    /// Run a turn on the user's `text` plus optional inline `images`,
    /// streaming `AgentEvent`s through `event_tx`. Implementations MUST send
    /// `AgentEvent::TurnComplete` (or `AgentEvent::Error`) before returning so
    /// the gRPC stream can close.
    async fn run_turn(
        &self,
        text: String,
        images: Vec<InlineImage>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
        text_file_bridge: Option<Arc<dyn cortex::execution_context::TextFileBridge>>,
    );

    /// Resume the turn that was cut short, with no new user message.
    ///
    /// Emits events on the same channel `run_turn` uses, so the caller's
    /// forwarder cannot tell the two apart. Backends that hold no agent report
    /// the limitation as a turn error rather than failing the RPC — the
    /// session is otherwise healthy.
    async fn resume_turn(&self, event_tx: mpsc::Sender<AgentEvent>, _cancel: CancellationToken) {
        let msg = "resuming an interrupted turn is not supported by this session";
        if let Err(e) = event_tx.send(AgentEvent::Error(msg.into())).await {
            tracing::warn!(error = %e, "failed to surface unsupported resume");
        }
    }

    /// Stable session id (for logs / SessionRegistry indexing).
    fn session_id(&self) -> &str;

    /// Finalize and close the backend session.
    ///
    /// Production backends should use this to flush any session-end lifecycle
    /// work that must happen before the daemon reports the session closed.
    async fn close_session(&self) -> anyhow::Result<()> {
        Ok(())
    }

    // ── Agent-control surface (default: not supported) ────────────────────
    //
    // These all return `Err(unsupported)` for stub backends; only `AgentBackend`
    // implements them. Daemon RPC handlers translate `unsupported` into a
    // gRPC `unimplemented` status so callers can render a clean message.

    /// Compact conversation history. Returns `(dropped, after_tokens)`.
    async fn compact(&self) -> anyhow::Result<(usize, usize)> {
        Err(anyhow::anyhow!("compact not supported by this backend"))
    }
    /// Component-level context-window usage snapshot (powers `/context`).
    async fn context_breakdown(&self) -> anyhow::Result<cortex::agent::ContextBreakdown> {
        Err(anyhow::anyhow!(
            "context_breakdown not supported by this backend"
        ))
    }
    /// Wipe history (keeping the agent alive for a fresh turn).
    fn clear_history(&self) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "clear_history not supported by this backend"
        ))
    }
    /// Drop the most recent assistant turn. Returns # messages removed.
    fn undo(&self) -> anyhow::Result<usize> {
        Err(anyhow::anyhow!("undo not supported by this backend"))
    }
    /// Rewind history to the Nth user message. Returns # messages removed.
    fn rewind_to_nth_user_message(&self, _n: usize) -> anyhow::Result<usize> {
        Err(anyhow::anyhow!("rewind not supported by this backend"))
    }
    /// Persist current state to disk.
    fn auto_save(&self) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("auto_save not supported by this backend"))
    }
    /// Start a fresh agent session, returning the new session id.
    async fn new_agent_session(&self) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("new_session not supported by this backend"))
    }
    /// Load an existing agent session from disk by id.
    fn load_agent_session(&self, _id: &str) -> anyhow::Result<u64> {
        Err(anyhow::anyhow!(
            "load_session not supported by this backend"
        ))
    }
    /// Read current state for `Session.GetState`.
    fn snapshot_state(&self) -> anyhow::Result<SessionStateSnapshot> {
        Err(anyhow::anyhow!(
            "snapshot_state not supported by this backend"
        ))
    }
    /// Push a steer message into the running agent loop.
    fn steer(&self, _text: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("steer not supported by this backend"))
    }

    /// Append a system-injected assistant message to the agent's history.
    /// Default unsupported; production backends mirror it into both
    /// in-memory history and SQLite so the next user turn sees it.
    async fn append_assistant_message(&self, _text: &str) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "append_assistant_message not supported by this backend"
        ))
    }

    /// Execute a shell command in the session's context, spending no LLM turn.
    async fn exec_shell(
        &self,
        _command: &str,
        _timeout_ms: u64,
    ) -> anyhow::Result<ExecShellOutcome> {
        Err(anyhow::anyhow!("exec_shell not supported by this backend"))
    }

    /// Workspace paths matching a prefix, for a client offering file
    /// completions. Default unsupported.
    async fn list_paths(
        &self,
        _prefix: &str,
        _limit: usize,
    ) -> anyhow::Result<Vec<cortex::tool_handlers::PathSuggestion>> {
        Err(anyhow::anyhow!("list_paths not supported by this backend"))
    }

    /// Fork the active agent session at its last message. Daemon owns the
    /// single SQLite writer, so this MUST run here rather than in the client.
    async fn fork(&self) -> anyhow::Result<ForkOutcome> {
        Err(anyhow::anyhow!("fork not supported by this backend"))
    }
}

/// Recover the process exit status from a `run_shell` result string.
///
/// The handler renders `Exit code: N` as the first line for both the piped
/// and PTY paths. Returns `0` when no such line is present — session-control
/// verbs and the apply-patch reroute produce output without one, and treating
/// those as failures would be worse than treating them as success.
fn parse_exit_code(output: &str) -> i32 {
    output
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("Exit code: "))
        .and_then(|n| n.trim().parse::<i32>().ok())
        .unwrap_or(0)
}

/// Result of a `!cmd` shell-mode execution.
#[derive(Debug, Clone)]
pub struct ExecShellOutcome {
    /// Combined, redacted command output.
    pub output: String,
    /// Process exit code, or `-1` when the command never ran.
    pub exit_code: i32,
    /// True when policy refused the command instead of executing it.
    pub denied: bool,
}

/// Production backend: owns one `cortex::Agent`. Hooks (vitals, activity,
/// bond, evolution, script) are registered at construction time, matching the
/// in-process behavior of `crates/cli/src/repl.rs` and `tui/mod.rs`.
pub struct AgentBackend {
    session_id: String,
    /// `Agent::send_message_with_cancel` takes `&mut self`; serialize turns
    /// behind a Mutex so two concurrent `Send` RPCs to the same session
    /// (which the protocol forbids but the transport doesn't enforce) queue
    /// safely instead of corrupting state.
    agent: Mutex<Agent>,
    /// Steer channel sender — held here so `Session.Steer` can push text
    /// into the running turn even though the agent is locked.
    steer_tx: mpsc::UnboundedSender<String>,
    /// Identifies this daemon instance as the lease holder. Stable for the
    /// process lifetime, so re-claiming our own session is always re-entrant;
    /// a restarted daemon has a new id and looks like a (legitimate) takeover.
    worker_id: String,
    /// Epoch of the lease this backend currently holds, or `-1` if none. Set on
    /// every successful claim; used to release precisely on close.
    lease_epoch: AtomicI64,
}

impl AgentBackend {
    /// Build a backend hosting a fresh agent. `resume_id` (if set) loads
    /// existing session history; otherwise a new session is started.
    ///
    /// `goal_event_sink` is the daemon-side bus that `Admin.SubscribeGoalEvents`
    /// publishes to — passing `None` is fine (the agent loop tolerates an
    /// absent sink), but production wires one through so the TUI footer and
    /// any other subscribers see lifecycle events without polling.
    pub async fn new(
        config: Config,
        metrics: BorgMetrics,
        resume_id: Option<&str>,
        config_rx: Option<watch::Receiver<Config>>,
        cost_event_sink: Option<Arc<dyn CostEventSink>>,
        execution_context: Option<cortex::execution_context::ExecutionContext>,
        goal_bus: Option<crate::goal_bus::GoalEventBus>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut agent = Agent::new(config, metrics).await?;
        // Captured before the context is moved into the agent: the client's
        // workspace (when provided) is the session's real working directory,
        // which the workspace stamp below prefers over the daemon's own cwd.
        let workspace_cwd_hint = execution_context
            .as_ref()
            .map(|c| c.workspace_cwd().to_path_buf());
        if let Some(execution_context) = execution_context {
            agent.set_execution_context(execution_context);
        }
        if let Some(rx) = config_rx {
            agent.set_config_watcher(rx);
        }
        if let Some(sink) = cost_event_sink {
            agent.set_cost_event_sink(sink);
        }
        if let Some(bus) = goal_bus {
            agent.set_goal_event_sink(bus.as_sink());
        }

        // Register the same hook set the in-process REPL/TUI uses. Failed
        // registration is non-fatal — log so operators can debug "why is XP
        // not advancing" rather than have it silently drop.
        match cortex::vitals::VitalsHook::new().await {
            Ok(h) => agent.hook_registry_mut().register(Box::new(h)),
            Err(e) => tracing::warn!(error = %e, "vitals hook unavailable"),
        }
        match cortex::activity_log::ActivityHook::new().await {
            Ok(h) => agent.hook_registry_mut().register(Box::new(h)),
            Err(e) => tracing::warn!(error = %e, "activity_log hook unavailable"),
        }
        match cortex::bond::BondHook::new().await {
            Ok(h) => agent.hook_registry_mut().register(Box::new(h)),
            Err(e) => tracing::warn!(error = %e, "bond hook unavailable"),
        }
        if agent.config().evolution.enabled {
            match cortex::evolution::EvolutionHook::new().await {
                Ok(h) => agent.hook_registry_mut().register(Box::new(h)),
                Err(e) => tracing::warn!(error = %e, "evolution hook unavailable"),
            }
        }
        match cortex::budgets::BudgetHook::new().await {
            Ok(h) => agent.hook_registry_mut().register(Box::new(h)),
            Err(e) => tracing::warn!(error = %e, "budget hook unavailable"),
        }
        for hook in cortex::hooks::ScriptHook::load_all(agent.config().hooks.enabled) {
            agent.hook_registry_mut().register(Box::new(hook));
        }
        // Outbound signed webhooks (webhooks.endpoints). None when unconfigured;
        // construction problems are logged inside from_config, never fatal.
        if let Some(hook) = cortex::hooks::WebhookHook::from_config(&agent.config().webhooks) {
            agent.hook_registry_mut().register(Box::new(hook));
        }

        if let Some(id) = resume_id {
            if let Err(e) = agent.load_session(id) {
                tracing::warn!(session_id = %id, error = %e, "load_session failed; starting fresh");
                agent.new_session().await;
            }
        }

        let (steer_tx, steer_rx) = mpsc::unbounded_channel::<String>();
        agent.set_steer_channel(steer_rx);

        let session_id = agent.session().meta.id.clone();
        let worker_id = format!("borgd-{}", std::process::id());
        let mut initial_epoch = -1;

        // Materialize the `sessions` row immediately so RPCs that resolve
        // "the latest session" (e.g. Admin.SetGoal with empty session_id)
        // hit a real row from the very first frame of the TUI. Without
        // this, `messages` rows get written under `session_id` but the
        // `sessions` table stays empty until something else (handoff,
        // fork, export) materializes a row — and `/goal foo` from a
        // fresh TUI fails with "no active session". Idempotent upsert,
        // so resume paths re-touch the existing row without clobbering it.
        if let Ok(db) = cortex::db::Database::open().await {
            let now = chrono::Utc::now().timestamp();
            let model = agent.config().llm.model.clone();
            if let Err(e) = db
                .upsert_session(&session_id, now, now, 0, &model, "")
                .await
            {
                tracing::warn!(session_id = %session_id, error = %e, "upsert_session on open failed");
            }
            // Stamp workspace identity so workspace-scoped resume
            // (`borg --continue`) can find this session again. Client-provided
            // workspace wins; a daemon opened without one falls back to its
            // own process cwd (best-effort — TUI clients re-stamp with their
            // real cwd right after open).
            let stamp_cwd = workspace_cwd_hint.or_else(|| std::env::current_dir().ok());
            if let Some(dir) = stamp_cwd {
                let (dir, repo_root) = cortex::session::workspace_identity(&dir);
                if let Err(e) = db
                    .set_session_workspace(
                        &session_id,
                        &dir.to_string_lossy(),
                        repo_root.as_ref().map(|p| p.to_string_lossy()).as_deref(),
                    )
                    .await
                {
                    tracing::warn!(session_id = %session_id, error = %e, "workspace stamp on open failed");
                }
            }
            // Take ownership of the session up front. A conflict here (another
            // live worker owns it) is only possible in a multi-worker setup; we
            // warn but still open locally — the per-turn claim is the real gate.
            match db
                .claim_session_lease(&session_id, &worker_id, now, SESSION_LEASE_TTL_SECS)
                .await
            {
                Ok(Some(lease)) => initial_epoch = lease.epoch,
                Ok(None) => tracing::warn!(
                    session_id = %session_id,
                    "session lease held by another worker at open"
                ),
                Err(e) => {
                    tracing::warn!(session_id = %session_id, error = %e, "claim_session_lease on open failed")
                }
            }
        }

        Ok(Arc::new(Self {
            session_id,
            agent: Mutex::new(agent),
            steer_tx,
            worker_id,
            lease_epoch: AtomicI64::new(initial_epoch),
        }))
    }

    /// Take or refresh the per-session lease for the turn about to run. Re-entrant
    /// for this worker (so a healthy single-process daemon always succeeds);
    /// only a different live owner yields [`LeaseStatus::Conflict`].
    async fn acquire_lease(&self) -> LeaseStatus {
        let db = match cortex::db::Database::open().await {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!(session_id = %self.session_id, error = %e, "lease: db open failed");
                return LeaseStatus::Unavailable;
            }
        };
        let now = chrono::Utc::now().timestamp();
        match db
            .claim_session_lease(
                &self.session_id,
                &self.worker_id,
                now,
                SESSION_LEASE_TTL_SECS,
            )
            .await
        {
            Ok(Some(lease)) => {
                self.lease_epoch.store(lease.epoch, Ordering::Relaxed);
                LeaseStatus::Held
            }
            Ok(None) => LeaseStatus::Conflict,
            Err(e) => {
                tracing::warn!(session_id = %self.session_id, error = %e, "claim_session_lease failed");
                LeaseStatus::Unavailable
            }
        }
    }
}

#[async_trait]
impl SessionBackend for AgentBackend {
    async fn run_turn(
        &self,
        text: String,
        images: Vec<InlineImage>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
        text_file_bridge: Option<Arc<dyn cortex::execution_context::TextFileBridge>>,
    ) {
        // Reject overlapping turns explicitly rather than silently queuing on
        // an async lock — a hung second `Send` is indistinguishable from a
        // crashed daemon for the user. SessionHost serializes turns at a
        // higher level, so this branch only fires when the protocol is
        // misused (concurrent Send to the same session_id).
        let mut agent = match self.agent.try_lock() {
            Ok(a) => a,
            Err(_) => {
                let msg = "another turn is already in flight for this session";
                tracing::warn!(session_id = %self.session_id, "{msg}");
                if let Err(e) = event_tx.send(AgentEvent::Error(msg.into())).await {
                    tracing::warn!(error = %e, "failed to surface concurrent-turn error");
                }
                return;
            }
        };

        // Own the session before advancing it. The log is the source of truth,
        // so a worker may only act on a session it holds the lease for — if
        // another live worker owns it, refuse rather than fork the log. A dead
        // worker's lease expires and any worker can then reconstruct from the
        // log and continue.
        if let LeaseStatus::Conflict = self.acquire_lease().await {
            let msg = "this session is owned by another worker; cannot run the turn here";
            tracing::warn!(session_id = %self.session_id, "{msg}");
            if let Err(e) = event_tx.send(AgentEvent::Error(msg.into())).await {
                tracing::warn!(error = %e, "failed to surface session-ownership conflict");
            }
            return;
        }

        // Attach the IDE-backed text-file bridge for the duration of the turn
        // and detach unconditionally — including on unwind. The async Mutex
        // serializes turns on this backend, so the next turn must not inherit
        // a stale bridge if this turn's future is dropped or panics.
        let bridge_attached = text_file_bridge.is_some();
        if bridge_attached {
            agent.set_text_file_bridge(text_file_bridge);
        }
        let bridge_guard = TextFileBridgeAutoDetach::new(&mut agent, bridge_attached);

        let result = if images.is_empty() {
            bridge_guard
                .agent
                .send_message_with_cancel(&text, event_tx.clone(), cancel)
                .await
        } else {
            use base64::Engine as _;
            use cortex::types::{ContentPart, MediaData, Message};
            let engine = base64::engine::general_purpose::STANDARD;
            let mut parts = vec![ContentPart::Text(text)];
            for img in images {
                parts.push(ContentPart::ImageBase64 {
                    media: MediaData {
                        mime_type: img.mime_type,
                        data: engine.encode(&img.data),
                        filename: img.filename,
                    },
                });
            }
            let msg = Message::user_multimodal(parts);
            bridge_guard
                .agent
                .send_message_raw(msg, event_tx.clone(), cancel)
                .await
        };
        // bridge_guard's Drop runs here (or earlier on unwind) and detaches.
        drop(bridge_guard);

        if let Err(e) = result {
            // anyhow's `Display` only renders the topmost message; `{:#}` walks
            // the `.context()` chain so the user (and the daemon log) sees the
            // root cause instead of just the outer wrap.
            let chained = format!("{e:#}");
            tracing::warn!(session_id = %self.session_id, error = %chained, "agent turn failed");
            if let Err(send_err) = event_tx.send(AgentEvent::Error(chained)).await {
                tracing::warn!(error = %send_err, "failed to surface agent error to client");
            }
        }
    }

    async fn resume_turn(&self, event_tx: mpsc::Sender<AgentEvent>, cancel: CancellationToken) {
        // Same two guards `run_turn` applies, and for the same reasons: a
        // resume is a turn, so it must not overlap another one and must not
        // advance a session this worker does not own.
        let mut agent = match self.agent.try_lock() {
            Ok(a) => a,
            Err(_) => {
                let msg = "another turn is already in flight for this session";
                tracing::warn!(session_id = %self.session_id, "{msg}");
                if let Err(e) = event_tx.send(AgentEvent::Error(msg.into())).await {
                    tracing::warn!(error = %e, "failed to surface concurrent-turn error");
                }
                return;
            }
        };

        if let LeaseStatus::Conflict = self.acquire_lease().await {
            let msg = "this session is owned by another worker; cannot run the turn here";
            tracing::warn!(session_id = %self.session_id, "{msg}");
            if let Err(e) = event_tx.send(AgentEvent::Error(msg.into())).await {
                tracing::warn!(error = %e, "failed to surface session-ownership conflict");
            }
            return;
        }

        if let Err(e) = agent
            .resume_interrupted_turn(event_tx.clone(), cancel)
            .await
        {
            let chained = format!("{e:#}");
            tracing::warn!(session_id = %self.session_id, error = %chained, "resume failed");
            if let Err(send_err) = event_tx.send(AgentEvent::Error(chained)).await {
                tracing::warn!(error = %send_err, "failed to surface resume error to client");
            }
        }
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    async fn close_session(&self) -> anyhow::Result<()> {
        let mut agent = self.agent.lock().await;
        agent.end_session().await;
        drop(agent);
        // Release our lease so another worker can take the session immediately
        // (rather than waiting out the TTL). Best-effort: a stale epoch is a
        // no-op, and on failure the lease simply expires.
        let epoch = self.lease_epoch.load(Ordering::Relaxed);
        if epoch >= 0 {
            if let Ok(db) = cortex::db::Database::open().await {
                if let Err(e) = db
                    .release_session_lease(&self.session_id, &self.worker_id, epoch)
                    .await
                {
                    tracing::warn!(session_id = %self.session_id, error = %e, "release_session_lease on close failed");
                }
            }
        }
        Ok(())
    }

    async fn compact(&self) -> anyhow::Result<(usize, usize)> {
        let mut agent = self.agent.lock().await;
        Ok(agent.compact().await)
    }

    async fn context_breakdown(&self) -> anyhow::Result<cortex::agent::ContextBreakdown> {
        // Waits on the agent mutex like `compact` does: during an in-flight
        // turn the RPC (and the /context caller) blocks until the turn
        // releases the lock. Accepted — same precedent as /compact, and a
        // mid-turn snapshot would be assembled from a half-mutated history.
        let mut agent = self.agent.lock().await;
        agent.context_breakdown().await
    }

    fn clear_history(&self) -> anyhow::Result<()> {
        let mut agent = lock_agent(&self.agent);
        agent.clear_history();
        Ok(())
    }

    fn undo(&self) -> anyhow::Result<usize> {
        let mut agent = lock_agent(&self.agent);
        Ok(agent.undo())
    }

    fn rewind_to_nth_user_message(&self, n: usize) -> anyhow::Result<usize> {
        let mut agent = lock_agent(&self.agent);
        Ok(agent.rewind_to_nth_user_message(n))
    }

    fn auto_save(&self) -> anyhow::Result<()> {
        let mut agent = lock_agent(&self.agent);
        agent.auto_save();
        Ok(())
    }

    async fn new_agent_session(&self) -> anyhow::Result<String> {
        let mut agent = self.agent.lock().await;
        agent.new_session().await;
        Ok(agent.session().meta.id.clone())
    }

    fn load_agent_session(&self, id: &str) -> anyhow::Result<u64> {
        let mut agent = lock_agent(&self.agent);
        agent.load_session(id)?;
        Ok(agent.session().messages.len() as u64)
    }

    fn snapshot_state(&self) -> anyhow::Result<SessionStateSnapshot> {
        let agent = lock_agent(&self.agent);
        let session = agent.session();
        let cfg = agent.next_turn_config();
        let (msgs, users) = agent.conversation_stats();
        Ok(SessionStateSnapshot {
            agent_session_id: session.meta.id.clone(),
            title: session.meta.title.clone(),
            message_count: msgs as u64,
            user_message_count: users as u64,
            token_estimate: 0,
            llm_provider: cfg.llm.provider.clone().unwrap_or_default(),
            llm_model: cfg.llm.model.clone(),
            collab_mode: format!("{}", cfg.conversation.collaboration_mode),
        })
    }

    fn steer(&self, text: &str) -> anyhow::Result<()> {
        self.steer_tx
            .send(text.to_string())
            .map_err(|e| anyhow::anyhow!("steer channel closed: {e}"))
    }

    async fn append_assistant_message(&self, text: &str) -> anyhow::Result<()> {
        let mut agent = self.agent.lock().await;
        agent.append_assistant_message(text).await;
        Ok(())
    }

    /// `!cmd` shell mode.
    ///
    /// Routed through `handle_run_shell_with_context` rather than a fresh
    /// `Command`, which is the whole point: it inherits the control-plane
    /// deny, the `ExecutionPolicy` check, `env_clear` + the credential
    /// allowlist, sandbox wrapping, the process registry, the output cap, and
    /// the timeout + process-group kill — with zero duplicated logic and no
    /// second path to keep in sync.
    async fn exec_shell(&self, command: &str, timeout_ms: u64) -> anyhow::Result<ExecShellOutcome> {
        let mut agent = self.agent.lock().await;
        let config = agent.config().clone();

        // Second gate. The TUI dispatcher checks this too, but a toggle
        // enforced only client-side is not enforced at all.
        if !config.ui.shell_mode_enabled {
            return Ok(ExecShellOutcome {
                output: "Shell mode is disabled (set `ui.shell_mode_enabled` to true).".to_string(),
                exit_code: -1,
                denied: true,
            });
        }

        let timeout_ms = if timeout_ms == 0 {
            config.tools.default_timeout_ms
        } else {
            timeout_ms
        };
        let ctx = agent.execution_context();
        // The output is captured for history rather than streamed; a live
        // receiver isn't required, so a local channel keeps the handler happy.
        let (tx, _rx) = mpsc::channel(64);

        let args = serde_json::json!({
            "action": "run",
            "command": command,
            "timeout_ms": timeout_ms,
        });
        let raw = cortex::tool_handlers::handle_run_shell_with_context(
            &args,
            &config,
            &config.policy,
            &tx,
            &format!("shell-mode-{}", uuid::Uuid::new_v4()),
            None,
            &ctx,
        )
        .await?;

        let raw_for_status = raw.clone();

        // The handler reports refusals as ordinary output text rather than an
        // `Err`, so the denial has to be recognized here. Checked against the
        // *pre-redaction* text and against a constant exported by the handler,
        // so neither redaction nor a reworded message can silently turn a
        // refusal into an apparent success.
        let denied = raw.starts_with(cortex::tool_handlers::SHELL_DENIED_PREFIX);

        // `!cat .env` must not land in the transcript in the clear. The tool
        // path redacts what it streams; this covers what we persist.
        let output = if config.security.output_redaction {
            cortex::security::secrets::redact_secrets(&raw)
        } else {
            raw
        };

        // Reuses `append_assistant_message`, which already refuses to inject
        // when the last assistant turn has unresolved tool calls — exactly the
        // history corruption `!cmd` could otherwise cause if typed mid-stream.
        agent
            .append_assistant_message(&format!(
                "<shell_command>{command}</shell_command>\n<shell_output>\n{output}\n</shell_output>"
            ))
            .await;

        Ok(ExecShellOutcome {
            output,
            // `-1` means "never ran". Otherwise recover the real status from
            // the handler's `Exit code: N` line rather than hardcoding 0 —
            // a hardcoded success made `is_error` unreachable for genuinely
            // failing commands, i.e. a lie in the wire contract.
            exit_code: if denied {
                -1
            } else {
                parse_exit_code(&raw_for_status)
            },
            denied,
        })
    }

    /// Completions come from the agent's own resolved `Config` and
    /// `ExecutionContext`, so the menu can only ever name what this session's
    /// `fs_read` would agree to open.
    async fn list_paths(
        &self,
        prefix: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<cortex::tool_handlers::PathSuggestion>> {
        let (ctx, security) = {
            let agent = self.agent.lock().await;
            (agent.execution_context(), agent.config().security.clone())
        };
        let prefix = prefix.to_string();
        // A directory read is blocking work, and the agent lock is already
        // released: nothing here belongs on a runtime worker thread.
        tokio::task::spawn_blocking(move || {
            cortex::tool_handlers::complete_paths(
                &prefix,
                &ctx,
                &security.blocked_paths,
                &security.allowed_paths,
                limit,
            )
        })
        .await?
    }

    async fn fork(&self) -> anyhow::Result<ForkOutcome> {
        // Resolve the parent agent session id under the agent lock, then
        // release it before doing DB work so concurrent reads (e.g. `GetState`)
        // aren't blocked on the agent for the duration.
        let parent_id = {
            let agent = self.agent.lock().await;
            agent.session().meta.id.clone()
        };

        let db = cortex::db::Database::open().await?;
        // Anchor at the last event in the parent's reconstructable history
        // (lineage-aware, so forking a fork anchors at the true tip).
        let msgs = db.load_session_lineage_rows(&parent_id).await?;
        let anchor = msgs
            .last()
            .map(|m| m.id)
            .ok_or_else(|| anyhow::anyhow!("session has no messages"))?;
        drop(db);
        let forked = cortex::session::fork_session(&parent_id, anchor).await?;
        Ok(ForkOutcome {
            forked_agent_session_id: forked.meta.id,
            title: forked.meta.title,
            message_count: forked.meta.message_count as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: calling `lock_agent` from within a multi-thread tokio
    /// runtime (which is what every production gRPC handler does) must not
    /// panic with "Cannot block the current thread from within a runtime".
    /// The naive `Mutex::blocking_lock()` panics here, killing the worker
    /// thread mid-turn. Symptom in production: `/poke` shows the local
    /// "*poked Borggundy*" line but no heartbeat response — the panic
    /// aborts the worker that was streaming bus events back to the TUI.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lock_agent_does_not_panic_inside_runtime() {
        // Models the production failure mode: a sync function using
        // `block_in_place + block_on` is called from inside a tokio async
        // task. Naive `Mutex::blocking_lock()` panics with "Cannot block
        // the current thread from within a runtime"; the bridge below must
        // not.
        let mutex = std::sync::Arc::new(Mutex::new(42i64));
        fn sync_lock_bridge(m: &Mutex<i64>) -> tokio::sync::MutexGuard<'_, i64> {
            tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(m.lock()))
        }
        let g = sync_lock_bridge(&mutex);
        assert_eq!(*g, 42);
    }

    /// Regression: errors surfaced via `AgentEvent::Error` must include the
    /// full `anyhow` context chain, not just the topmost wrap. A previous
    /// version used `e.to_string()` which produced messages like
    /// "Failed to build system prompt" with no underlying cause — leaving
    /// the user (and the daemon log) with nothing actionable. Source-grep
    /// guard so a future contributor can't silently revert this to
    /// `e.to_string()` and lose the context chain again. Pure
    /// library-behavior tests over `format!("{e:#}")` would pass even
    /// against a reverted production callsite.
    #[test]
    fn agent_error_callsite_uses_anyhow_alternate_format() {
        let src = include_str!("backend.rs");
        // Strip this test's own body before scanning so the patterns inside
        // string literals don't false-positive on themselves.
        let marker = "fn agent_error_callsite_uses_anyhow_alternate_format";
        let scan = match src.find(marker) {
            Some(idx) => &src[..idx],
            None => src,
        };
        // The production path must construct the message via `{:#}` so
        // anyhow walks the `.context()` chain.
        assert!(
            scan.contains("format!(\"{e:#}\")"),
            "backend.rs lost the anyhow-chained error format — \
             AgentEvent::Error would surface only the outer wrap"
        );
        // And it must not regress to a bare `.to_string()` on the error
        // before sending into the channel. The closest negative pattern
        // is the exact line shape the pre-fix code used.
        assert!(
            !scan.contains("AgentEvent::Error(e.to_string()"),
            "backend.rs reverted to e.to_string() — lose the context chain"
        );
    }
}
