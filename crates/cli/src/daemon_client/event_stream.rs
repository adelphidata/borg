//! Convert a gRPC `pb::AgentEvent` stream back into the in-process
//! `borg_core::agent::AgentEvent` enum used by the TUI/REPL render layer.
//!
//! The two prompt variants (`ShellConfirmation`, `UserInputRequest`) carry
//! `oneshot::Sender` channels in the core enum; on the wire they're a
//! `prompt_id`. We bridge by creating a local oneshot, handing the receiver
//! end to a background task that forwards the answer back to the daemon via
//! `Session.RespondToPrompt`. The render layer never knows it's talking to a
//! daemon — it consumes the same enum it always has.

use borg_core::agent::AgentEvent as CoreEvent;
use borg_core::tool_handlers::user_input::UserInputChoice;
use borg_core::types::{PlanStep, PlanStepStatus};
use borg_proto::session::agent_event::Kind;
use borg_proto::session::AgentEvent as PbEvent;
use tokio::sync::{mpsc, oneshot};
use tonic::Streaming;

use super::DaemonClient;

/// Spawn a translator task that drains `stream` (gRPC events for a single
/// turn or long-lived subscription) and emits `CoreEvent`s on the returned
/// receiver. Prompt responses are forwarded back to the daemon using a clone
/// of `client`.
///
/// The receiver closes when the upstream gRPC stream ends or errors. Errors
/// from the gRPC layer are surfaced as `CoreEvent::Error(...)` so callers can
/// render them like any other turn error.
pub fn spawn_event_adapter(
    client: DaemonClient,
    session_id: String,
    mut stream: Streaming<PbEvent>,
) -> mpsc::Receiver<CoreEvent> {
    let (tx, rx) = mpsc::channel::<CoreEvent>(256);
    tokio::spawn(async move {
        loop {
            match stream.message().await {
                Ok(Some(pb)) => {
                    if let Some(core_evt) = convert(pb, &client, &session_id) {
                        if tx.send(core_evt).await.is_err() {
                            break;
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(CoreEvent::Error(e.to_string())).await;
                    break;
                }
            }
        }
        // For session-Stream subscriptions the daemon side may still be
        // running; we simply drop. The render layer treats a closed channel
        // as "no more events from this stream."
        drop(client);
    });
    rx
}

/// Translate one wire event. Returns `None` for events we don't know how to
/// represent (forward-compat: a future proto variant is silently skipped
/// rather than crashing the render loop).
fn convert(pb: PbEvent, client: &DaemonClient, session_id: &str) -> Option<CoreEvent> {
    let kind = pb.kind?;
    Some(match kind {
        Kind::TextDelta(d) => CoreEvent::TextDelta(d.text),
        Kind::ThinkingDelta(d) => CoreEvent::ThinkingDelta(d.text),
        Kind::ToolExecuting(t) => CoreEvent::ToolExecuting {
            name: t.name,
            args: t.args_json,
        },
        Kind::ToolResult(t) => CoreEvent::ToolResult {
            name: t.name,
            result: t.result,
        },
        Kind::ShellConfirmation(s) => {
            let (respond_tx, respond_rx) = oneshot::channel::<bool>();
            spawn_shell_responder(client.clone(), session_id.to_string(), s.prompt_id, respond_rx);
            CoreEvent::ShellConfirmation {
                command: s.command,
                respond: respond_tx,
            }
        }
        Kind::ToolOutputDelta(t) => CoreEvent::ToolOutputDelta {
            name: t.name,
            delta: t.delta,
            is_stderr: t.is_stderr,
        },
        Kind::Usage(u) => CoreEvent::Usage(borg_core::llm::UsageData {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.input_tokens + u.output_tokens,
            cached_input_tokens: u.cache_read_tokens,
            cache_creation_tokens: u.cache_creation_tokens,
            provider: String::new(),
            model: String::new(),
        }),
        Kind::SubAgentUpdate(s) => CoreEvent::SubAgentUpdate {
            agent_id: s.agent_id,
            nickname: s.nickname,
            status: s.status,
        },
        Kind::SteerReceived(s) => CoreEvent::SteerReceived { text: s.text },
        Kind::PlanUpdated(p) => CoreEvent::PlanUpdated {
            steps: p
                .steps
                .into_iter()
                .map(|s| PlanStep {
                    title: s.text,
                    status: parse_plan_status(&s.status),
                })
                .collect(),
        },
        Kind::UserInputRequest(u) => {
            let (respond_tx, respond_rx) = oneshot::channel::<String>();
            spawn_input_responder(client.clone(), session_id.to_string(), u.prompt_id, respond_rx);
            CoreEvent::UserInputRequest {
                prompt: u.prompt,
                choices: u
                    .choices
                    .into_iter()
                    .map(|c| UserInputChoice {
                        label: c.label,
                        description: if c.description.is_empty() {
                            None
                        } else {
                            Some(c.description)
                        },
                    })
                    .collect(),
                allow_custom: u.allow_custom,
                respond: respond_tx,
            }
        }
        Kind::Preparing(_) => CoreEvent::Preparing,
        Kind::TurnComplete(_) => CoreEvent::TurnComplete,
        Kind::HistoryCompacted(h) => CoreEvent::HistoryCompacted {
            dropped: h.dropped as usize,
            before_tokens: h.before_tokens as usize,
            after_tokens: h.after_tokens as usize,
            iterative: h.iterative,
        },
        Kind::Error(e) => CoreEvent::Error(e.message),
    })
}

fn parse_plan_status(s: &str) -> PlanStepStatus {
    match s {
        "in_progress" => PlanStepStatus::InProgress,
        "completed" => PlanStepStatus::Completed,
        _ => PlanStepStatus::Pending,
    }
}

fn spawn_shell_responder(
    mut client: DaemonClient,
    session_id: String,
    prompt_id: String,
    rx: oneshot::Receiver<bool>,
) {
    tokio::spawn(async move {
        match rx.await {
            Ok(approved) => {
                let value = if approved { "true" } else { "false" };
                if let Err(e) = client.respond_to_prompt(&session_id, &prompt_id, value).await {
                    tracing::warn!(
                        session = %session_id,
                        prompt = %prompt_id,
                        error = %e,
                        "shell-confirmation reply failed",
                    );
                }
            }
            Err(_) => {
                // Renderer dropped without answering — daemon still has a
                // pending prompt; reply false so the agent doesn't hang.
                if let Err(e) = client.respond_to_prompt(&session_id, &prompt_id, "false").await {
                    tracing::warn!(error = %e, "fallback shell-deny failed");
                }
            }
        }
    });
}

fn spawn_input_responder(
    mut client: DaemonClient,
    session_id: String,
    prompt_id: String,
    rx: oneshot::Receiver<String>,
) {
    tokio::spawn(async move {
        match rx.await {
            Ok(value) => {
                if let Err(e) = client.respond_to_prompt(&session_id, &prompt_id, &value).await {
                    tracing::warn!(
                        session = %session_id,
                        prompt = %prompt_id,
                        error = %e,
                        "user-input reply failed",
                    );
                }
            }
            Err(_) => {
                // No answer — empty string keeps the agent from hanging while
                // signalling "no input."
                if let Err(e) = client.respond_to_prompt(&session_id, &prompt_id, "").await {
                    tracing::warn!(error = %e, "fallback user-input cancel failed");
                }
            }
        }
    });
}
