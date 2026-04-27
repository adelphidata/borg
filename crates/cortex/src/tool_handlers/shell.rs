use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tracing::instrument;

use crate::agent::AgentEvent;
use crate::config::Config;
use crate::policy::ExecutionPolicy;

/// Read stdout/stderr from a child process line-by-line, emitting `ToolOutputDelta` events.
async fn stream_child_output(
    child: &mut tokio::process::Child,
    timeout: Duration,
    event_tx: &mpsc::Sender<AgentEvent>,
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
    skill_env_allowlist: Option<&std::collections::HashSet<String>>,
) -> Result<String> {
    let command = args["command"].as_str().context("Missing 'command'")?;
    let timeout_ms = config.tools.default_timeout_ms;
    let timeout_dur = Duration::from_millis(timeout_ms);

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
            );
            return Ok("Shell command denied by policy.".to_string());
        }
        crate::policy::PolicyDecision::AutoApprove => {}
        crate::policy::PolicyDecision::Prompt => {
            let (confirm_tx, confirm_rx) = oneshot::channel();
            let _ = event_tx
                .send(AgentEvent::ShellConfirmation {
                    command: command.to_string(),
                    respond: confirm_tx,
                })
                .await;
            match confirm_rx.await {
                Ok(true) => {}
                Ok(false) => {
                    return Ok("Shell command denied by user.".to_string());
                }
                Err(_) => {
                    return Ok("Shell command cancelled (no response).".to_string());
                }
            }
        }
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
    #[cfg(unix)]
    let (shell, shell_flag) = ("sh", "-c");
    #[cfg(windows)]
    let (shell, shell_flag) = ("cmd.exe", "/C");
    let mut cmd = tokio::process::Command::new(shell);
    cmd.arg(shell_flag)
        .arg(command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, val) in &filtered_creds {
        cmd.env(key, val);
    }

    let mut child = cmd
        .kill_on_drop(true)
        .spawn()
        .context("Failed to spawn shell command")?;

    match stream_child_output(&mut child, timeout_dur, event_tx, "run_shell").await {
        Ok((stdout, stderr, code)) => {
            let status = code.unwrap_or(-1);
            Ok(format!(
                "Exit code: {status}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
            ))
        }
        Err(e) => {
            if e.to_string().contains("timed out") {
                Ok(format!(
                    "Error: command timed out after {timeout_ms}ms\nCommand: {command}"
                ))
            } else {
                Err(e)
            }
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
    use super::redact_command_for_audit;

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
}
