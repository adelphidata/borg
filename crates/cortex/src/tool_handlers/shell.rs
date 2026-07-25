use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tracing::instrument;

use crate::agent::AgentEvent;
use crate::config::Config;
use crate::execution_context::ExecutionContext;
use crate::policy::ExecutionPolicy;

/// Bounded accumulator for a single output stream: retains the first half of
/// the byte budget from the head and the last half from the tail, eliding the
/// middle. A runaway command (`yes`, `cat` of a huge file, a chatty build) is
/// otherwise collected into an unbounded `String` that can exhaust daemon
/// memory and blow the model's context window. The elision is surfaced with a
/// `... N bytes omitted ...` marker (errors-never-swallowed) rather than
/// silently truncated. Live `ToolOutputDelta` streaming is unaffected — this
/// only bounds the buffer handed back to the model.
struct HeadTailBuffer {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    head_budget: usize,
    tail_budget: usize,
    /// Every byte ever pushed, so the omitted count is exact.
    total: usize,
}

impl HeadTailBuffer {
    /// `cap` is the total retained byte budget for this stream. `0` is treated
    /// as `1` (never unbounded); callers resolve the configured default first.
    fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        let head_budget = cap / 2;
        let tail_budget = cap - head_budget; // always >= 1
        Self {
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            head_budget,
            tail_budget,
            total: 0,
        }
    }

    /// Convenience wrapper used by the unit tests, which express fixtures as
    /// string literals. The streaming paths push raw bytes via `push_bytes`.
    #[cfg(test)]
    fn push_str(&mut self, s: &str) {
        self.push_bytes(s.as_bytes());
    }

    /// Feed raw bytes. Callers on the streaming paths push fixed-size chunks
    /// straight from the read buffer (never a whole unbounded line), so a flood
    /// with no newlines can't accumulate before the cap engages.
    fn push_bytes(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        self.total += rest.len();
        // Fill the head first.
        if self.head.len() < self.head_budget {
            let take = (self.head_budget - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        if rest.is_empty() {
            return;
        }
        // Remainder feeds the tail ring, keeping only its most recent bytes.
        if rest.len() >= self.tail_budget {
            self.tail.clear();
            self.tail.extend(&rest[rest.len() - self.tail_budget..]);
        } else {
            while self.tail.len() + rest.len() > self.tail_budget {
                self.tail.pop_front();
            }
            self.tail.extend(rest);
        }
    }

    /// Number of bytes dropped from the middle (0 when nothing was elided).
    fn omitted(&self) -> usize {
        self.total - self.head.len() - self.tail.len()
    }

    /// Render the retained output. When nothing was elided this is the exact
    /// stream; otherwise it is `head` + marker + `tail`. Byte slices are decoded
    /// lossily — a cap boundary can land mid-UTF-8, and lossy decoding of
    /// already-truncated diagnostic output is the pragmatic choice.
    fn into_output(self) -> String {
        let omitted = self.omitted();
        if omitted == 0 {
            let mut all = self.head;
            all.extend(self.tail);
            return String::from_utf8_lossy(&all).into_owned();
        }
        let tail: Vec<u8> = self.tail.into_iter().collect();
        let mut out = String::from_utf8_lossy(&self.head).into_owned();
        out.push_str(&format!("\n... {omitted} bytes omitted ...\n"));
        out.push_str(&String::from_utf8_lossy(&tail));
        out
    }
}

/// Resolve the per-stream `run_shell` output cap: the configured value, or the
/// 1 MiB default when unset (`0`). Never unbounded — mirrors
/// `mcp.max_response_bytes`.
fn resolved_shell_output_cap(config: &Config) -> usize {
    let configured = config.security.max_shell_output_bytes;
    if configured == 0 {
        let default = crate::config::security::default_max_shell_output_bytes();
        // Surface the misconfiguration rather than silently ignoring the
        // operator's value — mirrors `mcp_clients::resolve_max_response_bytes`,
        // whose 0-fallback also warns.
        tracing::warn!(
            "security.max_shell_output_bytes=0 is invalid (would disable the cap); \
             falling back to default {default}",
        );
        default
    } else {
        configured
    }
}

/// Read stdout/stderr from a child process line-by-line, emitting `ToolOutputDelta` events.
async fn stream_child_output(
    child: &mut tokio::process::Child,
    timeout: Duration,
    event_tx: &mpsc::Sender<AgentEvent>,
    tool_call_id: &str,
    tool_name: &str,
    max_output_bytes: usize,
) -> Result<(String, String, Option<i32>)> {
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    // Each stream is bounded independently so a flood on one can't crowd out the
    // other, and neither can grow without limit.
    let mut stdout_buf = HeadTailBuffer::new(max_output_bytes);
    let mut stderr_buf = HeadTailBuffer::new(max_output_bytes);

    let streaming_future = async {
        let mut stdout_pipe = stdout_pipe;
        let mut stderr_pipe = stderr_pipe;
        let mut stdout_done = stdout_pipe.is_none();
        let mut stderr_done = stderr_pipe.is_none();
        // Fixed-size reads (not `lines()`) so a single flood with few/no
        // newlines can't accumulate an unbounded line in memory before the
        // HeadTailBuffer cap ever sees it — that would defeat the whole cap.
        // The PTY path reads in chunks for the same reason.
        let mut stdout_chunk = [0u8; 8192];
        let mut stderr_chunk = [0u8; 8192];

        while !stdout_done || !stderr_done {
            tokio::select! {
                res = async {
                    match stdout_pipe.as_mut() {
                        Some(p) => p.read(&mut stdout_chunk).await,
                        None => std::future::pending().await,
                    }
                }, if !stdout_done => {
                    match res {
                        Ok(0) => stdout_done = true,
                        Ok(n) => {
                            // Best-effort streaming delta: a full/closed channel just means no
                            // live listener — the final buffered output is still returned below.
                            let _ = event_tx.try_send(AgentEvent::ToolOutputDelta {
                                tool_call_id: tool_call_id.to_string(),
                                name: tool_name.to_string(),
                                delta: String::from_utf8_lossy(&stdout_chunk[..n]).into_owned(),
                                is_stderr: false,
                            });
                            stdout_buf.push_bytes(&stdout_chunk[..n]);
                        }
                        Err(e) => {
                            // A pipe read error is not clean EOF — surface it
                            // rather than presenting partial output as complete
                            // (no-silent-swallow), then stop this stream.
                            tracing::warn!(error = %e, "run_shell stdout read failed");
                            stdout_done = true;
                        }
                    }
                }
                res = async {
                    match stderr_pipe.as_mut() {
                        Some(p) => p.read(&mut stderr_chunk).await,
                        None => std::future::pending().await,
                    }
                }, if !stderr_done => {
                    match res {
                        Ok(0) => stderr_done = true,
                        Ok(n) => {
                            let _ = event_tx.try_send(AgentEvent::ToolOutputDelta {
                                tool_call_id: tool_call_id.to_string(),
                                name: tool_name.to_string(),
                                delta: String::from_utf8_lossy(&stderr_chunk[..n]).into_owned(),
                                is_stderr: true,
                            });
                            stderr_buf.push_bytes(&stderr_chunk[..n]);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "run_shell stderr read failed");
                            stderr_done = true;
                        }
                    }
                }
            }
        }

        child.wait().await
    };

    let status = tokio::time::timeout(timeout, streaming_future)
        .await
        .map_err(|_| anyhow::anyhow!("command timed out after {}ms", timeout.as_millis()))?
        .context("Failed to wait for shell process")?;

    Ok((
        stdout_buf.into_output(),
        stderr_buf.into_output(),
        status.code(),
    ))
}

