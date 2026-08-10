//! REPL entry points.
//!
//! `run()` is a thin alias to the TUI launcher. `one_shot()` is the
//! `borg ask` / piped-input path: it connects (or auto-spawns) `borgd`,
//! opens an ephemeral session, sends one message, and prints the streamed
//! response. The agent itself only ever runs inside `borgd`.

use anyhow::{Context, Result};
use std::io::{self, Write};

use cortex::agent::AgentEvent;
use cortex::config::security::KekBackend;
use cortex::config::Config;

use crate::daemon_client::{connect_or_spawn, spawn_event_adapter};

pub async fn run(
    resume: Option<String>,
    workspace: Option<std::path::PathBuf>,
) -> Result<Option<crate::tui::ResumeHint>> {
    crate::tui::run(resume, workspace).await
}

/// Runtime-degradation warnings derivable cheaply from persisted config, for
/// headless runs (`borg ask`) that have no TUI status surface to show them.
///
/// Config-only by design — no `doctor` sweep or DB probe — so it adds
/// negligible cost to a one-shot. DB auto-recovery is surfaced separately at
/// process boot (see `main`), so it is intentionally not repeated here.
pub(crate) fn runtime_degradation_warnings(config: &Config) -> Vec<String> {
    let mut out = Vec::new();
    // Mirrors the heartbeat's `SkipReason::NoLlmProvider` condition: without a
    // provider the agent has no model to call and every turn will fail.
    let provider = config.llm.provider.as_deref().map(str::trim).unwrap_or("");
    if provider.is_empty() {
        out.push(
            "no LLM provider configured — the agent cannot run turns until one is set \
             (`borg settings set llm.provider <name>`)"
                .to_string(),
        );
    }
    // `Auto` is persisted as the resolved backend after boot, so `File` here
    // means the OS keychain was unavailable and posture is Reduced (×0.85).
    if config.security.kek_backend == KekBackend::File {
        out.push(
            "secret storage is using the passphrase-encrypted file backend (reduced security \
             posture); `borg service install --secure` upgrades to the OS keychain"
                .to_string(),
        );
    }
    out
}

/// Send `message` to a fresh daemon session and stream the response to
/// stdout. `json_output` buffers the full text and prints a JSON envelope
/// at end-of-turn.
///
/// `mode` is currently informational — collaboration mode now lives in the
/// daemon's persisted config; CLI-level mode override is reserved for a
/// future Admin RPC and ignored here so the call still succeeds.
pub async fn one_shot(
    message: &str,
    json_output: bool,
    _mode: Option<&str>,
    workspace: Option<std::path::PathBuf>,
) -> Result<()> {
    let client = connect_or_spawn()
        .await
        .context("connect to borgd for one-shot")?;
    let mut session_client = client.clone();
    let (session_id, _last_seq) = session_client
        .open_session_with_workspace(None, workspace.as_deref(), None, false)
        .await
        .context("open one-shot session")?;
    let stream = session_client
        .send_message(&session_id, message)
        .await
        .context("send one-shot message")?;
    // The adapter forwards prompt replies via its own DaemonClient clone.
    let mut event_rx = spawn_event_adapter(client, session_id.clone(), stream);

    // Surface cheap runtime-degradation signals up front: headless runs have no
    // status bar, so a missing provider or reduced posture would otherwise stay
    // invisible until a turn mysteriously fails. Best-effort — a config load
    // failure must not block the actual request.
    let warnings = match Config::load_from_db().await {
        Ok(cfg) => runtime_degradation_warnings(&cfg),
        Err(e) => {
            tracing::warn!(error = %e, "load config for degradation warnings");
            Vec::new()
        }
    };
    if !json_output {
        for w in &warnings {
            eprintln!("Warning: {w}");
        }
    }

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
                        "warnings": warnings,
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
                tracing::warn!("unexpected shell confirmation in one-shot mode: {command}");
                if respond.send(false).is_err() {
                    tracing::warn!("shell-deny channel closed before reply");
                }
                if !json_output {
                    eprintln!("Shell command denied by daemon policy.");
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
            AgentEvent::ToolResult { name, result, .. } => {
                if !json_output {
                    let preview = if result.len() > cortex::constants::TOOL_RESULT_PREVIEW_CHARS {
                        let mut end = cortex::constants::TOOL_RESULT_PREVIEW_CHARS;
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
            } if !json_output => {
                let prefix = if is_stderr { "! " } else { "" };
                eprintln!("  {prefix}{delta}");
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

#[cfg(test)]
mod degradation_tests {
    use super::*;

    #[test]
    fn missing_provider_is_flagged() {
        let mut c = Config::default();
        c.llm.provider = None;
        let w = runtime_degradation_warnings(&c);
        assert!(
            w.iter().any(|s| s.contains("no LLM provider")),
            "expected a provider warning, got {w:?}"
        );
    }

    #[test]
    fn blank_provider_is_treated_as_unset() {
        let mut c = Config::default();
        c.llm.provider = Some("   ".to_string());
        let w = runtime_degradation_warnings(&c);
        assert!(w.iter().any(|s| s.contains("no LLM provider")), "{w:?}");
    }

    #[test]
    fn file_kek_backend_flags_reduced_posture() {
        let mut c = Config::default();
        c.llm.provider = Some("openrouter".to_string());
        c.security.kek_backend = KekBackend::File;
        let w = runtime_degradation_warnings(&c);
        assert!(
            w.iter().any(|s| s.contains("reduced security posture")),
            "expected a posture warning, got {w:?}"
        );
    }

    #[test]
    fn healthy_config_produces_no_warnings() {
        let mut c = Config::default();
        c.llm.provider = Some("openrouter".to_string());
        c.security.kek_backend = KekBackend::OsKeychain;
        assert!(
            runtime_degradation_warnings(&c).is_empty(),
            "a configured provider + keychain backend must be clean"
        );
    }

    #[test]
    fn both_degradations_surface_together() {
        let mut c = Config::default();
        c.llm.provider = None;
        c.security.kek_backend = KekBackend::File;
        let w = runtime_degradation_warnings(&c);
        assert_eq!(w.len(), 2, "both signals should be reported: {w:?}");
    }
}
