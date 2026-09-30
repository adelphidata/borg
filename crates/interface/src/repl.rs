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
    // Same predicate as the heartbeat's `SkipReason::NoLlmProvider` and the
    // scheduled-task preflight: without a provider the agent has no model to
    // call and every turn will fail.
    if config.llm.provider_unset() {
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
    let event_rx = spawn_event_adapter(client, session_id.clone(), stream);

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

    let exit_code = drain_one_shot(event_rx, json_output, &warnings, &mut io::stdout()).await;

    // Best-effort close — the daemon evicts idle sessions on shutdown anyway.
    if let Err(e) = session_client.close_session(&session_id).await {
        tracing::debug!(error = %e, "close one-shot session");
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

/// Exit code of a one-shot turn that completed with no error.
const EXIT_OK: i32 = 0;
/// Exit code when the turn reported an error.
const EXIT_TURN_ERROR: i32 = 1;
/// Exit code when the event stream closed before the turn completed.
const EXIT_INCOMPLETE: i32 = 2;

/// Print one turn's events and return the process exit code.
///
/// The exit code is the contract for scripts: `0` only when the turn
/// completed and reported no error, `1` when it reported an error, `2` when
/// the stream closed with no completion (daemon exit, session eviction). The
/// response goes to `out`; progress and errors go to stderr. In JSON mode the
/// envelope is printed one time after the last event, so its `exit_code` is
/// the final one.
async fn drain_one_shot(
    mut event_rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    json_output: bool,
    warnings: &[String],
    out: &mut impl Write,
) -> i32 {
    let mut full_response = String::new();
    let mut saw_turn_complete = false;
    let mut saw_error = false;

    while let Some(event) = event_rx.recv().await {
        match event {
            AgentEvent::TextDelta(delta) => {
                if json_output {
                    full_response.push_str(&delta);
                } else if let Err(e) = write!(out, "{delta}").and_then(|()| out.flush()) {
                    tracing::warn!(error = %e, "stdout write failed");
                }
            }
            // The attempt that sent this text failed; the full response
            // comes again. The JSON envelope holds the text of the attempt
            // that completes. Plain text is on the stream already and cannot
            // be removed, so a new line and a notice on stderr show where
            // the response starts again.
            AgentEvent::TextRetracted { bytes } => {
                if json_output {
                    cortex::agent::retract_text_tail(&mut full_response, bytes);
                } else {
                    eprintln!("[connection lost; the response starts again]");
                    if let Err(e) = writeln!(out).and_then(|()| out.flush()) {
                        tracing::warn!(error = %e, "stdout write failed");
                    }
                }
            }
            AgentEvent::TurnComplete => {
                saw_turn_complete = true;
                if !json_output {
                    if let Err(e) = writeln!(out) {
                        tracing::warn!(error = %e, "stdout write failed");
                    }
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
                saw_error = true;
            }
            // A notice (budget warning, posture change) is not a failure.
            AgentEvent::SystemNotice { text, .. } => {
                eprintln!("{text}");
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

    let exit_code = if saw_error {
        EXIT_TURN_ERROR
    } else if saw_turn_complete {
        EXIT_OK
    } else {
        eprintln!("Error: the connection to borgd closed before the turn finished");
        EXIT_INCOMPLETE
    };

    if json_output {
        let envelope = serde_json::json!({
            "response": full_response,
            "exit_code": exit_code,
            "completed": saw_turn_complete,
            "warnings": warnings,
        });
        match serde_json::to_string_pretty(&envelope) {
            Ok(text) => {
                if let Err(e) = writeln!(out, "{text}") {
                    tracing::warn!(error = %e, "stdout write failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "serialize one-shot envelope"),
        }
    }
    exit_code
}

#[cfg(test)]
mod drain_one_shot_tests {
    use super::*;
    use tokio::sync::mpsc;

    /// Feed `events`, close the channel, and return (exit code, stdout).
    async fn drain(events: Vec<AgentEvent>, json_output: bool) -> (i32, String) {
        let (tx, rx) = mpsc::channel(16);
        for event in events {
            tx.send(event).await.expect("send");
        }
        drop(tx);
        let mut out = Vec::new();
        let warnings = vec!["degraded".to_string()];
        let code = drain_one_shot(rx, json_output, &warnings, &mut out).await;
        (code, String::from_utf8(out).expect("utf8"))
    }

    fn text(s: &str) -> AgentEvent {
        AgentEvent::TextDelta(s.to_string())
    }

    fn budget_notice() -> AgentEvent {
        AgentEvent::SystemNotice {
            category: "budget".to_string(),
            text: "Warning: 90% of monthly token budget used".to_string(),
        }
    }

    /// Spec: the JSON envelope holds the text of the attempt that
    /// completes. Plain output cannot remove what it printed, so the
    /// response of the new attempt starts on its own line.
    #[tokio::test]
    async fn a_retraction_removes_the_text_of_the_failed_attempt() {
        let events = || {
            vec![
                AgentEvent::LlmAttemptStarted { attempt: 0 },
                text("Die Antwort ist f\u{fc}"),
                AgentEvent::TextRetracted { bytes: 19 },
                AgentEvent::LlmAttemptStarted { attempt: 1 },
                text("Die Antwort ist "),
                text("f\u{fc}nf."),
                AgentEvent::TurnComplete,
            ]
        };

        let (code, out) = drain(events(), true).await;
        assert_eq!(code, EXIT_OK);
        let envelope: serde_json::Value = serde_json::from_str(&out).expect("json envelope");
        assert_eq!(envelope["response"], "Die Antwort ist f\u{fc}nf.");

        let (code, out) = drain(events(), false).await;
        assert_eq!(code, EXIT_OK);
        assert_eq!(
            out.lines().last(),
            Some("Die Antwort ist f\u{fc}nf."),
            "the last line is the complete response: {out:?}"
        );
    }

    #[tokio::test]
    async fn exit_code_and_envelope_follow_the_turn_result() {
        // (name, events, exit code, completed, response)
        let cases = vec![
            (
                "completed turn",
                vec![text("hel"), text("lo"), AgentEvent::TurnComplete],
                0,
                true,
                "hello",
            ),
            (
                "stream closed before completion",
                vec![text("partial")],
                2,
                false,
                "partial",
            ),
            ("empty stream", vec![], 2, false, ""),
            (
                "error after completion",
                vec![
                    text("answer"),
                    AgentEvent::TurnComplete,
                    AgentEvent::Error("late failure".to_string()),
                ],
                1,
                true,
                "answer",
            ),
            (
                "error with no completion",
                vec![AgentEvent::Error("boom".to_string())],
                1,
                false,
                "",
            ),
            (
                "budget notice is not a failure",
                vec![budget_notice(), text("ok"), AgentEvent::TurnComplete],
                0,
                true,
                "ok",
            ),
        ];
        for (name, events, expected_code, completed, response) in cases {
            let (code, out) = drain(events, true).await;
            assert_eq!(code, expected_code, "case {name}");
            let envelope: serde_json::Value = serde_json::from_str(&out)
                .unwrap_or_else(|e| panic!("case {name}: one JSON envelope expected ({e}): {out}"));
            assert_eq!(
                envelope,
                serde_json::json!({
                    "response": response,
                    "exit_code": expected_code,
                    "completed": completed,
                    "warnings": ["degraded"],
                }),
                "case {name}"
            );
        }
    }

    #[tokio::test]
    async fn text_mode_prints_only_the_response_on_stdout() {
        let (code, out) = drain(
            vec![
                budget_notice(),
                text("hel"),
                AgentEvent::ToolExecuting {
                    tool_call_id: "c1".to_string(),
                    name: "run_shell".to_string(),
                    args: "{}".to_string(),
                },
                text("lo"),
                AgentEvent::TurnComplete,
            ],
            false,
        )
        .await;
        assert_eq!(code, 0);
        assert_eq!(out, "hello\n");

        let (code, out) = drain(vec![text("partial")], false).await;
        assert_eq!(code, 2, "a closed stream is not a success in text mode");
        assert_eq!(out, "partial");
    }
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