#[instrument(skip_all, fields(tool.name = "run_shell"))]
pub async fn handle_run_shell(
    args: &serde_json::Value,
    config: &Config,
    policy: &ExecutionPolicy,
    event_tx: &mpsc::Sender<AgentEvent>,
    tool_call_id: &str,
    skill_env_allowlist: Option<&std::collections::HashSet<String>>,
) -> Result<String> {
    let ctx = ExecutionContext::from_process()?;
    handle_run_shell_with_context(
        args,
        config,
        policy,
        event_tx,
        tool_call_id,
        skill_env_allowlist,
        &ctx,
    )
    .await
}

pub async fn handle_run_shell_with_context(
    args: &serde_json::Value,
    config: &Config,
    policy: &ExecutionPolicy,
    event_tx: &mpsc::Sender<AgentEvent>,
    tool_call_id: &str,
    skill_env_allowlist: Option<&std::collections::HashSet<String>>,
    ctx: &ExecutionContext,
) -> Result<String> {
    let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("run");

    // Session-control actions drive an already-running persistent session via
    // the process-global registry. They carry no command and add no approval
    // prompt — the command was policy-checked when the session was started.
    match action {
        "run" | "start" => {}
        "write_stdin" | "poll" | "kill" => return handle_session_action(action, args).await,
        other => {
            return Ok(format!(
                "Unknown run_shell action '{other}'. Valid: run, start, write_stdin, poll, kill."
            ))
        }
    }

    let command = args["command"].as_str().context("Missing 'command'")?;
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(config.tools.default_timeout_ms);
    let timeout_dur = Duration::from_millis(timeout_ms);
    let workdir = args
        .get("workdir")
        .and_then(serde_json::Value::as_str)
        .map(|raw| ctx.resolve_existing_path(raw))
        .transpose()?;
    let want_tty = args
        .get("tty")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    // If the model wrapped an `apply_patch` call inside a shell heredoc
    // (`bash -lc "apply_patch <<'EOF' ... EOF"`), reroute through the
    // verified patch path. The shell binary `apply_patch` doesn't exist on
    // `$PATH`, so running it raw fails; we also lose blocked-path
    // enforcement, vitals classification, and ghost-commit undo.
    if action == "run" {
        if let Some(patch_text) = coding::maybe_extract_apply_patch_from_shell(command) {
            let patch_args = serde_json::json!({ "patch": patch_text });
            return crate::tool_handlers::filesystem::handle_apply_patch(&patch_args, config);
        }
    }

    // Paired shell-side guard for the file-write control-plane deny: refuse
    // `run`/`start` commands that write Borg's own control-plane files or flip
    // a security setting via the CLI. Without this, `echo > ~/.borg/hooks.json`
    // or `borg settings set …` would bypass the `fs_write` boundary entirely.
    if crate::tool_handlers::filesystem::shell_command_targets_control_plane(command) {
        return Ok(deny_control_plane_write(command).await);
    }

    match policy.check(command) {
        crate::policy::PolicyDecision::Deny => {
            // Audit the denial so post-hoc review can spot prompt-injection
            // attempts hitting the catastrophic-command list. Free function
            // no-ops outside the daemon (CLI/REPL/tests), avoiding contention
            // on the SQLite writer mutex.
            crate::db::try_emit_global(
                crate::db::audit_events::SANDBOX_DENIAL,
                "system",
                &serde_json::json!({
                    "tool": "run_shell",
                    "command_redacted": redact_command_for_audit(command),
                }),
            )
            .await;
            return Ok("Shell command denied by policy.".to_string());
        }
        crate::policy::PolicyDecision::AutoApprove => {}
    }

    // Resolve credentials and filter to only those declared by skills
    let all_creds = config.resolve_credentials();
    let mut filtered_creds: std::collections::HashMap<String, String> = match skill_env_allowlist {
        Some(allowlist) => all_creds
            .into_iter()
            .filter(|(k, _)| allowlist.contains(k))
            .collect(),
        None => all_creds,
    };
    // Merge per-skill env vars (these are explicitly configured, always included)
    let skill_env = crate::skills::collect_skill_env(&config.skills);
    for (k, v) in skill_env {
        filtered_creds.entry(k).or_insert(v);
    }
    // Resolve the shell to an absolute path so workspace-bounded sandbox
    // profiles can validate it. Bare `sh` is rejected by Seatbelt
    // profile generation (paths must be absolute).
    #[cfg(unix)]
    let (shell, shell_flag) = ("/bin/sh", "-c");
    #[cfg(windows)]
    let (shell, shell_flag) = ("cmd.exe", "/C");

    if action == "start" {
        return start_session(
            ctx,
            config,
            &filtered_creds,
            command,
            workdir.as_deref(),
            shell,
            shell_flag,
        )
        .await;
    }

    if want_tty {
        if ctx.enforce_workspace_root() {
            // PTY-wrapped shells go through `portable_pty::CommandBuilder`,
            // which does not interoperate with our `sandbox-exec`/`bwrap`
            // wrapper. Refusing TTY in workspace-bounded sessions is the
            // safer default — IDE clients rarely need a TTY for tooling
            // anyway, and the failure is loud rather than silently
            // unsandboxed. Drop this guard once the sandbox crate grows a
            // pty-aware spawn path.
            return Ok(
                "tty=true is not supported in ACP sessions because the workspace sandbox \
                 cannot be applied to a PTY-wrapped shell. Re-run with tty=false or run \
                 the command outside an ACP session."
                    .to_string(),
            );
        }
        return run_in_pty(
            shell,
            shell_flag,
            command,
            workdir.as_deref().or(Some(ctx.workspace_cwd())),
            &filtered_creds,
            timeout_dur,
            timeout_ms,
            event_tx,
            tool_call_id,
            ctx,
            resolved_shell_output_cap(config),
        )
        .await;
    }

    let cwd_path = workdir
        .as_deref()
        .unwrap_or(ctx.workspace_cwd())
        .to_path_buf();
    let (program, full_args) =
        wrap_run_shell_for_sandbox(ctx, config, shell, shell_flag, command, &cwd_path);
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&full_args)
        // One-shot commands are non-interactive: nothing feeds this stdin, so
        // inheriting the parent's would let the command consume the operator's
        // keystrokes (or block forever on `cat`, `ssh`, an unexpected prompt)
        // until the timeout fires. The persistent-session path below pipes
        // stdin instead, because `write_stdin` needs a handle to write to.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd.current_dir(&cwd_path);
    // Strip the parent env down to the allowlist before re-injecting
    // skill-declared credentials. Without this, the daemon's loaded
    // provider keys (OPENROUTER_API_KEY, ANTHROPIC_API_KEY, ...) would
    // be visible to any `env` / `printenv` in the agent's shell.
    crate::security::env_allowlist::apply_filtered_env(&mut cmd, &filtered_creds);

    let mut child = cmd
        .kill_on_drop(true)
        .spawn()
        .context("Failed to spawn shell command")?;

    // Register with the process registry so `borg status` / `/doctor`
    // can see the child mid-flight. The guard's Drop marks the entry
    // Killed if `?` early-returns below, so an error path doesn't
    // leave a phantom Running entry.
    let _guard = ctx.process_registry().and_then(|reg| {
        child.id().map(|pid| {
            reg.register(
                pid,
                crate::process_registry::ProcessKind::RunShell,
                command,
                ctx.session_id().map(str::to_string),
            )
        })
    });

    match stream_child_output(
        &mut child,
        timeout_dur,
        event_tx,
        tool_call_id,
        "run_shell",
        resolved_shell_output_cap(config),
    )
    .await
    {
        Ok((stdout, stderr, code)) => {
            if let Some(g) = _guard {
                g.complete(code);
            }
            let status = code.unwrap_or(-1);
            Ok(format!(
                "Exit code: {status}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
            ))
        }
        Err(e) => {
            if e.to_string().contains("timed out") {
                if let Some(g) = _guard {
                    g.timed_out();
                }
                Ok(format!(
                    "Error: command timed out after {timeout_ms}ms\nCommand: {command}"
                ))
            } else {
                // Guard's Drop will mark Killed.
                Err(e)
            }
        }
    }
}

