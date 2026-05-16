use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::instrument;

use crate::agent::AgentEvent;
use crate::config::Config;
use crate::execution_context::ExecutionContext;
use crate::policy::ExecutionPolicy;

/// Read stdout/stderr from a child process line-by-line, emitting `ToolOutputDelta` events.
async fn stream_child_output(
    child: &mut tokio::process::Child,
    timeout: Duration,
    event_tx: &mpsc::Sender<AgentEvent>,
    tool_call_id: &str,
    tool_name: &str,
) -> Result<(String, String, Option<i32>)> {
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();

    let streaming_future = async {
        let mut stdout_lines = stdout_pipe.map(|p| BufReader::new(p).lines());
        let mut stderr_lines = stderr_pipe.map(|p| BufReader::new(p).lines());
        let mut stdout_done = stdout_lines.is_none();
        let mut stderr_done = stderr_lines.is_none();

        while !stdout_done || !stderr_done {
            tokio::select! {
                line = async {
                    match stdout_lines.as_mut() {
                        Some(reader) => reader.next_line().await,
                        None => std::future::pending().await,
                    }
                }, if !stdout_done => {
                    match line {
                        Ok(Some(l)) => {
                            let _ = event_tx.try_send(AgentEvent::ToolOutputDelta {
                                tool_call_id: tool_call_id.to_string(),
                                name: tool_name.to_string(),
                                delta: l.clone(),
                                is_stderr: false,
                            });
                            stdout_buf.push_str(&l);
                            stdout_buf.push('\n');
                        }
                        _ => { stdout_done = true; }
                    }
                }
                line = async {
                    match stderr_lines.as_mut() {
                        Some(reader) => reader.next_line().await,
                        None => std::future::pending().await,
                    }
                }, if !stderr_done => {
                    match line {
                        Ok(Some(l)) => {
                            let _ = event_tx.try_send(AgentEvent::ToolOutputDelta {
                                tool_call_id: tool_call_id.to_string(),
                                name: tool_name.to_string(),
                                delta: l.clone(),
                                is_stderr: true,
                            });
                            stderr_buf.push_str(&l);
                            stderr_buf.push('\n');
                        }
                        _ => { stderr_done = true; }
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

    Ok((stdout_buf, stderr_buf, status.code()))
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
    if let Some(patch_text) = coding::maybe_extract_apply_patch_from_shell(command) {
        let patch_args = serde_json::json!({ "patch": patch_text });
        return crate::tool_handlers::filesystem::handle_apply_patch(&patch_args, config);
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
    // profiles (M2) can validate it. Bare `sh` is rejected by Seatbelt
    // profile generation (paths must be absolute).
    #[cfg(unix)]
    let (shell, shell_flag) = ("/bin/sh", "-c");
    #[cfg(windows)]
    let (shell, shell_flag) = ("cmd.exe", "/C");

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

    match stream_child_output(&mut child, timeout_dur, event_tx, tool_call_id, "run_shell").await {
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
        fs_write: vec![workspace_root_str],
        deny_read: Vec::new(),
        deny_write: Vec::new(),
        allowed_domains: Vec::new(),
    }
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
        let mut combined = String::new();
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                    let _ = event_tx_clone.try_send(AgentEvent::ToolOutputDelta {
                        tool_call_id: tool_call_id.clone(),
                        name: "run_shell".to_string(),
                        delta: chunk.clone(),
                        is_stderr: false,
                    });
                    combined.push_str(&chunk);
                }
                Err(_) => break,
            }
        }
        combined
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
    use super::{handle_run_shell, redact_command_for_audit};
    use crate::agent::AgentEvent;
    use crate::config::Config;
    use crate::policy::{ExecutionPolicy, PolicyDecision};
    use tokio::sync::mpsc;

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
}
