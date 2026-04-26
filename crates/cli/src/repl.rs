//! REPL entry points.
//!
//! `run()` is a thin alias to the TUI launcher. `one_shot()` is the
//! `borg ask` / piped-input path: it connects (or auto-spawns) `borgd`,
//! opens an ephemeral session, sends one message, and prints the streamed
//! response. The agent itself only ever runs inside `borgd`.

use anyhow::{Context, Result};
use std::io::{self, Write};

use borg_core::agent::AgentEvent;

use crate::daemon_client::{connect_or_spawn, spawn_event_adapter};

pub async fn run(resume: Option<String>) -> Result<Option<crate::tui::ResumeHint>> {
    crate::tui::run(resume).await
}

/// Send `message` to a fresh daemon session and stream the response to
/// stdout. `auto_approve` answers shell-confirmation prompts; `json_output`
/// buffers the full text and prints a JSON envelope at end-of-turn.
///
/// `mode` is currently informational — collaboration mode now lives in the
/// daemon's persisted config; CLI-level mode override is reserved for a
/// future Admin RPC and ignored here so the call still succeeds.
pub async fn one_shot(
    message: &str,
    auto_approve: bool,
    json_output: bool,
    _mode: Option<&str>,
) -> Result<()> {
    let client = connect_or_spawn()
        .await
        .context("connect to borgd for one-shot")?;
    let mut session_client = client.clone();
    let (session_id, _last_seq) = session_client
        .open_session(None)
        .await
        .context("open one-shot session")?;
    let stream = session_client
        .send_message(&session_id, message)
        .await
        .context("send one-shot message")?;
    // The adapter forwards prompt replies via its own DaemonClient clone.
    let mut event_rx = spawn_event_adapter(client, session_id.clone(), stream);

    let mut stdout = io::stdout();
    let mut full_response = String::new();
    let mut exit_code: i32 = 0;

    while let Some(event) = event_rx.recv().await {
        match event {
            AgentEvent::TextDelta(delta) => {
                if json_output {
                    full_response.push_str(&delta);
                } else {
                    print!("{delta}");
                    if let Err(e) = stdout.flush() {
                        tracing::warn!(error = %e, "stdout flush failed");
                    }
                }
            }
            AgentEvent::TurnComplete => {
                if json_output {
                    let output = serde_json::json!({
                        "response": full_response,
                        "exit_code": exit_code,
                    });
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&output).unwrap_or_default()
                    );
                } else {
                    println!();
                }
            }
            AgentEvent::ShellConfirmation { respond, command } => {
                if auto_approve {
                    if respond.send(true).is_err() {
                        tracing::warn!("shell-approval channel closed before reply");
                    } else if !json_output {
                        eprintln!("[auto-approved] {command}");
                    }
                } else {
                    if respond.send(false).is_err() {
                        tracing::warn!("shell-deny channel closed before reply");
                    }
                    if !json_output {
                        eprintln!("Shell command denied in one-shot mode. Use --yes to allow.");
                    }
                }
            }
            AgentEvent::Error(e) => {
                eprintln!("Error: {e}");
                exit_code = 1;
            }
            AgentEvent::ToolExecuting { name, .. } => {
                if !json_output {
                    eprintln!("[running {name}]");
                }
            }
            AgentEvent::ToolResult { name, result } => {
                if !json_output {
                    let preview =
                        if result.len() > borg_core::constants::TOOL_RESULT_PREVIEW_CHARS {
                            let mut end = borg_core::constants::TOOL_RESULT_PREVIEW_CHARS;
                            while end > 0 && !result.is_char_boundary(end) {
                                end -= 1;
                            }
                            &result[..end]
                        } else {
                            &result
                        };
                    eprintln!("[{name} done] {preview}");
                }
            }
            AgentEvent::ToolOutputDelta {
                delta, is_stderr, ..
            } => {
                if !json_output {
                    let prefix = if is_stderr { "! " } else { "" };
                    eprintln!("  {prefix}{delta}");
                }
            }
            _ => {}
        }
    }

    // Best-effort close — the daemon evicts idle sessions on shutdown anyway.
    if let Err(e) = session_client.close_session(&session_id).await {
        tracing::debug!(error = %e, "close one-shot session");
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}