/// Message returned when a persistent-session action is requested but no
/// session registry is installed (CLI/REPL/test — no running daemon).
fn no_session_registry_msg() -> String {
    "Persistent shell sessions require the borg daemon. Use action=run (the default) \
     to execute a single self-contained command instead."
        .to_string()
}

/// Launch a persistent interactive session: spawn the shell with piped
/// stdin/stdout/stderr, register it with the subprocess registry (for
/// `borg status` visibility + auto-reap) and the session registry, and return
/// the `session_id` the model drives with write_stdin/poll/kill.
#[allow(clippy::too_many_arguments)]
async fn start_session(
    ctx: &ExecutionContext,
    config: &Config,
    filtered_creds: &std::collections::HashMap<String, String>,
    command: &str,
    workdir: Option<&std::path::Path>,
    shell: &str,
    shell_flag: &str,
) -> Result<String> {
    let Some(sessions) = crate::shell_session::global() else {
        return Ok(no_session_registry_msg());
    };

    let cwd_path = workdir.unwrap_or(ctx.workspace_cwd()).to_path_buf();
    let (program, full_args) =
        wrap_run_shell_for_sandbox(ctx, config, shell, shell_flag, command, &cwd_path);
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&full_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd.current_dir(&cwd_path);
    crate::security::env_allowlist::apply_filtered_env(&mut cmd, filtered_creds);
    // Lead a new process group so `kill` can signal the whole tree (the shell
    // plus anything it spawned), not just the direct child — otherwise an
    // interrupted session can leak descendants and their sandbox bookkeeping.
    #[cfg(unix)]
    cmd.process_group(0);

    let child = cmd
        .kill_on_drop(true)
        .spawn()
        .context("Failed to spawn shell session")?;

    // Mirror the one-shot path: register the PID so the session is visible to
    // `borg status` / `/doctor` and reachable by the self-healing sweep. The
    // guard is moved into the session and finalized on `kill`.
    let guard = ctx.process_registry().and_then(|reg| {
        child.id().map(|pid| {
            reg.register(
                pid,
                crate::process_registry::ProcessKind::RunShell,
                command,
                ctx.session_id().map(str::to_string),
            )
        })
    });

    let id = sessions.register(child, guard, resolved_shell_output_cap(config));
    Ok(format!(
        "Started shell session {id}. Drive it with run_shell: action=write_stdin \
         (session_id={id}, stdin=…) to send input, action=poll (session_id={id}) to read new \
         output, action=kill (session_id={id}) to stop."
    ))
}

