//! Production `ChatDispatcher` — routes admitted ingress through the
//! daemon's globally-installed [`crate::session_executor::SessionExecutor`]
//! and translates [`cortex::agent::AgentEvent`] into the OpenAI-shaped
//! [`AgentEventChunk`] stream the route layer expects.

use std::sync::Arc;

use async_trait::async_trait;
use cortex::agent::AgentEvent;
use cortex::ingress::AdmittedIngress;
use futures::stream::{self, BoxStream};
use futures::StreamExt;

use super::dispatcher::{AgentEventChunk, ChatDispatcher};
use crate::session_executor::{self, InboundTurn};

/// Production dispatcher backed by the daemon session executor.
///
/// Resolution order at dispatch time:
/// 1. Use the executor handle stashed at construction time, when present.
/// 2. Otherwise fall back to the process-global handle installed by
///    `borgd` via [`session_executor::set_global`].
///
/// The fallback exists so callers that never see the daemon handle
/// directly (e.g. `GatewayServer::run` constructing the OpenAI-compat
/// state before the orchestrator has installed the global) still wire
/// up correctly — the global is read fresh on each request.
#[derive(Default)]
pub struct AgentDispatcher {
    explicit: Option<session_executor::SessionExecutorHandle>,
}

impl AgentDispatcher {
    /// Build a dispatcher that always uses the process-global handle.
    pub fn new() -> Self {
        Self { explicit: None }
    }

    /// Build a dispatcher pinned to the supplied handle. Tests use this
    /// to bypass `set_global` (which is `OnceLock`-backed and can only
    /// fire once per process).
    pub fn with_executor(handle: session_executor::SessionExecutorHandle) -> Self {
        Self {
            explicit: Some(handle),
        }
    }

    fn executor(&self) -> Option<session_executor::SessionExecutorHandle> {
        self.explicit
            .clone()
            .or_else(|| session_executor::global().cloned())
    }
}

#[async_trait]
impl ChatDispatcher for AgentDispatcher {
    async fn dispatch(&self, admitted: AdmittedIngress) -> BoxStream<'static, AgentEventChunk> {
        let Some(executor) = self.executor() else {
            return stream::iter(vec![AgentEventChunk::Error(
                "session executor not installed".into(),
            )])
            .boxed();
        };

        let turn = InboundTurn {
            session_key: admitted.session_key.clone(),
            text: admitted.dispatch_text.clone(),
            images: vec![],
            kind: session_executor::TurnKind::User,
        };

        let receiver = match executor.start_turn(turn).await {
            Ok((rx, _cancel)) => rx,
            Err(e) => {
                return stream::iter(vec![AgentEventChunk::Error(format!(
                    "start_turn failed: {e}"
                ))])
                .boxed();
            }
        };

        translate_events(receiver).boxed()
    }
}

/// Drain the agent event channel, mapping each event to zero-or-one
/// [`AgentEventChunk`] and ensuring the stream terminates with `Done`
/// or `Error` (the OpenAI route relies on a terminal chunk to close
/// the SSE response).
fn translate_events(
    mut rx: tokio::sync::mpsc::Receiver<AgentEvent>,
) -> impl futures::Stream<Item = AgentEventChunk> {
    async_stream::stream! {
        let mut tool_seq: u32 = 0;
        let mut terminated = false;
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::TextDelta(s) => yield AgentEventChunk::Text(s),
                AgentEvent::TextRetracted { bytes } => yield AgentEventChunk::Retract(bytes),
                AgentEvent::ToolExecuting {
                    tool_call_id,
                    name,
                    args,
                } => {
                    tool_seq += 1;
                    let id = if tool_call_id.is_empty() {
                        // The cortex agent loop populates tool_call_id for
                        // every emitted ToolExecuting event. An empty value
                        // here means upstream regressed — surface the
                        // synthesized fallback so the regression doesn't
                        // hide in OpenAI-shaped traffic.
                        tracing::warn!(
                            tool = %name,
                            seq = tool_seq,
                            "ToolExecuting arrived without tool_call_id; falling back to call_<seq>"
                        );
                        format!("call_{tool_seq}")
                    } else {
                        tool_call_id
                    };
                    yield AgentEventChunk::ToolCall {
                        id,
                        name,
                        arguments: args,
                    };
                }
                AgentEvent::TurnComplete => {
                    terminated = true;
                    yield AgentEventChunk::Done;
                    break;
                }
                AgentEvent::Error(e) => {
                    terminated = true;
                    yield AgentEventChunk::Error(e);
                    break;
                }
                _ => {}
            }
        }
        if !terminated {
            // Channel dropped without a TurnComplete or Error — surface
            // a synthetic terminal chunk so the SSE response closes.
            yield AgentEventChunk::Error("agent stream closed unexpectedly".into());
        }
    }
}

