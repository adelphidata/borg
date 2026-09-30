//! Convert a gRPC `pb::AgentEvent` stream back into the in-process
//! `cortex::agent::AgentEvent` enum used by the TUI/REPL render layer.
//!
//! The two prompt variants (`ShellConfirmation`, `UserInputRequest`) carry
//! `oneshot::Sender` channels in the core enum; on the wire they're a
//! `prompt_id`. We bridge by creating a local oneshot, handing the receiver
//! end to a background task that forwards the answer back to the daemon via
//! `Session.RespondToPrompt`. The render layer never knows it's talking to a
//! daemon — it consumes the same enum it always has.

use std::collections::HashMap;

use cortex::agent::AgentEvent as CoreEvent;
use cortex::tool_handlers::user_input::{Answer, Question, QuestionOption};
use cortex::types::{PlanStep, PlanStepStatus};
use protocols::session::agent_event::Kind;
use protocols::session::AgentEvent as PbEvent;
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
                    if let Some(core_evt) = convert_pb_event(pb, &client, &session_id) {
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

/// Drain a `Streaming<PbEvent>` into an existing `mpsc::Sender<CoreEvent>`.
///
/// Variant of [`spawn_event_adapter`] for callers that already own the
/// receiver end (e.g. the TUI's `App::event_rx`). Reuses the same `convert()`
/// translator and the prompt-responder helpers, so prompts continue to
/// round-trip through `Session.RespondToPrompt`.
///
/// Terminates when the upstream gRPC stream ends, errors, or `tx` is closed
/// (the renderer dropped its receiver). The latter is important: a regression
/// that swallowed `mpsc::send` errors caused this forwarder to spin after the
/// renderer went away.
pub async fn forward_event_stream(
    client: DaemonClient,
    session_id: String,
    mut stream: Streaming<PbEvent>,
    tx: mpsc::Sender<CoreEvent>,
) {
    loop {
        match stream.message().await {
            Ok(Some(pb)) => {
                if !forward_one(pb, &client, &session_id, &tx).await {
                    return;
                }
            }
            Ok(None) => break,
            Err(e) => {
                let _ = tx.send(CoreEvent::Error(e.to_string())).await;
                break;
            }
        }
    }
}

/// Convert + forward a single event. Returns `false` when the receiver has
/// gone away (so the caller should stop pulling from the upstream stream).
/// Returns `true` if the event was sent OR silently skipped (unknown variant).
async fn forward_one(
    pb: PbEvent,
    client: &DaemonClient,
    session_id: &str,
    tx: &mpsc::Sender<CoreEvent>,
) -> bool {
    if let Some(core_evt) = convert_pb_event(pb, client, session_id) {
        if tx.send(core_evt).await.is_err() {
            return false;
        }
    }
    true
}

/// Translate one wire event. Returns `None` for events we don't know how to
/// represent (forward-compat: a future proto variant is silently skipped
/// rather than crashing the render loop).
pub(crate) fn convert_pb_event(
    pb: PbEvent,
    client: &DaemonClient,
    session_id: &str,
) -> Option<CoreEvent> {
    let kind = pb.kind?;
    Some(match kind {
        Kind::TextDelta(d) => CoreEvent::TextDelta(d.text),
        Kind::TextRetracted(r) => CoreEvent::TextRetracted {
            bytes: usize::try_from(r.bytes).unwrap_or(usize::MAX),
        },
        Kind::ThinkingDelta(d) => CoreEvent::ThinkingDelta(d.text),
        Kind::ToolExecuting(t) => CoreEvent::ToolExecuting {
            tool_call_id: t.tool_call_id,
            name: t.name,
            args: t.args_json,
        },
        Kind::ToolResult(t) => CoreEvent::ToolResult {
            tool_call_id: t.tool_call_id,
            name: t.name,
            result: t.result,
        },
        Kind::ShellConfirmation(s) => {
            let (respond_tx, respond_rx) = oneshot::channel::<bool>();
            spawn_shell_responder(
                client.clone(),
                session_id.to_string(),
                s.prompt_id,
                respond_rx,
            );
            CoreEvent::ShellConfirmation {
                command: s.command,
                respond: respond_tx,
            }
        }
        Kind::ToolOutputDelta(t) => CoreEvent::ToolOutputDelta {
            tool_call_id: t.tool_call_id,
            name: t.name,
            delta: t.delta,
            is_stderr: t.is_stderr,
        },
        Kind::Usage(u) => CoreEvent::Usage(cortex::llm::UsageData {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.input_tokens + u.output_tokens,
            cached_input_tokens: u.cache_read_tokens,
            cache_creation_tokens: u.cache_creation_tokens,
            provider: String::new(),
            model: String::new(),
            provider_cost_usd: None,
            response_id: None,
            upstream: None,
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
            let (respond_tx, respond_rx) = oneshot::channel::<HashMap<String, Answer>>();
            spawn_input_responder(
                client.clone(),
                session_id.to_string(),
                u.prompt_id,
                respond_rx,
            );
            let questions: Vec<Question> = u
                .questions
                .into_iter()
                .map(|q| Question {
                    id: q.id,
                    header: q.header,
                    question: q.question,
                    multi_select: q.multi_select,
                    options: q
                        .options
                        .into_iter()
                        .map(|o| QuestionOption {
                            label: o.label,
                            description: o.description,
                        })
                        .collect(),
                })
                .collect();
            CoreEvent::UserInputRequest {
                questions,
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
        Kind::SystemNotice(n) => CoreEvent::SystemNotice {
            category: n.category,
            text: n.text,
        },
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
                if let Err(e) = client
                    .respond_to_prompt(&session_id, &prompt_id, value)
                    .await
                {
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
                if let Err(e) = client
                    .respond_to_prompt(&session_id, &prompt_id, "false")
                    .await
                {
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
    rx: oneshot::Receiver<HashMap<String, Answer>>,
) {
    tokio::spawn(async move {
        match rx.await {
            Ok(map) => {
                let value = serde_json::to_string(&map).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "failed to encode user-input answers; sending empty map");
                    "{}".to_string()
                });
                if let Err(e) = client
                    .respond_to_prompt(&session_id, &prompt_id, &value)
                    .await
                {
                    tracing::warn!(
                        session = %session_id,
                        prompt = %prompt_id,
                        error = %e,
                        "user-input reply failed",
                    );
                }
            }
            Err(_) => {
                // No answer — empty map keeps the agent from hanging while
                // signalling "no input." (Each question key absent => empty
                // answers list; matches `serialize_answers` fill-missing
                // behavior.)
                if let Err(e) = client
                    .respond_to_prompt(&session_id, &prompt_id, "{}")
                    .await
                {
                    tracing::warn!(error = %e, "fallback user-input cancel failed");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    //! Forwarder tests focus on `forward_one`, the per-event slice of
    //! `forward_event_stream`. This is a deliberate decision:
    //! `tonic::Streaming` has no public constructor, so a "real" forwarder
    //! test would have to stand up a full gRPC server. `forward_one` is the
    //! only piece with non-trivial behavior (convert + send + detect closed
    //! receiver); the wrapper loop is mechanical.
    //!
    //! Regression guard: a previous version swallowed `mpsc::send` errors
    //! and spun after the renderer dropped its receiver. The closed-receiver
    //! test below would catch that recurring.
    use super::*;
    use protocols::session::{
        agent_event::Kind as PbKind, AgentEvent as PbEvent, TextDelta, ToolExecuting, TurnComplete,
    };

    fn pb(kind: PbKind) -> PbEvent {
        PbEvent {
            kind: Some(kind),
            event_seq: 0,
        }
    }

    /// Build a DaemonClient wrapping a non-functional channel — the prompt
    /// responder paths aren't exercised by the events used here, so the
    /// channel is never actually called.
    fn dummy_client() -> DaemonClient {
        // `Channel::from_static(...).connect_lazy()` returns immediately and
        // only fails on first RPC; safe to construct in tests.
        let channel = tonic::transport::Channel::from_static("http://127.0.0.1:1").connect_lazy();
        DaemonClient::from_channel(channel)
    }

    #[tokio::test]
    async fn forwards_three_events_in_order() {
        let client = dummy_client();
        let (tx, mut rx) = mpsc::channel::<CoreEvent>(16);

        let events = vec![
            pb(PbKind::TextDelta(TextDelta {
                text: "hello".into(),
            })),
            pb(PbKind::ToolExecuting(ToolExecuting {
                tool_call_id: "call-7".into(),
                name: "shell".into(),
                args_json: "{}".into(),
            })),
            pb(PbKind::TurnComplete(TurnComplete {})),
        ];

        for ev in events {
            assert!(
                forward_one(ev, &client, "sess-1", &tx).await,
                "forward_one returned false before receiver was dropped"
            );
        }
        drop(tx);

        match rx.recv().await {
            Some(CoreEvent::TextDelta(t)) => assert_eq!(t, "hello"),
            _ => panic!("expected TextDelta"),
        }
        match rx.recv().await {
            Some(CoreEvent::ToolExecuting {
                tool_call_id, name, ..
            }) => {
                assert_eq!(tool_call_id, "call-7");
                assert_eq!(name, "shell");
            }
            _ => panic!("expected ToolExecuting"),
        }
        match rx.recv().await {
            Some(CoreEvent::TurnComplete) => {}
            _ => panic!("expected TurnComplete"),
        }
        assert!(rx.recv().await.is_none(), "stream should be closed");
    }

    #[tokio::test]
    async fn closed_receiver_makes_forward_one_return_false() {
        // Real failure mode: a previous version did `let _ = tx.send(...)`,
        // so the forwarder kept pulling from a dead stream forever. This
        // asserts the close signal is propagated.
        let client = dummy_client();
        let (tx, rx) = mpsc::channel::<CoreEvent>(1);
        drop(rx);

        let proceed = forward_one(
            pb(PbKind::TextDelta(TextDelta { text: "x".into() })),
            &client,
            "sess-1",
            &tx,
        )
        .await;
        assert!(
            !proceed,
            "forward_one must return false once the receiver is dropped"
        );
    }
}