/// Handle the session-control actions (`write_stdin` / `poll` / `kill`) against
/// the process-global session registry. Returns an explanatory message (as a
/// normal tool result, not an error) when the session is unknown or no
/// registry is installed, so the agent loop treats it as recoverable.
async fn handle_session_action(action: &str, args: &serde_json::Value) -> Result<String> {
    // `write_stdin` injects fresh commands into a live shell that were never
    // seen by the start-time policy/control-plane checks, so re-guard the stdin
    // here — before the registry lookup, so the deny holds regardless of
    // whether a session registry is installed. Otherwise it becomes the bypass
    // for the file-write control-plane boundary.
    if action == "write_stdin" {
        let stdin = args.get("stdin").and_then(|v| v.as_str()).unwrap_or("");
        if crate::tool_handlers::filesystem::shell_command_targets_control_plane(stdin) {
            return Ok(deny_control_plane_write(stdin).await);
        }
    }

    let Some(sessions) = crate::shell_session::global() else {
        return Ok(no_session_registry_msg());
    };
    let session_id = args
        .get("session_id")
        .and_then(|v| v.as_str())
        .context("Missing 'session_id' for a session action")?;

    match action {
        "write_stdin" => {
            let stdin = args.get("stdin").and_then(|v| v.as_str()).unwrap_or("");
            match sessions.write_stdin(session_id, stdin).await {
                Ok(()) => Ok(format!(
                    "Wrote {} bytes to session {session_id}. Use action=poll to read output.",
                    stdin.len()
                )),
                Err(e) => Ok(e.to_string()),
            }
        }
        "poll" => match sessions.poll(session_id) {
            Ok(out) if out.is_empty() => Ok(format!("[no new output from session {session_id}]")),
            Ok(out) => Ok(out),
            Err(e) => Ok(e.to_string()),
        },
        "kill" => match sessions.kill(session_id).await {
            Ok(()) => Ok(format!("Killed session {session_id}.")),
            Err(e) => Ok(e.to_string()),
        },
        _ => unreachable!("handle_session_action only dispatched for session verbs"),
    }
}

/// Decide whether to wrap `run_shell` invocations in a workspace-scoped
/// sandbox profile. ACP sessions (`ctx.enforce_workspace_root() == true`)
/// MUST be sandboxed — that's the kernel-level boundary the plan
/// (§"Tool-Specific Behavior — `run_shell`") requires so a `cd /tmp;
/// echo > /etc/passwd` from an IDE-launched agent can't escape the
/// declared workspace. Non-ACP callers retain the existing direct-spawn
/// behavior to avoid changing semantics for the TUI/REPL.
fn wrap_run_shell_for_sandbox(
    ctx: &ExecutionContext,
    config: &Config,
    shell: &str,
    shell_flag: &str,
    command: &str,
    cwd: &std::path::Path,
) -> (String, Vec<String>) {
    let bare_args = vec![shell_flag.to_string(), command.to_string()];
    if !ctx.enforce_workspace_root() {
        return (shell.to_string(), bare_args);
    }
    let workspace_root_str = ctx.workspace_root().to_string_lossy().into_owned();
    let policy = sandbox::policy::SandboxPolicy {
        // ACP shell turns frequently invoke build/test/curl flows. Network
        // is permitted; strict boundary enforcement is filesystem-scoped.
        network: true,
        fs_read: vec![workspace_root_str.clone()],
        fs_write: vec![workspace_root_str.clone()],
        deny_read: Vec::new(),
        deny_write: Vec::new(),
        allowed_domains: Vec::new(),
    }
    // Enforce configured blocked_paths as sandbox deny rules so a shell command
    // can't read e.g. `~/.env` or `<workspace>/credentials` — the allow-list
    // scoping alone permits broad reads, and the hardcoded sensitive-home denies
    // don't cover operator-configured entries. Enforced on all three backends:
    // Seatbelt `(deny file-read*)`, the Windows backend, and bwrap via
    // `hide_path` (tmpfs over directories, /dev/null over files).
    .with_blocked_paths_denied(
        &config.security.blocked_paths,
        std::slice::from_ref(&workspace_root_str),
    )
    .configured(&config.security.blocked_paths);
    let wrapped = policy.wrap_command(shell, &bare_args, cwd);
    (wrapped.program, wrapped.args)
}

/// Run `command` under a pseudo-terminal so it sees stdout as a TTY. Used for
/// REPLs and tools whose output (or willingness to start) depends on `isatty`.
/// Output is read off the master end into a single combined buffer; stdout
/// and stderr are merged because that's what a TTY does on the wire — there
/// is no separate stderr file descriptor inside a PTY.
#[allow(clippy::too_many_arguments)]
async fn run_in_pty(
    shell: &str,
    shell_flag: &str,
    command: &str,
    workdir: Option<&std::path::Path>,
    env: &std::collections::HashMap<String, String>,
    timeout: Duration,
    timeout_ms: u64,
    event_tx: &mpsc::Sender<AgentEvent>,
    tool_call_id: &str,
    ctx: &ExecutionContext,
    max_output_bytes: usize,
) -> Result<String> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("Failed to open pseudo-terminal")?;

    let mut builder = CommandBuilder::new(shell);
    builder.arg(shell_flag);
    builder.arg(command);
    if let Some(dir) = workdir {
        builder.cwd(dir);
    }
    // Mirror the non-PTY spawn site: env_clear + allowlist-filtered parent
    // env, with skill-declared creds layered on top.
    builder.env_clear();
    for (key, val) in crate::security::env_allowlist::build_filtered_env(env) {
        builder.env(key, val);
    }

    let mut child = pair
        .slave
        .spawn_command(builder)
        .context("Failed to spawn PTY child")?;
    // Closing the slave on the parent side is necessary so EOF propagates to
    // the reader once the child exits — otherwise the read loop blocks
    // forever waiting on a dangling fd.
    drop(pair.slave);

    // Mirror the non-PTY spawn site: register the PID so PTY-mode REPLs
    // are visible to `borg status` / `/doctor` and reaped by the
    // self-healing sweep. Drop semantics apply: a panic or unwrapped
    // error below marks the entry Killed.
    let _guard = ctx.process_registry().and_then(|reg| {
        child.process_id().map(|pid| {
            reg.register(
                pid,
                crate::process_registry::ProcessKind::RunShell,
                command,
                ctx.session_id().map(str::to_string),
            )
        })
    });

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("Failed to clone PTY reader")?;
    let event_tx_clone = event_tx.clone();
    let tool_call_id = tool_call_id.to_string();
    let reader_task = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        // A PTY merges stdout+stderr onto one fd, so the whole session shares a
        // single bounded buffer.
        let mut combined = HeadTailBuffer::new(max_output_bytes);
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    // Best-effort streaming delta; dropped sends don't affect `combined` below.
                    let _ = event_tx_clone.try_send(AgentEvent::ToolOutputDelta {
                        tool_call_id: tool_call_id.clone(),
                        name: "run_shell".to_string(),
                        delta: String::from_utf8_lossy(&buf[..n]).into_owned(),
                        is_stderr: false,
                    });
                    combined.push_bytes(&buf[..n]);
                }
                // A PTY master read after the slave closes surfaces as an error
                // (EIO on Linux), which is the *normal* end-of-session signal
                // here — not a truncation. Treat it as EOF like `Ok(0)`.
                Err(_) => break,
            }
        }
        combined.into_output()
    });

    let wait_task = tokio::task::spawn_blocking(move || child.wait());

    let combined_result = tokio::time::timeout(timeout, async {
        let combined = reader_task.await.context("PTY reader task panicked")?;
        let status = wait_task
            .await
            .context("PTY wait task panicked")?
            .context("Failed to wait for PTY child")?;
        Ok::<_, anyhow::Error>((combined, status.exit_code()))
    })
    .await;

    match combined_result {
        Ok(Ok((combined, exit_code))) => {
            if let Some(g) = _guard {
                g.complete(Some(exit_code as i32));
            }
            Ok(format!(
                "Exit code: {exit_code}\n--- output (tty) ---\n{combined}"
            ))
        }
        Ok(Err(e)) => Err(e),
        Err(_) => {
            if let Some(g) = _guard {
                g.timed_out();
            }
            Ok(format!(
                "Error: command timed out after {timeout_ms}ms (tty)\nCommand: {command}"
            ))
        }
    }
}

