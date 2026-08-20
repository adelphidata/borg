//! User-authored script hooks discovered from `~/.borg/hooks.json`.
//!
//! Schema mirrors the Claude Code hooks format:
//!
//! ```json
//! {
//!   "hooks": {
//!     "PreToolUse": [
//!       { "matcher": "run_shell",
//!         "hooks": [{ "type": "command", "command": "~/bin/audit.sh", "timeout": 5 }] }
//!     ],
//!     "PostToolUse": [ ... ],
//!     "SessionStart": [ ... ],
//!     "SessionEnd": [ ... ],
//!     "UserPromptSubmit": [ ... ],
//!     "Stop": [ ... ]
//!   }
//! }
//! ```
//!
//! Event names map to [`HookPoint`] variants. Hooks run `sh -c <command>` with the
//! event payload as `$1` (a JSON object). For `PreToolUse`, a non-zero exit or
//! timeout returns [`HookAction::Skip`] to abort the tool call — the hook's
//! stderr (trimmed, length-bounded) rides along as the skip reason so the
//! model learns *why* the call was blocked; all other events are observers.
//!
//! The payload is delivered on **both** channels: as `$1` and on the hook's
//! stdin. Claude Code hooks read it from stdin (`input=$(cat)`), so a script
//! copied from a Claude Code config would otherwise block until borg's timeout —
//! which on `PreToolUse` silently turns into [`HookAction::Skip`] and blocks the
//! tool call. Supporting both keeps borg's own `$1` convention working while
//! making the documented compatibility real.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;

use crate::hooks::{Hook, HookAction, HookContext, HookData, HookPoint};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HooksFile {
    /// Optional JSON-Schema pointer (`"$schema": "..."`). Tolerated so editor
    /// tooling can annotate the file; ignored at runtime. Named explicitly
    /// (rather than dropping `deny_unknown_fields`) so a *misplaced event key*
    /// at the root is still a hard parse error.
    #[serde(default, rename = "$schema")]
    #[allow(dead_code)]
    schema: Option<String>,
    /// Optional human description of the hook profile. Logged at load, ignored
    /// otherwise. Tolerated for the same reason as `$schema`.
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    hooks: HashMap<String, Vec<MatcherGroup>>,
}

#[derive(Debug, Deserialize)]
struct MatcherGroup {
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum HookHandlerConfig {
    #[serde(rename = "command")]
    Command {
        command: String,
        #[serde(default)]
        timeout: Option<u64>,
        /// Run in the background instead of holding up the operation that
        /// fired the hook. Notifications, metrics pushes, and desktop toasts
        /// are the cases: they have nothing to say about whether the work
        /// should proceed, but a synchronous one adds its whole runtime to
        /// every matching tool call.
        #[serde(default, rename = "async")]
        run_async: bool,
    },
}

/// How many background hooks may run at once, process-wide.
///
/// A cap rather than a queue: hooks fire per tool call, so a slow background
/// hook on a busy matcher would otherwise accumulate processes without bound.
/// Over the cap the hook is dropped with a warning — a skipped notification is
/// recoverable, a fork bomb is not.
const MAX_CONCURRENT_BACKGROUND_HOOKS: usize = 4;

fn background_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_BACKGROUND_HOOKS))
}

// hooks.json-compatible event names; keep in sync if HookPoint variants change.
// Shared with `hooks::webhook` so webhook `events` filters use the same
// vocabulary as `~/.borg/hooks.json`.
pub(crate) const EVENT_NAMES: &[(&str, HookPoint)] = &[
    ("SessionStart", HookPoint::SessionStart),
    ("SessionEnd", HookPoint::SessionEnd),
    ("UserPromptSubmit", HookPoint::BeforeAgentStart),
    ("PreToolUse", HookPoint::BeforeToolCall),
    ("PostToolUse", HookPoint::AfterToolCall),
    ("Stop", HookPoint::TurnComplete),
];

pub(crate) fn parse_event_name(s: &str) -> Option<HookPoint> {
    EVENT_NAMES
        .iter()
        .find(|(name, _)| *name == s)
        .map(|(_, point)| *point)
}

pub(crate) fn event_name_for_point(point: HookPoint) -> &'static str {
    EVENT_NAMES
        .iter()
        .find(|(_, p)| *p == point)
        .map(|(n, _)| *n)
        .unwrap_or("Unknown")
}

/// A hook defined in `~/.borg/hooks.json` that runs a shell command on a lifecycle event.
#[derive(Clone)]
pub struct ScriptHook {
    name: String,
    points: Vec<HookPoint>,
    matcher: Option<Regex>,
    command: String,
    timeout: Duration,
    /// Run detached from the operation that fired it. A background hook can
    /// never block, rewrite, or otherwise steer that operation — it always
    /// reports `Continue` immediately.
    background: bool,
}

impl ScriptHook {
    /// Path to the default hooks config file: `~/.borg/hooks.json`.
    pub fn default_path() -> Option<PathBuf> {
        crate::config::Config::data_dir()
            .ok()
            .map(|d| d.join("hooks.json"))
    }

    /// Load hooks from the default path, gated by `enabled`.
    ///
    /// Returns an empty `Vec` if `enabled` is false, the file is missing, or the file
    /// is malformed. Never returns `Err` — all errors are logged via `tracing::warn!`.
    pub fn load_all(enabled: bool) -> Vec<ScriptHook> {
        if !enabled {
            return Vec::new();
        }
        match Self::default_path() {
            Some(path) => Self::load_from_file(&path),
            None => {
                tracing::warn!("hooks: could not resolve home directory");
                Vec::new()
            }
        }
    }