/// Convenience constructor used by the gateway server when it has no
/// explicit handle to thread through. Returns an `Arc<dyn …>` that
/// matches the route state's expected type.
pub fn arc_dispatcher() -> Arc<dyn ChatDispatcher> {
    Arc::new(AgentDispatcher::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_executor::{InboundTurn, SessionExecutor};
    use cortex::ingress::{IngressMessage, IngressSource, MessageBody, PrincipalId};
    use std::sync::Mutex;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// Stub executor that scripts a fixed event sequence.
    struct StubExecutor {
        events: Mutex<Option<Vec<AgentEvent>>>,
        last_turn: Mutex<Option<InboundTurn>>,
    }

    #[async_trait]
    impl SessionExecutor for StubExecutor {
        async fn start_turn(
            &self,
            turn: InboundTurn,
        ) -> anyhow::Result<(mpsc::Receiver<AgentEvent>, CancellationToken)> {
            *self.last_turn.lock().expect("lock") = Some(turn);
            let (tx, rx) = mpsc::channel(16);
            let events = self.events.lock().expect("lock").take().unwrap_or_default();
            tokio::spawn(async move {
                for e in events {
                    if tx.send(e).await.is_err() {
                        return;
                    }
                }
            });
            Ok((rx, CancellationToken::new()))
        }
        async fn cancel(&self, _key: &str) -> bool {
            false
        }
        async fn cancel_all(&self) -> usize {
            0
        }
        async fn active_turns(&self) -> usize {
            0
        }
    }

    fn admit(text: &str, sender: &str) -> AdmittedIngress {
        let msg = IngressMessage {
            source: IngressSource::Http(sender.into()),
            principal: PrincipalId::new(format!("token:{sender}")),
            sender_id: sender.into(),
            thread_id: None,
            body: MessageBody::text(text),
            meta: serde_json::Value::Null,
            received_at: 0,
        };
        // Use the gate to produce a real `AdmittedIngress` so `session_key`
        // is computed exactly as production would.
        let gate =
            cortex::ingress::IngressGate::with_noop_audit(100, std::time::Duration::from_secs(60));
        gate.admit(msg).expect("admit")
    }

    #[tokio::test]
    async fn translates_text_deltas_to_text_chunks_then_done() {
        // Real failure mode: a regression that drops `TextDelta` (e.g.
        // routing it as a tool call) would silently break streaming.
        let stub = Arc::new(StubExecutor {
            events: Mutex::new(Some(vec![
                AgentEvent::TextDelta("hi ".into()),
                AgentEvent::TextDelta("there".into()),
                AgentEvent::TurnComplete,
            ])),
            last_turn: Mutex::new(None),
        });
        let dispatcher = AgentDispatcher::with_executor(stub.clone());
        let mut chunks: Vec<AgentEventChunk> = dispatcher
            .dispatch(admit("ignored", "alice"))
            .await
            .collect()
            .await;
        assert_eq!(
            chunks.pop(),
            Some(AgentEventChunk::Done),
            "must terminate with Done"
        );
        let joined: String = chunks
            .iter()
            .filter_map(|c| match c {
                AgentEventChunk::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(joined, "hi there");
    }

    #[tokio::test]
    async fn tool_executing_becomes_tool_call_chunk_with_stable_id_prefix() {
        // Real failure mode: a regression dropping `ToolExecuting` would
        // hide tool activity from OpenAI clients that render
        // `tool_calls` in the assistant message.
        let stub = Arc::new(StubExecutor {
            events: Mutex::new(Some(vec![
                AgentEvent::ToolExecuting {
                    tool_call_id: "call-stable-1".into(),
                    name: "memory_search".into(),
                    args: r#"{"q":"x"}"#.into(),
                },
                AgentEvent::TurnComplete,
            ])),
            last_turn: Mutex::new(None),
        });
        let dispatcher = AgentDispatcher::with_executor(stub);
        let chunks: Vec<AgentEventChunk> = dispatcher
            .dispatch(admit("search", "bob"))
            .await
            .collect()
            .await;
        // Expect: [ToolCall, Done]
        assert_eq!(chunks.len(), 2);
        match &chunks[0] {
            AgentEventChunk::ToolCall {
                id,
                name,
                arguments,
            } => {
                assert_eq!(id, "call-stable-1");
                assert_eq!(name, "memory_search");
                assert_eq!(arguments, r#"{"q":"x"}"#);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        assert_eq!(chunks[1], AgentEventChunk::Done);
    }

    #[tokio::test]
    async fn channel_closed_without_terminal_emits_synthetic_error() {
        // Real failure mode: agent event sender dropping mid-turn (panic,
        // shutdown) without TurnComplete/Error would leave the SSE stream
        // open forever. The synthetic Error keeps clients from hanging.
        let stub = Arc::new(StubExecutor {
            events: Mutex::new(Some(vec![AgentEvent::TextDelta("partial".into())])),
            last_turn: Mutex::new(None),
        });
        let dispatcher = AgentDispatcher::with_executor(stub);
        let chunks: Vec<AgentEventChunk> = dispatcher
            .dispatch(admit("x", "carol"))
            .await
            .collect()
            .await;
        let last = chunks.last().expect("at least one chunk");
        assert!(
            matches!(last, AgentEventChunk::Error(_)),
            "expected synthetic Error terminator, got: {last:?}"
        );
    }

    #[tokio::test]
    async fn no_executor_yields_single_error_chunk() {
        // The route layer expects a terminal chunk; the no-executor path
        // must return one Error chunk rather than an empty stream.
        let dispatcher = AgentDispatcher::new();
        let chunks: Vec<AgentEventChunk> = dispatcher
            .dispatch(admit("x", "dave"))
            .await
            .collect()
            .await;
        assert_eq!(chunks.len(), 1);
        assert!(matches!(chunks[0], AgentEventChunk::Error(_)));
    }
}