/// Audit and produce the user-facing message for a command refused by the
/// shell-side control-plane guard. Shared by the one-shot `run`/`start` path
/// and the persistent-session `write_stdin` path so both emit the same audit
/// event and explanation.
async fn deny_control_plane_write(command: &str) -> String {
    crate::db::try_emit_global(
        crate::db::audit_events::SANDBOX_DENIAL,
        "system",
        &serde_json::json!({
            "tool": "run_shell",
            "reason": "control_plane_write",
            "command_redacted": redact_command_for_audit(command),
        }),
    )
    .await;
    "Shell command denied: writing Borg control-plane files (config DB, secrets, hooks.json) \
     or changing settings via `borg settings set` is not permitted."
        .to_string()
}

/// Bound the size of the command string we put in the audit chain. Keeps the
/// hash-chained payload from ballooning if the model emits a multi-kilobyte
/// shell command, and replaces control bytes so the JSON stays well-formed.
///
/// `MAX_CHARS` is a *char* count, not a byte count. Worst-case byte size of the
/// truncated body is `MAX_CHARS × 4` (pure 4-byte UTF-8) plus the `…<truncated>`
/// suffix — well within JSON-payload sanity.
fn redact_command_for_audit(command: &str) -> String {
    const MAX_CHARS: usize = 256;
    let cleaned: String = command
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.chars().count() <= MAX_CHARS {
        cleaned
    } else {
        let mut s = cleaned.chars().take(MAX_CHARS).collect::<String>();
        s.push_str("…<truncated>");
        s
    }
}

#[cfg(test)]
mod tests {
    use super::{
        handle_run_shell, redact_command_for_audit, wrap_run_shell_for_sandbox, HeadTailBuffer,
    };
    use crate::agent::AgentEvent;
    use crate::config::Config;
    use crate::execution_context::ExecutionContext;
    use crate::policy::{ExecutionPolicy, PolicyDecision};
    use tokio::sync::mpsc;

    /// A sub-agent derived from a workspace-confined parent must still be
    /// sandbox-wrapped. Regression guard: children are granted
    /// `auto_approve: ["*"]`, so if `for_child` ever stops carrying
    /// `enforce_workspace_root` the child runs arbitrary commands unsandboxed.
    #[test]
    fn child_context_of_confined_parent_still_wraps_shell_in_sandbox() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("mkdir workspace");
        let parent =
            ExecutionContext::for_workspace(root.clone(), root.clone()).expect("parent context");
        let config = Config::default();

        let (parent_program, _) =
            wrap_run_shell_for_sandbox(&parent, &config, "/bin/sh", "-c", "echo hi", &root);
        let child = parent.for_child();
        let (child_program, child_args) =
            wrap_run_shell_for_sandbox(&child, &config, "/bin/sh", "-c", "echo hi", &root);