    /// Load hooks from a specific file path. Primary test seam.
    pub fn load_from_file(path: &Path) -> Vec<ScriptHook> {
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                tracing::warn!("hooks: failed to read {}: {}", path.display(), e);
                return Vec::new();
            }
        };
        Self::load_from_str(&contents, path)
    }

    fn load_from_str(contents: &str, source: &Path) -> Vec<ScriptHook> {
        let file: HooksFile = match serde_json::from_str(contents) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("hooks: failed to parse {}: {}", source.display(), e);
                return Vec::new();
            }
        };

        if let Some(desc) = file.description.as_deref() {
            tracing::debug!("hooks: {} — {}", source.display(), desc);
        }

        let mut hooks = Vec::new();
        // Sort for deterministic ordering (HashMap iteration is random).
        let mut events: Vec<(&String, &Vec<MatcherGroup>)> = file.hooks.iter().collect();
        events.sort_by(|a, b| a.0.cmp(b.0));

        for (event_name, groups) in events {
            let Some(point) = parse_event_name(event_name) else {
                tracing::warn!(
                    "hooks: unknown event name {:?} in {}",
                    event_name,
                    source.display()
                );
                continue;
            };

            for (gi, group) in groups.iter().enumerate() {
                let matcher = match compile_matcher(group.matcher.as_deref()) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(
                            "hooks: invalid matcher {:?} at {}[{}]: {}",
                            group.matcher,
                            event_name,
                            gi,
                            e
                        );
                        continue;
                    }
                };

                for (hi, raw) in group.hooks.iter().enumerate() {
                    let cfg: HookHandlerConfig = match serde_json::from_value(raw.clone()) {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(
                                "hooks: invalid handler at {}[{}][{}] in {}: {}",
                                event_name,
                                gi,
                                hi,
                                source.display(),
                                e
                            );
                            continue;
                        }
                    };
                    let HookHandlerConfig::Command {
                        command,
                        timeout,
                        run_async,
                    } = cfg;
                    if command.trim().is_empty() {
                        tracing::warn!(
                            "hooks: empty command at {}[{}][{}] in {}",
                            event_name,
                            gi,
                            hi,
                            source.display()
                        );
                        continue;
                    }
                    let timeout_secs = timeout.unwrap_or(60).clamp(1, 600);
                    let name = format!("{event_name}[{gi}][{hi}]");
                    // Two events cannot honour `async`, and silently accepting
                    // it there would leave the operator with a hook that looks
                    // configured and does nothing they expect.
                    let background = match (run_async, point) {
                        (true, HookPoint::BeforeToolCall) => {
                            tracing::warn!(
                                "hooks: {name} is PreToolUse with \"async\": true — a background \
                                 hook cannot block a tool call, and honouring it would silently \
                                 disable the gate this event exists for. It runs synchronously; \
                                 move the work to PostToolUse if it should not block."
                            );
                            false
                        }
                        (true, HookPoint::SessionEnd) => {
                            tracing::warn!(
                                "hooks: {name} is SessionEnd with \"async\": true — the session \
                                 is torn down before a background hook could finish, so it runs \
                                 synchronously."
                            );
                            false
                        }
                        (run_async, _) => run_async,
                    };
                    hooks.push(ScriptHook {
                        name,
                        points: vec![point],
                        matcher: matcher.clone(),
                        command,
                        timeout: Duration::from_secs(timeout_secs),
                        background,
                    });
                }
            }
        }
        hooks
    }

    fn matches_tool(&self, ctx: &HookContext) -> bool {
        let tool_name = match &ctx.data {
            HookData::ToolCall { name, .. } | HookData::ToolResult { name, .. } => name.as_str(),
            _ => return true, // non-tool events: matcher is ignored
        };
        match &self.matcher {
            None => true,
            Some(r) => r.is_match(tool_name),
        }
    }

    fn is_pre_tool(&self) -> bool {
        self.points.contains(&HookPoint::BeforeToolCall)
    }

    fn build_payload(&self, ctx: &HookContext) -> String {
        let tool = match &ctx.data {
            HookData::ToolCall { name, args } => {
                // `args` is the tool's own serialized JSON. Pass it as a
                // parsed object when it is one, so a hook can read a field
                // without re-parsing a string; fall back to the raw text so a
                // non-object payload still reaches the hook. `tool_input` is
                // the Claude Code spelling of the same field.
                let parsed: serde_json::Value =
                    serde_json::from_str(args).unwrap_or_else(|_| serde_json::json!(args));
                Some(serde_json::json!({
                    "name": name,
                    "is_error": false,
                    "args": parsed,
                    "tool_input": parsed,
                }))
            }
            HookData::ToolResult { name, is_error, .. } => {
                Some(serde_json::json!({ "name": name, "is_error": *is_error }))
            }
            _ => None,
        };
        serde_json::json!({
            "event": event_name_for_point(ctx.point),
            "session_id": ctx.session_id,
            "turn": ctx.turn_count,
            "tool": tool,
        })
        .to_string()
    }

    /// Interpret a successful hook's stdout.
    ///
    /// Exit 0 means "allow", and that stays the default: anything that is not
    /// a recognized JSON decision object is ignored, so hooks that simply
    /// print progress keep working unchanged. The one recognized decision is
    /// a `PreToolUse` argument rewrite, in either the canonical spelling
    /// (`{"action":"modify","args":{…}}`) or the Claude Code-compatible one
    /// (`{"decision":"modify","tool_input":{…}}`).
    ///
    /// A malformed rewrite warns and falls through to `Continue` — a hook can
    /// never break the tool call it was inspecting.
    fn parse_success_stdout(&self, ctx: &HookContext, stdout: &str) -> HookAction {
        if stdout.is_empty() || !self.is_pre_tool() {
            return HookAction::Continue;
        }
        let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(stdout)
        else {
            return HookAction::Continue;
        };
        let verb = obj
            .get("action")
            .or_else(|| obj.get("decision"))
            .and_then(|v| v.as_str());
        if verb != Some("modify") {
            return HookAction::Continue;
        }
        if !matches!(ctx.data, HookData::ToolCall { .. }) {
            return HookAction::Continue;
        }
        match obj.get("args").or_else(|| obj.get("tool_input")) {
            Some(serde_json::Value::Object(args)) => HookAction::Modify {
                args: serde_json::Value::Object(args.clone()).to_string(),
            },
            _ => {
                tracing::warn!(
                    hook = %self.name,
                    "hook asked to modify a tool call but supplied no args object; running the call unchanged"
                );
                HookAction::Continue
            }
        }
    }

    fn execute_inner(&self, ctx: &HookContext) -> HookAction {
        if !self.matches_tool(ctx) {
            return HookAction::Continue;
        }
        let payload = self.build_payload(ctx);
        let started = Instant::now();
        let result = run_subprocess(&self.command, &payload, self.timeout, self.is_pre_tool());
        let duration_ms = started.elapsed().as_millis();
        // Completion trace on *every* arm, success included — a slow-but-OK hook
        // is otherwise invisible (only failures used to log). Low-cardinality
        // metadata only; command text, payload, stdout, and stderr are
        // deliberately excluded because they can carry secrets/PII.
        match result {
            Ok((0, stdout, _)) => {
                tracing::debug!(
                    hook = %self.name,
                    point = ?ctx.point,
                    outcome = "ok",
                    duration_ms,
                    "hook completed"
                );
                self.parse_success_stdout(ctx, &stdout)
            }
            Ok((code, _, stderr)) => {
                tracing::warn!(
                    hook = %self.name,
                    point = ?ctx.point,
                    outcome = "nonzero_exit",
                    exit_code = code,
                    duration_ms,
                    "hook exited nonzero"
                );
                if self.is_pre_tool() {
                    HookAction::Skip {
                        reason: Some(block_reason(&self.name, code, &stderr)),
                    }
                } else {
                    HookAction::Continue
                }
            }
            Err(SubprocessError::Spawn(e)) => {
                // Fail-open even on PreToolUse: a broken hook must not block every tool.
                tracing::warn!(
                    hook = %self.name,
                    point = ?ctx.point,
                    outcome = "spawn_failed",
                    duration_ms,
                    "hook spawn failed: {e}"
                );
                HookAction::Continue
            }
            Err(SubprocessError::Timeout) => {
                tracing::warn!(
                    hook = %self.name,
                    point = ?ctx.point,
                    outcome = "timeout",
                    duration_ms,
                    "hook timed out after {:?}",
                    self.timeout
                );
                if self.is_pre_tool() {
                    HookAction::Skip {
                        reason: Some(format!(
                            "PreToolUse hook '{}' blocked this tool call: hook timed out \
                             after {:?} without reaching a decision",
                            self.name, self.timeout
                        )),
                    }
                } else {
                    HookAction::Continue
                }
            }
            Err(SubprocessError::Io(e)) => {
                tracing::warn!(
                    hook = %self.name,
                    point = ?ctx.point,
                    outcome = "io_error",
                    duration_ms,
                    "hook io error: {e}"
                );
                HookAction::Continue
            }
        }
    }
}

#[async_trait]
impl Hook for ScriptHook {
    fn name(&self) -> &str {
        &self.name
    }

    fn points(&self) -> &[HookPoint] {
        &self.points
    }

    async fn execute(&self, ctx: &HookContext) -> HookAction {
        // `execute_inner` is fully synchronous — `std::process`, a stdin
        // writer thread, blocking pipe reads — and a hook's timeout can be up
        // to 600s. Running it inline parks a tokio worker thread for that
        // whole time, which starves the runtime rather than just delaying
        // this turn. `spawn_blocking` moves it to the blocking pool.
        //
        // A panic inside the closure surfaces as `Err(JoinError)`, so this
        // keeps the "hooks can never break the agent" guarantee that
        // `with_panic_guard` provided when the body ran inline.
        let hook = self.clone();
        let ctx = ctx.clone();

        if self.background {
            // Before the slot, not after: `execute_inner` checks the matcher,
            // so a background hook with a narrow matcher would otherwise burn
            // one of the four slots and a spawn on every tool call it does not
            // apply to.
            if !self.matches_tool(&ctx) {
                return HookAction::Continue;
            }
            // Bounded, and never awaited: the caller resumes now and the
            // hook's own timeout still bounds the child. Its action is
            // discarded by construction — a hook the operator asked not to
            // wait for must not steer what it could not delay.
            let Ok(permit) = background_slots().try_acquire() else {
                tracing::warn!(
                    hook = %self.name,
                    point = ?ctx.point,
                    outcome = "background_slots_exhausted",
                    "skipping background hook: {MAX_CONCURRENT_BACKGROUND_HOOKS} already running"
                );
                return HookAction::Continue;
            };
            permit.forget();
            let name = self.name.clone();
            tokio::spawn(async move {
                let outcome = tokio::task::spawn_blocking(move || hook.execute_inner(&ctx)).await;
                background_slots().add_permits(1);
                if let Err(e) = outcome {
                    tracing::warn!("background hook {name} failed to run ({e})");
                }
            });
            return HookAction::Continue;
        }

        match tokio::task::spawn_blocking(move || hook.execute_inner(&ctx)).await {
            Ok(action) => action,
            Err(e) => {
                tracing::warn!("hook {} failed to run ({e}); continuing", self.name);
                HookAction::Continue
            }
        }
    }
}

impl std::fmt::Debug for ScriptHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptHook")
            .field("name", &self.name)
            .field("points", &self.points)
            .field("has_matcher", &self.matcher.is_some())
            .field("timeout", &self.timeout)
            .field("background", &self.background)
            .finish()
    }
}

fn compile_matcher(raw: Option<&str>) -> Result<Option<Regex>, regex::Error> {
    match raw {
        None => Ok(None),
        Some("*") => Ok(None),
        Some(s) => Regex::new(s).map(Some),
    }
}

/// Cap on the block-reason text surfaced to the model when a PreToolUse hook
/// vetoes a tool call. Keeps a chatty hook from flooding the transcript while
/// leaving plenty of room for a real explanation.
const MAX_BLOCK_REASON_CHARS: usize = 2000;

/// Build the reason string surfaced to the model when a PreToolUse hook exits
/// nonzero. Hooks write their explanation to stderr (the hooks.json-compatible
/// convention documented in `docs/hooks.md`); an empty stderr falls back to
/// the exit code so the model at least learns which hook fired and how.
fn block_reason(hook: &str, exit_code: i32, stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        return format!(
            "PreToolUse hook '{hook}' blocked this tool call \
             (exit code {exit_code}, no reason given)"
        );
    }
    let mut reason: String = trimmed.chars().take(MAX_BLOCK_REASON_CHARS).collect();
    if reason.len() < trimmed.len() {
        reason.push_str(" [truncated]");
    }
    format!("PreToolUse hook '{hook}' blocked this tool call (exit code {exit_code}): {reason}")
}

#[derive(Debug)]
enum SubprocessError {
    Spawn(std::io::Error),
    Timeout,
    Io(std::io::Error),
}

/// Cap on retained hook output per stream, so a runaway script can't grow the
/// buffer unbounded. Reading continues past the cap (the excess is discarded) —
/// stopping the read would re-introduce the deadlock this cap exists alongside.
const MAX_HOOK_STREAM_BYTES: u64 = 8 * 1024;

/// How long to wait for a drain thread's retained bytes once the child itself
/// has exited. In the normal case the pipe is already at EOF and the value is
/// waiting; this only bounds the case where the hook leaked a pipe end to a
/// process that outlives it.
const STREAM_DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Spawn `sh -c <command>` with the payload on both `$1` and stdin, enforce
/// `timeout` via `process_tree::wait_or_kill_group` (which takes the hook's
/// whole process group with it on expiry).
///
/// All three pipes are serviced by dedicated threads *while* we poll, not after.
/// Draining only once `try_wait` reports exit is a deadlock: a hook that writes
/// more than the ~64 KiB pipe buffer blocks in `write`, so it never exits,
/// `try_wait` never returns `Some`, and a perfectly good hook is SIGKILLed at the
/// deadline and reported as a timeout. The same reasoning applies in reverse to
/// the stdin write.
/// Run a hook.
///
/// `wants_stdout` says whether the caller can act on the child's stdout — true
/// only for `PreToolUse`, the one event whose stdout carries a decision. When
/// false the stdout pipe is still drained (that is what keeps a chatty child
/// unblocked), but the caller does not wait around for a hook that leaked the
/// pipe to a background process.
fn run_subprocess(
    command: &str,
    payload: &str,
    timeout: Duration,
    wants_stdout: bool,
) -> Result<(i32, String, String), SubprocessError> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .arg("borg-hook") // $0 inside the script
        .arg(payload) // $1
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Hooks are the last agent-spawned subprocess site that still inherited the
    // daemon's whole environment — every provider API key the credential pool
    // loaded, plus `BORG_KEK_PASSPHRASE`. A hook is ordinary shell, frequently
    // copied from someone else's config, and its stdout is captured and logged,
    // so `env`/`set -x` inside one was a direct path from the keyring to
    // `~/.borg/logs/`. Same closed-set filter the shell tool already uses.
    // A hook that genuinely needs a credential must read it from the secret
    // store, not from whatever happened to be in the daemon's environment.
    crate::security::env_allowlist::apply_filtered_env_std(&mut cmd, &HashMap::new());
    // Own process group so the timeout path below can kill the hook's whole
    // tree — `child.kill()` alone leaves backgrounded grandchildren running
    // (and holding our pipes) past the deadline.
    crate::util::process_tree::spawn_in_own_group_std(&mut cmd);
    let mut child = cmd.spawn().map_err(SubprocessError::Spawn)?;
    // Windows counterpart to that process group. Deliberately *not*
    // kill-on-close: a hook that finishes normally may have detached a helper
    // on purpose (`some-daemon &`), and that helper must outlive it. Only the
    // timeout path below terminates the job.
    let job = mcp::job_object::contain(child.id(), "hook", mcp::job_object::OnClose::Leave);

    // Feed stdin from its own thread and let the handle drop, closing the pipe so
    // a hook blocked on `cat` sees EOF. A hook that exits without reading its
    // input yields `BrokenPipe` (or `EPIPE`), which is not an error: the hook's
    // exit status and output still stand. Any other write error is worth a warn,
    // but still must not mask the child's own result.
    let stdin_writer = child.stdin.take().map(|mut stdin| {
        let payload = payload.to_string();
        std::thread::spawn(move || match stdin.write_all(payload.as_bytes()) {
            Ok(()) => {
                let _ = stdin.flush();
            }
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(e) => tracing::warn!("hook stdin write failed: {e}"),
        })
    });

    let stdout_drain = child.stdout.take().map(spawn_stream_drain);
    let stderr_drain = child.stderr.take().map(spawn_stream_drain);

    let outcome = match crate::util::process_tree::wait_or_kill_group(&mut child, timeout) {
        Ok(status) => Ok(status.code().unwrap_or(-1)),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            // `wait_or_kill_group` already signalled the unix process group.
            // On Windows the job is the only thing that reaches the tree, and
            // a timed-out hook forfeits the "detached helpers survive" grace.
            if let Some(job) = &job {
                job.terminate();
            }
            Err(SubprocessError::Timeout)
        }
        Err(e) => Err(SubprocessError::Io(e)),
    };

    // The child is gone, but its pipes are NOT necessarily at EOF: a hook that
    // backgrounds anything without redirecting (`some-daemon &`) leaks the write
    // end of stdout/stderr — and the read end of stdin — to a grandchild that
    // outlives it. Joining these threads unconditionally would block for the
    // grandchild's entire lifetime. `ScriptHook::execute` runs this synchronously
    // on a runtime worker with no outer timeout, so that would hold a tool call
    // and one worker hostage while silently voiding the configured `timeout`.
    // Abandon the threads instead; each exits on its own once the last writer
    // closes, and none of them touch state the caller still owns.
    drop(stdin_writer);
    // stdout carries a hook's structured decision (see
    // `ScriptHook::parse_success_stdout`); draining it is also what keeps a
    // chatty child unblocked.
    //
    // Try first without blocking. A hook that backgrounds anything leaks the
    // write end of stdout to a grandchild, so the drain thread never reaches
    // EOF and the timed wait always burns its full grace — and this runs
    // synchronously on a runtime worker, on `PreToolUse`, for every tool call.
    // The wait is worth paying only when the caller can act on the content,
    // which is exactly when a decision is possible.
    let mut stdout_text = String::new();
    if let Some(rx) = stdout_drain {
        let received = match rx.try_recv() {
            Ok(buf) => Some(buf),
            Err(_) if wants_stdout => rx.recv_timeout(STREAM_DRAIN_GRACE).ok(),
            Err(_) => None,
        };
        if let Some(buf) = received {
            stdout_text = String::from_utf8_lossy(&buf).trim().to_string();
        }
    }
    // stderr is both logged and returned to the caller: on a PreToolUse veto
    // it becomes the block reason the model sees.
    let mut stderr_text = String::new();
    if let Some(rx) = stderr_drain {
        if let Ok(buf) = rx.recv_timeout(STREAM_DRAIN_GRACE) {
            stderr_text = String::from_utf8_lossy(&buf).trim().to_string();
            if !stderr_text.is_empty() {
                tracing::warn!("hook stderr: {stderr_text}");
            }
        }
    }

    outcome.map(|code| (code, stdout_text, stderr_text))
}