        assert!(
            child.enforce_workspace_root(),
            "child must inherit workspace enforcement"
        );
        assert_ne!(
            child_program, "/bin/sh",
            "child command must be wrapped by the sandbox, not executed bare"
        );
        assert_eq!(
            child_program, parent_program,
            "child must use the same sandbox program as its parent"
        );
        assert!(
            child_args.iter().any(|arg| arg.contains("echo hi")),
            "wrapped argv must still carry the original command: {child_args:?}"
        );
    }

    /// The unconfined case must stay unwrapped — otherwise every CLI shell call
    /// would start paying for `sandbox-exec`/`bwrap`.
    #[test]
    fn child_context_of_unconfined_parent_stays_bare() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = ExecutionContext::unrestricted(tmp.path().to_path_buf());
        let config = Config::default();
        let (program, args) = wrap_run_shell_for_sandbox(
            &parent.for_child(),
            &config,
            "/bin/sh",
            "-c",
            "echo hi",
            tmp.path(),
        );
        assert_eq!(program, "/bin/sh");
        assert_eq!(args, vec!["-c".to_string(), "echo hi".to_string()]);
    }

    /// A child must not keep the parent's IDE text-file bridge: children run in
    /// detached tasks that outlive the ACP request the bridge belongs to.
    #[test]
    fn child_context_drops_the_text_file_bridge_but_keeps_containment() {
        struct StubBridge;
        impl crate::execution_context::TextFileBridge for StubBridge {
            fn supports_read_text_file(&self) -> bool {
                true
            }
            fn supports_write_text_file(&self) -> bool {
                true
            }
            fn read_text_file(&self, _path: &std::path::Path) -> anyhow::Result<String> {
                Ok(String::new())
            }
            fn write_text_file(
                &self,
                _path: &std::path::Path,
                _content: &str,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("mkdir workspace");
        let parent = ExecutionContext::for_workspace(root.clone(), root.clone())
            .expect("parent context")
            .with_text_file_bridge(Some(std::sync::Arc::new(StubBridge)));
        assert!(parent.text_file_bridge().is_some());

        let child = parent.for_child();
        assert!(child.text_file_bridge().is_none());
        assert!(child.enforce_workspace_root());
        assert_eq!(child.workspace_root(), parent.workspace_root());
        child
            .resolve_path_for_write("/etc/passwd")
            .expect_err("child must still reject paths outside the workspace root");
    }

    #[test]
    fn head_tail_buffer_under_cap_is_verbatim() {
        // Output within the budget must pass through byte-for-byte with no
        // marker — the cap is invisible for well-behaved commands.
        let mut buf = HeadTailBuffer::new(64);
        buf.push_str("hello ");
        buf.push_str("world\n");
        assert_eq!(buf.omitted(), 0);
        assert_eq!(buf.into_output(), "hello world\n");
    }

    #[test]
    fn head_tail_buffer_at_exact_cap_is_verbatim() {
        // Boundary: total == cap must not trigger elision.
        let mut buf = HeadTailBuffer::new(10);
        buf.push_str("0123456789"); // exactly 10 bytes
        assert_eq!(buf.omitted(), 0);
        assert_eq!(buf.into_output(), "0123456789");
    }

    #[test]
    fn head_tail_buffer_over_cap_keeps_head_and_tail_with_marker() {
        // cap 10 → 5-byte head + 5-byte tail. 20 bytes in ⇒ 10 omitted, and the
        // marker must report the exact dropped count while head+tail survive.
        let mut buf = HeadTailBuffer::new(10);
        buf.push_str("AAAAA"); // head fills: AAAAA
        buf.push_str("MMMMMMMMMM"); // middle, elided
        buf.push_str("ZZZZZ"); // tail keeps the last 5: ZZZZZ
        assert_eq!(buf.omitted(), 10);
        assert_eq!(buf.into_output(), "AAAAA\n... 10 bytes omitted ...\nZZZZZ");
    }

    #[test]
    fn head_tail_buffer_tail_tracks_most_recent_bytes() {
        // The tail is a ring: only the newest bytes are retained as more arrive.
        let mut buf = HeadTailBuffer::new(6); // 3 head + 3 tail
        for _ in 0..100 {
            buf.push_str("abc");
        }
        let out = buf.into_output();
        assert!(out.starts_with("abc\n... "), "head preserved: {out:?}");
        assert!(
            out.ends_with("\nabc"),
            "tail is the most recent bytes: {out:?}"
        );
        assert_eq!(buf_omitted_from(&out), 300 - 6);
    }

    #[test]
    fn head_tail_buffer_zero_cap_falls_back_not_unbounded() {
        // cap 0 must behave as a tiny bound (never unbounded) — resolution to
        // the real default happens in `resolved_shell_output_cap`, but the
        // buffer itself must never retain everything.
        let mut buf = HeadTailBuffer::new(0);
        buf.push_str(&"x".repeat(1000));
        assert!(buf.omitted() >= 999);
    }

    /// Parse the `... N bytes omitted ...` marker back out for assertions.
    fn buf_omitted_from(rendered: &str) -> usize {
        let marker = rendered
            .lines()
            .find(|l| l.contains("bytes omitted"))
            .expect("marker present");
        marker
            .trim_start_matches("... ")
            .trim_end_matches(" bytes omitted ...")
            .parse()
            .expect("numeric omitted count")
    }

    #[test]
    fn redact_truncates_long_commands_and_marks_them() {
        // Real failure mode: a 5KB injected command would otherwise bloat
        // every row in the hash-chained audit log.
        let long = "echo ".to_string() + &"a".repeat(1000);
        let redacted = redact_command_for_audit(&long);
        assert!(redacted.len() < long.len());
        assert!(redacted.ends_with("<truncated>"));
    }

    #[test]
    fn redact_replaces_control_chars_so_json_stays_well_formed() {
        let with_ctrl = "echo hi\nrm -rf /\0";
        let redacted = redact_command_for_audit(with_ctrl);
        assert!(!redacted.contains('\n'));
        assert!(!redacted.contains('\0'));
        assert!(redacted.contains("rm -rf /"));
    }

    #[test]
    fn shell_command_constants_match_platform() {
        #[cfg(unix)]
        {
            let (shell, flag) = ("sh", "-c");
            assert_eq!(shell, "sh");
            assert_eq!(flag, "-c");
        }
        #[cfg(windows)]
        {
            let (shell, flag) = ("cmd.exe", "/C");
            assert_eq!(shell, "cmd.exe");
            assert_eq!(flag, "/C");
        }
    }

    #[test]
    fn policy_never_requires_shell_confirmation() {
        let policy = ExecutionPolicy {
            auto_approve: vec!["echo *".to_string()],
            deny: vec!["echo blocked".to_string()],
        };
        assert_eq!(policy.check("echo ok"), PolicyDecision::AutoApprove);
        assert_eq!(policy.check("echo blocked"), PolicyDecision::Deny);
        assert_eq!(
            policy.check("printf 'still allowed\\n'"),
            PolicyDecision::AutoApprove
        );
    }

    #[tokio::test]
    async fn allowed_shell_command_does_not_emit_shell_confirmation() {
        let config = Config::default();
        let policy = ExecutionPolicy::default();
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let args = serde_json::json!({ "command": "printf 'ok\\n'" });

        let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-1", None)
            .await
            .expect("shell command should run");
        assert!(result.contains("Exit code: 0"), "got: {result}");

        while let Ok(event) = event_rx.try_recv() {
            assert!(
                !matches!(event, AgentEvent::ShellConfirmation { .. }),
                "run_shell must not emit shell confirmations"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_shell_output_is_bounded_by_cap() {
        // A command that floods stdout must come back head/tail-capped with an
        // omission marker, not as an unbounded buffer — otherwise a runaway
        // command blows the model's context window.
        let mut config = Config::default();
        config.security.max_shell_output_bytes = 256;
        let policy = ExecutionPolicy::default();
        let (event_tx, _event_rx) = mpsc::channel(1024);
        // ~9 KiB of stdout, well over the 256-byte cap.
        let args = serde_json::json!({
            "command": "for i in $(seq 1 1000); do echo AAAAAAAA; done",
        });

        let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-cap", None)
            .await
            .expect("shell command should run");

        assert!(result.contains("Exit code: 0"), "got: {result}");
        assert!(
            result.contains("bytes omitted"),
            "expected elision marker, got: {result}"
        );
        // The retained stdout is bounded near the cap (plus framing + marker),
        // far below the ~9 KiB the command actually produced.
        assert!(
            result.len() < 1024,
            "output not bounded: {} bytes",
            result.len()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_shell_bounds_output_with_no_newlines() {
        // The cap must engage even when the flood has no newlines. Line-based
        // reading would buffer the whole single line unbounded before the cap
        // saw a byte; chunked reads keep memory bounded regardless of framing.
        let mut config = Config::default();
        config.security.max_shell_output_bytes = 256;
        let policy = ExecutionPolicy::default();
        let (event_tx, _event_rx) = mpsc::channel(1024);
        // ~512 KiB on a single line (no `\n`) — the pathological case for a
        // line reader.
        let args = serde_json::json!({
            "command": "head -c 524288 /dev/zero | tr '\\0' A",
        });

        let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-noln", None)
            .await
            .expect("shell command should run");

        assert!(result.contains("Exit code: 0"), "got exit: {result:.120}");
        assert!(
            result.contains("bytes omitted"),
            "no-newline flood must still be elided"
        );
        assert!(
            result.len() < 1024,
            "no-newline output not bounded: {} bytes",
            result.len()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_shell_small_output_has_no_marker() {
        // The cap must be invisible for well-behaved commands: no marker, no
        // truncation.
        let mut config = Config::default();
        config.security.max_shell_output_bytes = 256;
        let policy = ExecutionPolicy::default();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let args = serde_json::json!({ "command": "printf 'hello\\n'" });

        let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-small", None)
            .await
            .expect("shell command should run");
        assert!(result.contains("hello"), "got: {result}");
        assert!(
            !result.contains("bytes omitted"),
            "unexpected marker: {result}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_shell_tty_true_makes_stdout_a_tty() {
        // Confirms the PTY branch actually allocates a pseudo-terminal: a
        // command that asks `stdin/stdout` whether they are a TTY answers
        // "True" only when running under a PTY.
        let config = Config::default();
        let policy = ExecutionPolicy::default();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let args = serde_json::json!({
            "command": "test -t 1 && echo IS_TTY || echo NOT_TTY",
            "tty": true,
        });
        let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-1", None)
            .await
            .expect("tty shell should run");
        assert!(
            result.contains("IS_TTY"),
            "expected TTY output, got: {result}"
        );
        assert!(
            result.contains("--- output (tty) ---"),
            "tty branch should emit tty marker: {result}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_shell_tty_false_keeps_pipe_shape_and_separates_streams() {
        // Regression fence: omitting `tty` (or passing false) preserves the
        // original piped behavior — separate stdout/stderr sections in the
        // returned blob.
        let config = Config::default();
        let policy = ExecutionPolicy::default();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let args = serde_json::json!({
            "command": "test -t 1 && echo IS_TTY || echo NOT_TTY",
        });
        let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-1", None)
            .await
            .expect("non-tty shell should run");
        assert!(
            result.contains("NOT_TTY"),
            "expected non-tty output, got: {result}"
        );
        assert!(
            result.contains("--- stdout ---"),
            "non-tty must keep separate stdout: {result}"
        );
        assert!(
            result.contains("--- stderr ---"),
            "non-tty must keep separate stderr: {result}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_shell_tty_honors_workdir_and_timeout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("marker"), "x").expect("write marker");
        let config = Config::default();
        let policy = ExecutionPolicy::default();
        let (event_tx, _event_rx) = mpsc::channel(8);

        let args = serde_json::json!({
            "command": "ls",
            "tty": true,
            "workdir": tmp.path().to_string_lossy().as_ref(),
        });
        let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-1", None)
            .await
            .expect("workdir tty run");
        assert!(
            result.contains("marker"),
            "ls in workdir should see marker: {result}"
        );

        let timeout_args = serde_json::json!({
            "command": "sleep 5",
            "tty": true,
            "timeout_ms": 200,
        });
        let timed = handle_run_shell(&timeout_args, &config, &policy, &event_tx, "call-1", None)
            .await
            .expect("timeout path returns Ok with explanation");
        assert!(
            timed.contains("timed out") && timed.contains("(tty)"),
            "tty timeout should surface explicitly: {timed}"
        );
    }

    #[tokio::test]
    async fn run_shell_reroutes_apply_patch_heredoc_to_verified_path() {
        // Regression: when the model emits `bash -lc "apply_patch <<'EOF' ... EOF"`
        // we must dispatch through `handle_apply_patch` rather than running the
        // (non-existent) `apply_patch` binary in a subshell. The signal that the
        // reroute worked is a real file landing in cwd and a non-shell-shaped
        // success string.
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(tmp.path()).expect("chdir");

        let config = Config::default();
        let policy = ExecutionPolicy::default();
        let (event_tx, _event_rx) = mpsc::channel(8);

        let cmd = "bash -lc \"apply_patch <<'EOF'\n*** Begin Patch\n*** Add File: routed.txt\n+ok\n*** End Patch\nEOF\"";
        let args = serde_json::json!({ "command": cmd });

        let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-1", None)
            .await
            .expect("rerouted apply_patch should succeed");

        std::env::set_current_dir(&prev_cwd).expect("restore cwd");

        assert!(
            !result.contains("Exit code:"),
            "rerouted call must not return shell-shaped output: {result}"
        );
        assert!(
            tmp.path().join("routed.txt").exists(),
            "patched file should be created in cwd"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_shell_does_not_leak_parent_env_secrets() {
        // The actual leak regression: a provider-shaped env var on the
        // parent must NOT appear in the captured shell output. Without
        // `apply_filtered_env` on the spawn site, `env` would echo it
        // straight back to the agent (and into ~/.borg/logs/{date}.jsonl).
        let leak_name = "BORG_RUN_SHELL_TEST_FAKE_API_KEY";
        let leak_value = "must-not-appear-in-stdout-9f3d8c";
        unsafe {
            std::env::set_var(leak_name, leak_value);
        }

        // Pass an explicit workdir so this test doesn't race with other
        // tests in this module that mutate process-global cwd (see
        // `run_shell_reroutes_apply_patch_heredoc_to_verified_path`).
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = Config::default();
        let policy = ExecutionPolicy::default();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let args = serde_json::json!({
            "command": "env",
            "workdir": tmp.path().to_string_lossy().as_ref(),
        });

        let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-1", None)
            .await
            .expect("env command should run");

        unsafe {
            std::env::remove_var(leak_name);
        }

        assert!(
            result.contains("Exit code: 0"),
            "env should exit cleanly, got: {result}"
        );
        assert!(
            !result.contains(leak_value),
            "leak_value appeared in run_shell output (env_clear/filter missing): {result}"
        );
        assert!(
            !result.contains(leak_name),
            "leak_name appeared in run_shell output: {result}"
        );
        // Sanity floor: PATH should still be forwarded or the shell
        // wouldn't have found `env` in the first place. This also rules
        // out a degenerate "stdout was empty" pass.
        assert!(
            result.contains("PATH="),
            "PATH must survive the filter for shells to be usable: {result}"
        );
    }

    #[tokio::test]
    async fn omitting_action_runs_one_shot_as_before() {
        // Backward-compat fence: with no `action`, the handler must run the
        // command to completion and return the one-shot shape — exactly the
        // pre-session behavior.
        let config = Config::default();
        let policy = ExecutionPolicy::default();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let args = serde_json::json!({ "command": "printf 'ok\\n'" });
        let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-1", None)
            .await
            .expect("one-shot run");
        assert!(result.contains("Exit code: 0"), "got: {result}");
        assert!(result.contains("--- stdout ---"), "got: {result}");
    }

    #[tokio::test]
    async fn session_actions_without_daemon_report_requirement() {
        // No session registry is installed in the test binary (no daemon), so
        // every persistent-session verb must degrade to an explanatory message
        // — never panic, never silently no-op.
        let config = Config::default();
        let policy = ExecutionPolicy::default();
        let (event_tx, _event_rx) = mpsc::channel(8);

        for args in [
            serde_json::json!({ "action": "start", "command": "cat" }),
            serde_json::json!({ "action": "poll", "session_id": "sh-1" }),
            serde_json::json!({ "action": "write_stdin", "session_id": "sh-1", "stdin": "x" }),
            serde_json::json!({ "action": "kill", "session_id": "sh-1" }),
        ] {
            let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-1", None)
                .await
                .expect("session action returns Ok with a message");
            assert!(
                result.contains("require the borg daemon"),
                "expected daemon-required message for {args}, got: {result}"
            );
        }
    }

    #[tokio::test]
    async fn run_shell_denies_control_plane_write_without_executing() {
        // A `run` command that redirects into Borg's own hooks config must be
        // refused by the shell-side control-plane guard — and refused *before*
        // execution, so there is no "Exit code" in the result (the command
        // never spawned). hooks.json runs arbitrary shell at every hook point.
        let config = Config::default();
        let policy = ExecutionPolicy::default();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let args = serde_json::json!({ "command": "echo pwned > ~/.borg/hooks.json" });
        let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-1", None)
            .await
            .expect("guarded command returns Ok with a message");
        assert!(result.contains("denied"), "expected denial, got: {result}");
        assert!(
            !result.contains("Exit code"),
            "guarded command must not execute: {result}"
        );
    }

    #[tokio::test]
    async fn write_stdin_control_plane_payload_denied_before_registry_check() {
        // `write_stdin` injects a fresh command into a live shell. The
        // control-plane guard must fire on the stdin payload *before* the
        // session-registry lookup — otherwise a live session is a bypass for
        // the file-write boundary. No registry is installed here, so a guarded
        // payload must still return the control-plane denial, NOT the
        // daemon-required message.
        let config = Config::default();
        let policy = ExecutionPolicy::default();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let args = serde_json::json!({
            "action": "write_stdin",
            "session_id": "sh-1",
            "stdin": "echo pwned > ~/.borg/hooks.json\n",
        });
        let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-1", None)
            .await
            .expect("guarded write_stdin returns Ok with a message");
        assert!(result.contains("denied"), "expected denial, got: {result}");
        assert!(
            !result.contains("require the borg daemon"),
            "control-plane guard must precede the registry check: {result}"
        );
    }

    /// A one-shot `run_shell` command must not inherit the parent's stdin.
    ///
    /// Regression shape: the parent's stdin is a pipe held open with nothing
    /// ever written to it. If the child inherits it, `cat` blocks on read
    /// forever and the call only returns when the command timeout fires. With
    /// stdin nulled, `cat` sees immediate EOF and returns empty output. The
    /// timeout here is much shorter than the shell timeout, so an inherited
    /// stdin fails the test instead of merely slowing it down.
    ///
    /// Runs in a child process so the pipe replaces the *real* fd 0 — setting
    /// it only for the spawned command would test nothing.
    #[tokio::test]
    async fn one_shot_run_shell_does_not_inherit_parent_stdin() {
        const CHILD_ENV: &str = "BORG_RUN_SHELL_STDIN_CHILD";

        if std::env::var_os(CHILD_ENV).is_some() {
            let config = Config::default();
            let policy = ExecutionPolicy::default();
            let (event_tx, _event_rx) = mpsc::channel(64);
            let args = serde_json::json!({ "command": "cat" });
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                handle_run_shell(&args, &config, &policy, &event_tx, "call-1", None),
            )
            .await
            .expect("`cat` must not block on an inherited stdin")
            .expect("run_shell should succeed");
            assert!(
                !result.contains("timed out"),
                "command must complete, not time out: {result}"
            );
            return;
        }

        // `--exact` matches the fully qualified test path. Passing the bare
        // function name would select zero tests, and a child that ran nothing
        // still exits 0 — the assertion below would pass vacuously.
        const CHILD_TEST: &str =
            "tool_handlers::shell::tests::one_shot_run_shell_does_not_inherit_parent_stdin";
        let exe = std::env::current_exe().expect("test executable path");
        let mut child = std::process::Command::new(exe)
            .args([CHILD_TEST, "--exact", "--nocapture"])
            .env(CHILD_ENV, "1")
            // Hand the child an open-but-silent stdin. A `cat` that inherits
            // this never sees EOF.
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn child test process");
        let held_stdin = child.stdin.take().expect("hold child stdin open");
        let status = child.wait().expect("wait for child test process");
        drop(held_stdin);
        assert!(status.success(), "child test process failed: {status}");
    }

    #[tokio::test]
    async fn run_shell_allows_reading_control_plane_file() {
        // Reads of a control-plane file are not policy mutations and must keep
        // working — `cat`-ing hooks.json (here a harmless echo of the path) is
        // not a write, so the guard must not fire.
        let config = Config::default();
        let policy = ExecutionPolicy::default();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let args = serde_json::json!({ "command": "echo reading ~/.borg/hooks.json" });
        let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-1", None)
            .await
            .expect("benign command runs");
        assert!(
            result.contains("Exit code: 0"),
            "benign read-shaped command must execute: {result}"
        );
        assert!(!result.contains("denied"), "must not be denied: {result}");
    }

    #[tokio::test]
    async fn unknown_action_is_reported_not_executed() {
        let config = Config::default();
        let policy = ExecutionPolicy::default();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let args = serde_json::json!({ "action": "frobnicate", "command": "echo hi" });
        let result = handle_run_shell(&args, &config, &policy, &event_tx, "call-1", None)
            .await
            .expect("unknown action returns Ok with a message");
        assert!(result.contains("Unknown run_shell action"), "got: {result}");
    }
}