/// Read a child pipe to EOF on a dedicated thread, retaining at most
/// [`MAX_HOOK_STREAM_BYTES`] and discarding the rest.
///
/// Reading all the way to EOF is what keeps the child unblocked. Returning early
/// at the cap would leave a chatty hook stuck writing into a full pipe, which is
/// exactly the deadlock the caller's threading exists to prevent.
///
/// The retained bytes come back over a channel rather than a `JoinHandle` so the
/// caller can give up on a thread that is still waiting on a leaked pipe — see
/// [`STREAM_DRAIN_GRACE`].
fn spawn_stream_drain<R: Read + Send + 'static>(
    mut stream: R,
) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut retained = Vec::new();
        if let Err(e) = (&mut stream)
            .take(MAX_HOOK_STREAM_BYTES)
            .read_to_end(&mut retained)
        {
            tracing::debug!("hook stream read failed: {e}");
            // Send what we got: the caller may already have stopped listening,
            // in which case the error is just a dropped receiver.
            let _ = tx.send(retained);
            return;
        }
        // Anything past the cap is discarded, but still consumed.
        if let Err(e) = std::io::copy(&mut stream, &mut std::io::sink()) {
            tracing::debug!("hook stream drain failed: {e}");
        }
        let _ = tx.send(retained);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{HookAction, HookContext, HookData, HookPoint, HookRegistry};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        TempDir::new().expect("tempdir")
    }

    fn write_hooks_file(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("hooks.json");
        std::fs::write(&path, body).expect("write hooks.json");
        path
    }

    fn make_tool_ctx(point: HookPoint, tool_name: &str) -> HookContext {
        HookContext {
            point,
            session_id: "s_abc".to_string(),
            turn_count: 4,
            data: HookData::ToolCall {
                name: tool_name.to_string(),
                args: "{}".to_string(),
            },
        }
    }

    fn make_tool_result_ctx(point: HookPoint, tool_name: &str, is_error: bool) -> HookContext {
        HookContext {
            point,
            session_id: "s_abc".to_string(),
            turn_count: 4,
            data: HookData::ToolResult {
                name: tool_name.to_string(),
                result: "ok".to_string(),
                is_error,
            },
        }
    }

    fn make_simple_ctx(point: HookPoint) -> HookContext {
        HookContext {
            point,
            session_id: "s_abc".to_string(),
            turn_count: 4,
            data: HookData::SessionStart {
                session_id: "s_abc".to_string(),
            },
        }
    }

    fn make_hook(command: &str, point: HookPoint) -> ScriptHook {
        ScriptHook {
            name: format!("test-{:?}", point),
            points: vec![point],
            matcher: None,
            command: command.to_string(),
            timeout: Duration::from_secs(5),
            background: false,
        }
    }

    /// Captures each tracing event's fields as one `field=value …` string.
    struct EventRecorder(Arc<std::sync::Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventRecorder {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Grab(Vec<String>);
            impl tracing::field::Visit for Grab {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0.push(format!("{}={:?}", field.name(), value));
                }
            }
            let mut grab = Grab(Vec::new());
            event.record(&mut grab);
            self.0.lock().unwrap().push(grab.0.join(" "));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn successful_hook_emits_completion_trace_with_duration() {
        // A hook that exits 0 used to be completely silent. It must now emit a
        // completion event carrying the outcome and a duration so slow-but-OK
        // hooks are observable — the whole point of this change.
        use tracing_subscriber::layer::SubscriberExt;
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(EventRecorder(events.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);

        let hook = make_hook("exit 0", HookPoint::AfterToolCall);
        let ctx = make_tool_ctx(HookPoint::AfterToolCall, "run_shell");
        assert!(matches!(hook.execute_inner(&ctx), HookAction::Continue));

        let recorded = events.lock().unwrap();
        assert!(
            recorded
                .iter()
                .any(|e| e.contains("outcome=") && e.contains("ok") && e.contains("duration_ms=")),
            "expected success completion event with duration, got: {recorded:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nonzero_hook_completion_trace_records_outcome_and_duration() {
        // The failure path already logged; assert it now also carries the
        // structured outcome + duration and still returns Skip on a pre-tool
        // hook (behavior unchanged).
        use tracing_subscriber::layer::SubscriberExt;
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(EventRecorder(events.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);

        let hook = make_hook("exit 3", HookPoint::BeforeToolCall);
        let ctx = make_tool_ctx(HookPoint::BeforeToolCall, "run_shell");
        assert!(matches!(hook.execute_inner(&ctx), HookAction::Skip { .. }));

        let recorded = events.lock().unwrap();
        assert!(
            recorded
                .iter()
                .any(|e| e.contains("nonzero_exit") && e.contains("duration_ms=")),
            "expected nonzero completion event with duration, got: {recorded:?}"
        );
    }

    // ── config parsing ──

    mod config_tests {
        use super::*;

        #[test]
        fn parses_minimal_valid_config() {
            let dir = tmp();
            let path = write_hooks_file(
                dir.path(),
                r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"echo hi","timeout":5}]}]}}"#,
            );
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0].points, vec![HookPoint::AfterToolCall]);
            assert_eq!(hooks[0].timeout, Duration::from_secs(5));
            assert!(hooks[0].matcher.is_none());
        }

        #[test]
        fn parses_all_six_event_keys() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "SessionStart":     [{"hooks":[{"type":"command","command":"a"}]}],
                "SessionEnd":       [{"hooks":[{"type":"command","command":"b"}]}],
                "UserPromptSubmit": [{"hooks":[{"type":"command","command":"c"}]}],
                "PreToolUse":       [{"hooks":[{"type":"command","command":"d"}]}],
                "PostToolUse":      [{"hooks":[{"type":"command","command":"e"}]}],
                "Stop":             [{"hooks":[{"type":"command","command":"f"}]}]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 6);
            let mut points: Vec<HookPoint> = hooks.iter().flat_map(|h| h.points.clone()).collect();
            points.sort_by_key(|p| format!("{p:?}"));
            let mut expected = vec![
                HookPoint::SessionStart,
                HookPoint::SessionEnd,
                HookPoint::BeforeAgentStart,
                HookPoint::BeforeToolCall,
                HookPoint::AfterToolCall,
                HookPoint::TurnComplete,
            ];
            expected.sort_by_key(|p| format!("{p:?}"));
            assert_eq!(points, expected);
        }

        #[test]
        fn missing_file_returns_empty_not_error() {
            let dir = tmp();
            let path = dir.path().join("does-not-exist.json");
            assert!(ScriptHook::load_from_file(&path).is_empty());
        }

        #[test]
        fn empty_hooks_object_returns_empty() {
            let dir = tmp();
            let path = write_hooks_file(dir.path(), r#"{"hooks":{}}"#);
            assert!(ScriptHook::load_from_file(&path).is_empty());
        }

        #[test]
        fn completely_empty_file_returns_empty() {
            let dir = tmp();
            let path = write_hooks_file(dir.path(), r#"{}"#);
            assert!(ScriptHook::load_from_file(&path).is_empty());
        }

        #[test]
        fn malformed_json_returns_empty() {
            let dir = tmp();
            let path = write_hooks_file(dir.path(), "not json at all {{{ ");
            assert!(ScriptHook::load_from_file(&path).is_empty());
        }

        #[test]
        fn root_level_event_is_rejected_not_silently_ignored() {
            // A common mistake: hook events at the top level instead of nested
            // under "hooks". Without deny_unknown_fields this parses to zero
            // hooks with no signal. We require an explicit parse error so the
            // load path can warn the user.
            let err = serde_json::from_str::<HooksFile>(
                r#"{"SessionStart":[{"hooks":[{"type":"command","command":"echo hi"}]}]}"#,
            )
            .expect_err("root-level hook events must be rejected");
            assert!(
                err.to_string().contains("unknown field `SessionStart`"),
                "unexpected parse error: {err}"
            );
        }

        #[test]
        fn root_level_event_file_warns_and_loads_no_hooks() {
            // A misplaced event key at the root degrades to zero hooks (logged
            // via warn), never an Err or panic — hooks can never break the agent.
            // The differential against the correctly-nested form proves the
            // emptiness is the *misplacement* (which deny_unknown_fields now
            // rejects), not a generally-broken loader: the byte-identical hook
            // loads fine once nested under "hooks".
            let hook = r#"[{"hooks":[{"type":"command","command":"echo hi"}]}]"#;

            let bad_dir = tmp();
            let bad = write_hooks_file(bad_dir.path(), &format!(r#"{{"PostToolUse":{hook}}}"#));
            assert!(
                ScriptHook::load_from_file(&bad).is_empty(),
                "a root-level event key must degrade to zero hooks, not load or panic"
            );

            let good_dir = tmp();
            let good = write_hooks_file(
                good_dir.path(),
                &format!(r#"{{"hooks":{{"PostToolUse":{hook}}}}}"#),
            );
            let loaded = ScriptHook::load_from_file(&good);
            assert_eq!(
                loaded.len(),
                1,
                "the same hook must load when correctly nested"
            );
            assert_eq!(loaded[0].points, vec![HookPoint::AfterToolCall]);
        }

        #[test]
        fn top_level_metadata_is_tolerated_and_hooks_still_load() {
            // A user-authored hooks.json may carry benign top-level metadata:
            // `$schema` for editor tooling and a human `description`. These must
            // not disable the hooks — regression against `deny_unknown_fields`
            // rejecting the whole file (which degrades to zero hooks + a warn).
            let dir = tmp();
            let body = r#"{
                "$schema": "https://example.com/hooks.schema.json",
                "description": "stop-time review gate",
                "hooks": {"PostToolUse":[{"hooks":[{"type":"command","command":"echo hi"}]}]}
            }"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(
                hooks.len(),
                1,
                "top-level $schema/description must be tolerated; hooks must still load"
            );
            assert_eq!(hooks[0].points, vec![HookPoint::AfterToolCall]);

            // The tolerance is a deliberate allow-list of known metadata, not a
            // blanket relaxation: an unrecognized top-level key is still a hard
            // parse error (which the loader turns into a warn + zero hooks).
            assert!(
                serde_json::from_str::<HooksFile>(r#"{"version": 2, "hooks": {}}"#).is_err(),
                "unknown top-level keys must still be rejected"
            );
        }

        #[test]
        fn unknown_event_key_warns_and_skips() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "Bogus":       [{"hooks":[{"type":"command","command":"nope"}]}],
                "PostToolUse": [{"hooks":[{"type":"command","command":"ok"}]}]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0].points, vec![HookPoint::AfterToolCall]);
        }

        #[test]
        fn invalid_matcher_regex_warns_and_skips() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "PreToolUse": [
                    {"matcher":"[","hooks":[{"type":"command","command":"bad"}]},
                    {"matcher":"run_shell","hooks":[{"type":"command","command":"good"}]}
                ]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert!(hooks[0].matcher.is_some());
        }

        #[test]
        fn missing_command_field_skips_entry() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "PostToolUse": [{"hooks":[
                    {"type":"command"},
                    {"type":"command","command":"ok"}
                ]}]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0].command, "ok");
        }

        #[test]
        fn empty_command_string_skipped() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "PostToolUse": [{"hooks":[
                    {"type":"command","command":"   "},
                    {"type":"command","command":"real"}
                ]}]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0].command, "real");
        }

        #[test]
        fn timeout_clamped_to_max_600() {
            let dir = tmp();
            let body = r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"x","timeout":9999}]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks[0].timeout, Duration::from_secs(600));
        }

        #[test]
        fn timeout_clamped_to_min_1() {
            let dir = tmp();
            let body = r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"x","timeout":0}]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks[0].timeout, Duration::from_secs(1));
        }

        #[test]
        fn timeout_default_60_when_absent() {
            let dir = tmp();
            let body =
                r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"x"}]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks[0].timeout, Duration::from_secs(60));
        }

        #[test]
        fn matcher_star_matches_all_tools() {
            let dir = tmp();
            let body = r#"{"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"x"}]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            // "*" is treated as "no matcher" (match all).
            assert!(hooks[0].matcher.is_none());
        }

        #[test]
        fn non_command_type_skipped() {
            let dir = tmp();
            let body = r#"{"hooks":{"PostToolUse":[{"hooks":[
                {"type":"prompt"},
                {"type":"agent"},
                {"type":"command","command":"keep"}
            ]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0].command, "keep");
        }

        #[test]
        fn hook_name_encodes_position() {
            let dir = tmp();
            let body = r#"{"hooks":{"PostToolUse":[{"hooks":[
                {"type":"command","command":"a"},
                {"type":"command","command":"b"}
            ]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks[0].name(), "PostToolUse[0][0]");
            assert_eq!(hooks[1].name(), "PostToolUse[0][1]");
        }
    }

    // ── execution ──

    mod execute_tests {
        use super::*;

        #[tokio::test]
        async fn exit_zero_returns_continue() {
            let hook = make_hook("true", HookPoint::AfterToolCall);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            assert!(matches!(hook.execute(&ctx).await, HookAction::Continue));
        }

        #[tokio::test]
        async fn pre_tool_use_nonzero_returns_skip() {
            let hook = make_hook("false", HookPoint::BeforeToolCall);
            let ctx = make_tool_ctx(HookPoint::BeforeToolCall, "run_shell");
            assert!(matches!(hook.execute(&ctx).await, HookAction::Skip { .. }));
        }

        /// A blocking hook's stderr is the reason channel: the skip must carry
        /// it (plus which hook fired and the exit code) so the model is not
        /// left staring at an unexplained abort.
        #[tokio::test]
        async fn pre_tool_use_block_carries_stderr_as_reason() {
            let hook = make_hook(
                "echo 'refusing: workspace is frozen' >&2; exit 2",
                HookPoint::BeforeToolCall,
            );
            let ctx = make_tool_ctx(HookPoint::BeforeToolCall, "run_shell");
            match hook.execute(&ctx).await {
                HookAction::Skip { reason: Some(r) } => {
                    assert!(
                        r.contains("refusing: workspace is frozen"),
                        "reason must carry the hook's stderr, got: {r}"
                    );
                    assert!(
                        r.contains(&hook.name) && r.contains("exit code 2"),
                        "reason must name the hook and exit code, got: {r}"
                    );
                }
                other => panic!("expected Skip with reason, got {other:?}"),
            }
        }

        /// Empty stderr falls back to an exit-code message — never a reasonless
        /// block.
        #[tokio::test]
        async fn pre_tool_use_block_without_stderr_reports_exit_code() {
            let hook = make_hook("exit 3", HookPoint::BeforeToolCall);
            let ctx = make_tool_ctx(HookPoint::BeforeToolCall, "run_shell");
            match hook.execute(&ctx).await {
                HookAction::Skip { reason: Some(r) } => {
                    assert!(
                        r.contains("exit code 3") && r.contains("no reason given"),
                        "empty stderr must fall back to the exit code, got: {r}"
                    );
                }
                other => panic!("expected Skip with fallback reason, got {other:?}"),
            }
        }

        /// A timed-out PreToolUse hook still blocks the call, and the reason
        /// says so — a silent Skip here regressed real users before.
        #[tokio::test]
        async fn pre_tool_use_timeout_reason_mentions_timeout() {
            let mut hook = make_hook("sleep 10", HookPoint::BeforeToolCall);
            hook.timeout = Duration::from_secs(1);
            let ctx = make_tool_ctx(HookPoint::BeforeToolCall, "run_shell");
            match hook.execute(&ctx).await {
                HookAction::Skip { reason: Some(r) } => {
                    assert!(
                        r.contains("timed out"),
                        "timeout skip must explain itself, got: {r}"
                    );
                }
                other => panic!("expected Skip with timeout reason, got {other:?}"),
            }
        }

        /// The surfaced reason is length-bounded so a chatty hook cannot flood
        /// the transcript, and the truncation is marked.
        #[test]
        fn block_reason_is_length_bounded() {
            let long = "x".repeat(3 * MAX_BLOCK_REASON_CHARS);
            let reason = block_reason("PreToolUse[0][0]", 1, &long);
            assert!(
                reason.len() < MAX_BLOCK_REASON_CHARS + 200,
                "reason must be bounded, got {} chars",
                reason.len()
            );
            assert!(reason.contains("[truncated]"));

            let short = block_reason("PreToolUse[0][0]", 1, "  keep me  ");
            assert!(
                short.ends_with("keep me"),
                "short stderr is trimmed, not truncated: {short}"
            );
        }

        #[tokio::test]
        async fn pre_tool_use_zero_returns_continue() {
            let hook = make_hook("true", HookPoint::BeforeToolCall);
            let ctx = make_tool_ctx(HookPoint::BeforeToolCall, "run_shell");
            assert!(matches!(hook.execute(&ctx).await, HookAction::Continue));
        }

        #[tokio::test]
        async fn post_tool_use_nonzero_returns_continue() {
            let hook = make_hook("false", HookPoint::AfterToolCall);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            assert!(matches!(hook.execute(&ctx).await, HookAction::Continue));
        }

        #[tokio::test]
        async fn session_start_nonzero_returns_continue() {
            let hook = make_hook("false", HookPoint::SessionStart);
            let ctx = make_simple_ctx(HookPoint::SessionStart);
            assert!(matches!(hook.execute(&ctx).await, HookAction::Continue));
        }

        #[tokio::test]
        async fn timeout_non_pre_tool_returns_continue() {
            let mut hook = make_hook("sleep 10", HookPoint::AfterToolCall);
            hook.timeout = Duration::from_secs(1);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            let start = Instant::now();
            let action = hook.execute(&ctx).await;
            assert!(matches!(action, HookAction::Continue));
            assert!(
                start.elapsed() < Duration::from_secs(4),
                "should reap quickly"
            );
        }

        #[tokio::test]
        async fn timeout_pre_tool_returns_skip() {
            let mut hook = make_hook("sleep 10", HookPoint::BeforeToolCall);
            hook.timeout = Duration::from_secs(1);
            let ctx = make_tool_ctx(HookPoint::BeforeToolCall, "run_shell");
            let start = Instant::now();
            let action = hook.execute(&ctx).await;
            assert!(matches!(action, HookAction::Skip { .. }));
            assert!(start.elapsed() < Duration::from_secs(4));
        }

        #[tokio::test]
        async fn spawn_failure_returns_continue_even_on_pre_tool() {
            // Fail-open on spawn for PreToolUse: a typo shouldn't block every tool.
            // We simulate a spawn-ish failure by giving sh a command it runs but fails.
            // Actual spawn failure requires sh itself to be absent, which we can't test
            // portably — but we exercise the broader non-zero-exit path via `false`.
            // For the actual spawn failure path see fault_tests::spawn_failure_does_not_panic.
            let hook = make_hook("exit 127", HookPoint::BeforeToolCall);
            let ctx = make_tool_ctx(HookPoint::BeforeToolCall, "run_shell");
            // 127 from the shell means "command not found" — still a non-zero exit
            // so we map to Skip for PreToolUse. This matches Claude Code semantics.
            assert!(matches!(hook.execute(&ctx).await, HookAction::Skip { .. }));
        }

        #[tokio::test]
        async fn payload_json_reaches_child_as_argv1() {
            let dir = tmp();
            let out = dir.path().join("payload.txt");
            // Escape the path for shell inclusion — tempdir paths are well-formed on test hosts.
            let cmd = format!(
                "printf '%s' \"$1\" > {}",
                shell_quote(out.to_str().unwrap())
            );
            let hook = make_hook(&cmd, HookPoint::AfterToolCall);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            let action = hook.execute(&ctx).await;
            assert!(matches!(action, HookAction::Continue));
            let contents = std::fs::read_to_string(&out).expect("payload file written");
            assert!(
                contents.contains(r#""event":"PostToolUse""#),
                "got {contents:?}"
            );
            assert!(contents.contains(r#""session_id":"s_abc""#));
            assert!(contents.contains(r#""turn":4"#));
            assert!(contents.contains(r#""name":"run_shell""#));
            assert!(contents.contains(r#""is_error":false"#));
        }

        fn shell_quote(s: &str) -> String {
            // Very small shell quoter for test paths (no single quotes expected).
            format!("'{}'", s.replace('\'', "'\\''"))
        }
    }

    // ── matcher filtering ──

    mod matcher_tests {
        use super::*;

        fn hook_with_matcher(matcher: &str, point: HookPoint, command: &str) -> ScriptHook {
            ScriptHook {
                name: format!("matcher-{matcher}"),
                points: vec![point],
                matcher: Some(Regex::new(matcher).expect("valid regex")),
                command: command.to_string(),
                timeout: Duration::from_secs(5),
                background: false,
            }
        }

        #[tokio::test]
        async fn matcher_regex_matches_tool_name() {
            let dir = tmp();
            let marker = dir.path().join("fired.txt");
            let cmd = format!("touch '{}'", marker.display());
            let hook = hook_with_matcher("run_shell", HookPoint::AfterToolCall, &cmd);

            // Matches
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            let _ = hook.execute(&ctx).await;
            assert!(marker.exists(), "hook should have fired for run_shell");

            // Clear and try non-matching
            std::fs::remove_file(&marker).ok();
            let ctx2 = make_tool_result_ctx(HookPoint::AfterToolCall, "read_file", false);
            let _ = hook.execute(&ctx2).await;
            assert!(!marker.exists(), "hook should NOT have fired for read_file");
        }

        #[tokio::test]
        async fn matcher_alternation() {
            let dir = tmp();
            let marker = dir.path().join("fired.txt");
            let cmd = format!("touch '{}'", marker.display());
            let hook =
                hook_with_matcher("apply_patch|write_memory", HookPoint::AfterToolCall, &cmd);

            for tool in ["apply_patch", "write_memory"] {
                let _ = std::fs::remove_file(&marker);
                let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, tool, false);
                let _ = hook.execute(&ctx).await;
                assert!(marker.exists(), "expected fire for {tool}");
            }

            let _ = std::fs::remove_file(&marker);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "read_file", false);
            let _ = hook.execute(&ctx).await;
            assert!(!marker.exists(), "should not fire for read_file");
        }

        #[tokio::test]
        async fn matcher_absent_matches_all() {
            let dir = tmp();
            let marker = dir.path().join("fired.txt");
            let cmd = format!("touch '{}'", marker.display());
            let hook = make_hook(&cmd, HookPoint::AfterToolCall);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "anything", false);
            let _ = hook.execute(&ctx).await;
            assert!(marker.exists());
        }

        #[tokio::test]
        async fn matcher_ignored_on_non_tool_events() {
            // Hook has a tool-name matcher, but SessionStart has no tool name.
            // Implementation returns true (ignore matcher) for non-tool events.
            let dir = tmp();
            let marker = dir.path().join("fired.txt");
            let cmd = format!("touch '{}'", marker.display());
            let hook = hook_with_matcher("run_shell", HookPoint::SessionStart, &cmd);
            let ctx = make_simple_ctx(HookPoint::SessionStart);
            let _ = hook.execute(&ctx).await;
            assert!(
                marker.exists(),
                "matcher should be no-op on non-tool events"
            );
        }
    }

    // ── loader + registry integration ──

    mod loader_tests {
        use super::*;

        #[test]
        fn load_all_reads_from_temp_config() {
            let dir = tmp();
            let body = r#"{"hooks":{"PostToolUse":[{"matcher":"run_shell","hooks":[{"type":"command","command":"true","timeout":3}]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0].timeout, Duration::from_secs(3));
            assert!(hooks[0].matcher.is_some());
        }

        #[tokio::test]
        async fn registered_hooks_dispatched_via_registry() {
            let dir = tmp();
            let marker_a = dir.path().join("a.txt");
            let marker_b = dir.path().join("b.txt");
            let hook_a = make_hook(
                &format!("touch '{}'", marker_a.display()),
                HookPoint::AfterToolCall,
            );
            let hook_b = make_hook(
                &format!("touch '{}'", marker_b.display()),
                HookPoint::AfterToolCall,
            );

            let mut reg = HookRegistry::new();
            reg.register(Box::new(hook_a));
            reg.register(Box::new(hook_b));

            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            let action = reg.dispatch(&ctx).await;
            assert!(matches!(action, HookAction::Continue));
            assert!(marker_a.exists());
            assert!(marker_b.exists());
        }

        #[tokio::test]
        async fn skip_short_circuits_subsequent_hooks() {
            let dir = tmp();
            let marker = dir.path().join("should_not_exist.txt");
            let skipper = make_hook("false", HookPoint::BeforeToolCall);
            let toucher = make_hook(
                &format!("touch '{}'", marker.display()),
                HookPoint::BeforeToolCall,
            );

            let mut reg = HookRegistry::new();
            reg.register(Box::new(skipper));
            reg.register(Box::new(toucher));

            let ctx = make_tool_ctx(HookPoint::BeforeToolCall, "run_shell");
            let action = reg.dispatch(&ctx).await;
            assert!(matches!(action, HookAction::Skip { .. }));
            assert!(!marker.exists(), "second hook must not run after Skip");
        }

        #[test]
        fn disabled_flag_returns_empty() {
            // load_all with enabled=false must return empty regardless of disk state.
            let hooks = ScriptHook::load_all(false);
            assert!(hooks.is_empty());
        }
    }

    // ── fault isolation ──

    mod fault_tests {
        use super::*;

        #[test]
        fn malformed_json_does_not_crash_loader() {
            let dir = tmp();
            let path = write_hooks_file(dir.path(), "totally not json");
            let hooks = ScriptHook::load_from_file(&path);
            assert!(hooks.is_empty());
        }

        #[tokio::test]
        async fn spawn_failure_does_not_panic() {
            // Run `sh -c` with a command that exits nonzero to exercise the non-zero path.
            // Actual sh-missing is not portably testable.
            let hook = make_hook(
                "/definitely/nonexistent/binary --flag",
                HookPoint::AfterToolCall,
            );
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            let action = hook.execute(&ctx).await;
            // sh runs, command-not-found → exit 127. PostToolUse → Continue.
            assert!(matches!(action, HookAction::Continue));
        }

        #[tokio::test]
        async fn a_slow_hook_does_not_block_the_async_runtime() {
            // "Hooks can never break the agent" includes not wedging the
            // runtime. `execute_inner` is synchronous and a hook timeout can
            // be up to 600s, so running it inline parked a tokio worker thread
            // for the duration instead of just delaying this turn.
            let hook = make_hook("sleep 1; exit 0", HookPoint::AfterToolCall);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", true);

            let progressed = Arc::new(AtomicBool::new(false));
            let flag = progressed.clone();
            let ticker = tokio::spawn(async move {
                // Must get scheduled while the hook is still running.
                tokio::time::sleep(Duration::from_millis(100)).await;
                flag.store(true, Ordering::SeqCst);
            });

            let action = hook.execute(&ctx).await;
            ticker.await.expect("concurrent task must complete");

            assert!(matches!(action, HookAction::Continue));
            assert!(
                progressed.load(Ordering::SeqCst),
                "another task must have run while the blocking hook was in flight"
            );
        }

        #[tokio::test]
        async fn non_utf8_stdout_does_not_crash() {
            // Bash prints raw bytes, hook runner must not panic.
            let hook = make_hook("printf '\\xff\\xfe\\xfd'; exit 0", HookPoint::AfterToolCall);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            let action = hook.execute(&ctx).await;
            assert!(matches!(action, HookAction::Continue));
        }

        #[test]
        fn invalid_regex_entry_does_not_disable_others() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "PreToolUse": [
                    {"matcher":"[bad","hooks":[{"type":"command","command":"x"}]},
                    {"matcher":"ok.*","hooks":[{"type":"command","command":"y"}]}
                ],
                "PostToolUse": [{"hooks":[{"type":"command","command":"z"}]}]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            // Invalid matcher group dropped; the other two load.
            assert_eq!(hooks.len(), 2);
        }

        #[test]
        fn garbage_event_key_does_not_disable_others() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "Nope":        [{"hooks":[{"type":"command","command":"x"}]}],
                "PostToolUse": [{"hooks":[{"type":"command","command":"y"}]}]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0].command, "y");
        }

        /// Poll for `marker` until it appears, up to `limit`.
        async fn marker_appears(marker: &Path, limit: Duration) -> bool {
            let deadline = Instant::now() + limit;
            while Instant::now() < deadline {
                if marker.exists() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            false
        }

        /// Spec: `"async": true` means the caller does not wait — and the hook
        /// still runs, with its exit code discarded. Asserting only on the
        /// returned `Continue` would pass even if the hook were dropped on the
        /// floor, so the marker file is what proves it really executed.
        #[tokio::test]
        async fn a_background_hook_runs_detached_and_never_steers() {
            let dir = tmp();
            let marker = dir.path().join("ran");
            let mut hook = make_hook(
                &format!("sleep 0.3; touch {}; exit 2", marker.display()),
                HookPoint::AfterToolCall,
            );
            hook.background = true;
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);

            let start = Instant::now();
            let action = hook.execute(&ctx).await;
            let elapsed = start.elapsed();

            assert!(
                matches!(action, HookAction::Continue),
                "a discarded exit code must never steer the operation"
            );
            assert!(
                elapsed < Duration::from_secs(1),
                "the caller must resume immediately, elapsed={elapsed:?}"
            );
            assert!(
                !marker.exists(),
                "the caller resumed after the hook finished — it was not detached"
            );
            assert!(
                marker_appears(&marker, Duration::from_secs(10)).await,
                "the detached hook never ran"
            );
        }

        /// Spec: the matcher is checked before a slot is taken. There are only
        /// four background slots process-wide, so a narrowly-matched hook that
        /// spawned on every tool call would starve the ones that do apply.
        #[tokio::test]
        async fn a_background_hook_that_does_not_match_never_spawns() {
            let dir = tmp();
            let marker = dir.path().join("ran");
            let mut hook = make_hook(
                &format!("touch {}", marker.display()),
                HookPoint::AfterToolCall,
            );
            hook.background = true;
            hook.matcher = Some(Regex::new("^fs_write$").expect("regex"));
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);

            assert!(matches!(hook.execute(&ctx).await, HookAction::Continue));
            assert!(
                !marker_appears(&marker, Duration::from_secs(1)).await,
                "a hook whose matcher excludes the tool must not run"
            );
        }

        /// Spec: `async` must survive the config round-trip, and the two events
        /// that cannot honour it must be corrected at load time rather than
        /// leaving the operator with a hook that quietly does the wrong thing.
        #[test]
        fn async_is_parsed_and_refused_where_it_cannot_work() {
            let dir = tmp();
            let body = r#"{"hooks":{
                "PostToolUse": [{"hooks":[{"type":"command","command":"notify","async":true}]}],
                "PreToolUse":  [{"hooks":[{"type":"command","command":"gate","async":true}]}],
                "SessionEnd":  [{"hooks":[{"type":"command","command":"flush","async":true}]}],
                "Stop":        [{"hooks":[{"type":"command","command":"plain"}]}]
            }}"#;
            let path = write_hooks_file(dir.path(), body);
            let hooks = ScriptHook::load_from_file(&path);

            let by_command = |cmd: &str| {
                hooks
                    .iter()
                    .find(|h| h.command == cmd)
                    .unwrap_or_else(|| panic!("{cmd} hook must load"))
                    .background
            };
            assert!(by_command("notify"), "PostToolUse honours async");
            assert!(
                !by_command("gate"),
                "PreToolUse must stay synchronous — honouring async would silently \
                 disable the gate a nonzero exit is supposed to apply"
            );
            assert!(
                !by_command("flush"),
                "SessionEnd cannot run detached — the session is gone before it finishes"
            );
            assert!(
                !by_command("plain"),
                "omitting async keeps hooks synchronous"
            );
        }

        #[tokio::test]
        async fn timeout_kills_and_reaps_child() {
            let mut hook = make_hook("sleep 30", HookPoint::AfterToolCall);
            hook.timeout = Duration::from_secs(1);
            let ctx = make_tool_result_ctx(HookPoint::AfterToolCall, "run_shell", false);
            let start = Instant::now();
            let action = hook.execute(&ctx).await;
            let elapsed = start.elapsed();
            assert!(matches!(action, HookAction::Continue));
            assert!(
                elapsed < Duration::from_secs(3),
                "child should be killed promptly, elapsed={elapsed:?}"
            );
        }

        /// A timed-out hook must not leave backgrounded grandchildren running:
        /// killing only the direct `sh` reparents the grandchild to init, where
        /// it keeps holding our pipes (and doing whatever it was doing) with
        /// nothing left to reap it. The group kill has to take out the tree.
        #[cfg(unix)]
        #[tokio::test]
        async fn timeout_kills_the_hooks_whole_process_tree() {
            let dir = tmp();
            let pid_file = dir.path().join("grandchild.pid");
            let command = format!("sleep 30 & echo $! > {}; wait", pid_file.display());
            let result = tokio::task::spawn_blocking(move || {
                run_subprocess(&command, "{}", Duration::from_secs(1), true)
            })
            .await
            .expect("join");
            assert!(matches!(result, Err(SubprocessError::Timeout)));

            let grandchild: i32 = std::fs::read_to_string(&pid_file)
                .expect("hook should have recorded the grandchild pid")
                .trim()
                .parse()
                .expect("pid");
            assert!(
                crate::util::process_tree::wait_for_pid_exit(grandchild).await,
                "grandchild {grandchild} survived the hook timeout"
            );
        }

        #[test]
        fn empty_hook_array_within_group_ok() {
            let dir = tmp();
            let body = r#"{"hooks":{"PostToolUse":[{"matcher":"x","hooks":[]}]}}"#;
            let path = write_hooks_file(dir.path(), body);
            assert!(ScriptHook::load_from_file(&path).is_empty());
        }

        /// Claude Code hooks read their payload from stdin. `docs/hooks.md`
        /// advertises schema compatibility, so a script copied from a Claude Code
        /// config must work — it used to block on `cat` until borg's timeout,
        /// which on `PreToolUse` silently became `Skip` and blocked the tool.
        #[test]
        fn payload_is_delivered_on_stdin() {
            let dir = tmp();
            let out = dir.path().join("stdin-payload.json");
            let command = format!("cat > {}", out.display());
            let (code, _stdout, _stderr) = run_subprocess(
                &command,
                r#"{"event":"SessionStart","turn":7}"#,
                Duration::from_secs(10),
                true,
            )
            .expect("hook must run");
            assert_eq!(code, 0);
            let captured = std::fs::read_to_string(&out).expect("hook should have written stdin");
            assert_eq!(captured, r#"{"event":"SessionStart","turn":7}"#);
        }

        /// Serializes the one test below that mutates the process-global env.
        /// Nothing else in this module touches it, and the names it sets are
        /// deliberately unique to this test, so a private lock is enough.
        fn hook_env_lock() -> std::sync::MutexGuard<'static, ()> {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            LOCK.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// Spec: a hook must not see the daemon's credentials. The daemon env
        /// holds every provider key the credential pool loaded plus
        /// `BORG_KEK_PASSPHRASE`, and a hook's stdout is captured and written
        /// to `~/.borg/logs/`, so an inherited key is one `env` away from disk.
        /// The name is checked as well as the value: a hook that reads
        /// `${…_API_KEY}` must get nothing, not a shortened secret.
        ///
        /// The probe names are private to this test on purpose. A real provider
        /// name (`OPENROUTER_API_KEY`) is saved, overwritten and restored by
        /// `config::tests::secrets_llm` under *its* lock, and is commonly
        /// exported on a developer's host — setting or clearing it here would
        /// flake those tests in both directions. The allowlist's own suite
        /// covers the real provider names.
        #[test]
        fn hook_env_excludes_daemon_credentials_but_keeps_process_basics() {
            let _lock = hook_env_lock();
            let dir = tmp();
            let out = dir.path().join("env.txt");
            // SAFETY: these two names are used nowhere else in the workspace,
            // and `hook_env_lock` serializes this test against itself.
            std::env::set_var("HOOK_PROBE_PROVIDER_API_KEY", "leak_value_must_not_appear");
            std::env::set_var("BORG_KEK_PASSPHRASE", "kek_leak_must_not_appear");

            let command = format!("env > {}", out.display());
            let result = run_subprocess(&command, "{}", Duration::from_secs(10), true);

            std::env::remove_var("HOOK_PROBE_PROVIDER_API_KEY");
            std::env::remove_var("BORG_KEK_PASSPHRASE");

            let (code, _stdout, _stderr) = result.expect("hook must run");
            assert_eq!(code, 0);
            let captured = std::fs::read_to_string(&out).expect("hook should have written env");
            for leaked in [
                "leak_value_must_not_appear",
                "kek_leak_must_not_appear",
                "HOOK_PROBE_PROVIDER_API_KEY",
                "BORG_KEK_PASSPHRASE",
            ] {
                assert!(
                    !captured.contains(leaked),
                    "{leaked} reached the hook env:\n{captured}"
                );
            }
            // The filter must not leave the hook unable to run: `sh` needs
            // PATH to resolve `env` at all, so its absence would have failed
            // the spawn above, but assert it explicitly so a future narrowing
            // of the allowlist fails here with a clear reason.
            assert!(
                captured.lines().any(|l| l.starts_with("PATH=")),
                "hooks still need PATH:\n{captured}"
            );
        }

        /// The `$1` convention must keep working alongside stdin — borg's own
        /// documented hooks rely on it.
        #[test]
        fn payload_is_also_delivered_as_first_argument() {
            let dir = tmp();
            let out = dir.path().join("arg-payload.json");
            let command = format!("printf '%s' \"$1\" > {}", out.display());
            let (code, _stdout, _stderr) = run_subprocess(
                &command,
                r#"{"event":"SessionEnd"}"#,
                Duration::from_secs(10),
                true,
            )
            .expect("hook must run");
            assert_eq!(code, 0);
            let captured = std::fs::read_to_string(&out).expect("hook should have written $1");
            assert_eq!(captured, r#"{"event":"SessionEnd"}"#);
        }

        /// A hook that exits without consuming stdin makes our write fail with
        /// `BrokenPipe`. That is not the hook's failure: its exit status must be
        /// reported as-is rather than turned into an error (or, on `PreToolUse`,
        /// into a silent tool block).
        #[test]
        fn hook_that_ignores_stdin_still_reports_its_exit_code() {
            // Comfortably past the ~64 KiB pipe buffer so the write genuinely
            // blocks, but well under `ARG_MAX` — the payload also rides on argv,
            // and a megabyte there fails the spawn outright.
            let big_payload = format!(r#"{{"padding":"{}"}}"#, "x".repeat(128 * 1024));
            let (code, _stdout, _stderr) =
                run_subprocess("exit 0", &big_payload, Duration::from_secs(10), true)
                    .expect("broken pipe must not be reported as a subprocess error");
            assert_eq!(code, 0);

            let (code, _stdout, _stderr) =
                run_subprocess("exit 3", &big_payload, Duration::from_secs(10), true)
                    .expect("broken pipe must not mask a nonzero exit either");
            assert_eq!(code, 3);
        }

        /// A hook writing more than the ~64 KiB pipe buffer used to deadlock:
        /// stdout was only drained *after* `try_wait` reported exit, so the child
        /// blocked in `write`, never exited, and was SIGKILLed at the deadline and
        /// misreported as a timeout.
        #[test]
        fn hook_writing_more_than_the_pipe_buffer_does_not_time_out() {
            let (code, _stdout, _stderr) = run_subprocess(
                "printf 'x%.0s' $(seq 1 200000); exit 0",
                "{}",
                Duration::from_secs(20),
                true,
            )
            .expect("a chatty hook must complete, not time out");
            assert_eq!(code, 0, "chatty hook should report its real exit code");
        }

        /// A hook that backgrounds a process leaks the stdout/stderr write ends to
        /// a grandchild, so those pipes never reach EOF. Joining the drain threads
        /// unconditionally blocked `run_subprocess` for the grandchild's whole
        /// lifetime — far past `timeout`, on a runtime worker, with a `PreToolUse`
        /// tool call waiting on it.
        #[test]
        fn hook_that_backgrounds_a_process_returns_within_its_timeout() {
            let started = Instant::now();
            let (code, _stdout, _stderr) =
                run_subprocess("sleep 5 & exit 0", "{}", Duration::from_secs(1), true)
                    .expect("a hook that exits promptly must not be reported as failed");
            let elapsed = started.elapsed();
            assert_eq!(code, 0, "the hook's own exit status must be reported");
            assert!(
                elapsed < Duration::from_secs(3),
                "returned after {elapsed:?}; the leaked pipe held it for the \
                 background process's lifetime instead of honoring the timeout"
            );
        }
    }

    mod modify_directive {
        use super::*;

        fn tool_ctx(args: &str) -> HookContext {
            HookContext {
                point: HookPoint::BeforeToolCall,
                session_id: "s_abc".to_string(),
                turn_count: 1,
                data: HookData::ToolCall {
                    name: "run_shell".to_string(),
                    args: args.to_string(),
                },
            }
        }

        #[tokio::test(flavor = "current_thread")]
        async fn pre_tool_payload_carries_the_call_arguments() {
            // A hook that cannot see the arguments cannot judge the call. The
            // payload must carry them under both the canonical and the
            // Claude Code-compatible spelling.
            let hook = make_hook(
                r#"python3 -c "import json,sys; p=json.loads(sys.argv[1]); sys.exit(0 if p['tool']['args']['command']=='ls' and p['tool']['tool_input']['command']=='ls' else 3)" "$1""#,
                HookPoint::BeforeToolCall,
            );
            let action = hook.execute(&tool_ctx(r#"{"command":"ls"}"#)).await;
            assert!(
                matches!(action, HookAction::Continue),
                "hook could not read the arguments out of its payload: {action:?}"
            );
        }

        #[tokio::test(flavor = "current_thread")]
        async fn canonical_modify_shape_rewrites_the_arguments() {
            let hook = make_hook(
                r#"echo '{"action":"modify","args":{"command":"ls -la"}}'"#,
                HookPoint::BeforeToolCall,
            );
            match hook.execute(&tool_ctx(r#"{"command":"ls"}"#)).await {
                HookAction::Modify { args } => {
                    assert_eq!(args, r#"{"command":"ls -la"}"#);
                }
                other => panic!("expected a rewrite, got {other:?}"),
            }
        }

        #[tokio::test(flavor = "current_thread")]
        async fn claude_code_modify_shape_rewrites_the_arguments() {
            let hook = make_hook(
                r#"echo '{"decision":"modify","tool_input":{"command":"ls -la"}}'"#,
                HookPoint::BeforeToolCall,
            );
            match hook.execute(&tool_ctx(r#"{"command":"ls"}"#)).await {
                HookAction::Modify { args } => {
                    assert_eq!(args, r#"{"command":"ls -la"}"#);
                }
                other => panic!("expected a rewrite, got {other:?}"),
            }
        }

        #[tokio::test(flavor = "current_thread")]
        async fn ordinary_stdout_is_not_a_decision() {
            // Hooks that just print progress predate this feature and must
            // keep working untouched.
            for chatty in [
                "echo hello world",
                r#"echo '{"unrelated":true}'"#,
                r#"echo '{"action":"allow"}'"#,
                r#"echo 'not json {'"#,
            ] {
                let hook = make_hook(chatty, HookPoint::BeforeToolCall);
                let action = hook.execute(&tool_ctx(r#"{"command":"ls"}"#)).await;
                assert!(
                    matches!(action, HookAction::Continue),
                    "'{chatty}' must not be read as a decision, got {action:?}"
                );
            }
        }

        #[tokio::test(flavor = "current_thread")]
        async fn a_modify_without_args_runs_the_call_unchanged() {
            // Fail-safe: a malformed rewrite must never break the tool call
            // the hook was only inspecting.
            let hook = make_hook(r#"echo '{"action":"modify"}'"#, HookPoint::BeforeToolCall);
            let action = hook.execute(&tool_ctx(r#"{"command":"ls"}"#)).await;
            assert!(matches!(action, HookAction::Continue), "got {action:?}");
        }

        #[tokio::test(flavor = "current_thread")]
        async fn a_nonzero_exit_blocks_even_when_stdout_asks_to_modify() {
            // Block outranks rewrite: the call is not running, so its
            // arguments are moot.
            let hook = make_hook(
                r#"echo '{"action":"modify","args":{"command":"wipe"}}'; echo 'denied' >&2; exit 1"#,
                HookPoint::BeforeToolCall,
            );
            let action = hook.execute(&tool_ctx(r#"{"command":"ls"}"#)).await;
            assert!(
                matches!(action, HookAction::Skip { .. }),
                "a blocking hook must not be able to rewrite instead: {action:?}"
            );
        }

        #[tokio::test(flavor = "current_thread")]
        async fn a_non_tool_point_ignores_modify() {
            // There is no call to rewrite at a session/prompt point.
            let hook = make_hook(
                r#"echo '{"action":"modify","args":{"command":"ls -la"}}'"#,
                HookPoint::SessionStart,
            );
            let ctx = HookContext {
                point: HookPoint::SessionStart,
                session_id: "s_abc".to_string(),
                turn_count: 1,
                data: HookData::SessionStart {
                    session_id: "s_abc".to_string(),
                },
            };
            let action = hook.execute(&ctx).await;
            assert!(matches!(action, HookAction::Continue), "got {action:?}");
        }
    }
}
