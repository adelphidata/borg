//! Production [`AdmitSink`] — hands admitted ingress to the daemon's
//! globally-installed [`crate::session_executor::SessionExecutor`] and
//! drains the resulting agent events to completion in the background.
//!
//! Generic webhooks are fire-and-forget: the route returns 202 to the
//! upstream service before the agent finishes. The sink therefore
//! spawns a detached task per ingress and logs progress/errors via
//! `tracing` rather than returning them to the HTTP caller. Output
//! delivery (if any) is the agent's own responsibility — e.g. the
//! `schedule` tool may post replies to a configured channel.

use std::sync::Arc;

use async_trait::async_trait;
use cortex::agent::AgentEvent;
use cortex::ingress::AdmittedIngress;

use super::routes::AdmitSink;
use crate::session_executor::{self, InboundTurn};

/// Production sink backed by the daemon session executor. See
/// [`crate::openai_compat::AgentDispatcher`] for the analogous resolution
/// rules (explicit handle wins over the process-global one).
#[derive(Default)]
pub struct AgentSink {
    explicit: Option<session_executor::SessionExecutorHandle>,
}

impl AgentSink {
    /// Build a sink that always uses the process-global executor handle.
    pub fn new() -> Self {
        Self { explicit: None }
    }

    /// Build a sink pinned to the supplied handle. Tests use this to
    /// avoid the `OnceLock`-backed `set_global` install.
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
impl AdmitSink for AgentSink {
    async fn handle(&self, admitted: AdmittedIngress) {
        let Some(executor) = self.executor() else {
            tracing::warn!(
                source = %admitted.message.source.audit_label(),
                session_key = %admitted.session_key,
                "generic webhook admitted but no session executor installed; dropping"
            );
            return;
        };

        let turn = InboundTurn {
            session_key: admitted.session_key.clone(),
            text: admitted.dispatch_text.clone(),
            images: vec![],
            kind: session_executor::TurnKind::User,
        };
        let session_key = admitted.session_key.clone();
        let source = admitted.message.source.audit_label();

        match executor.start_turn(turn).await {
            Ok((rx, _cancel)) => {
                tokio::spawn(drain_to_log(rx, session_key, source));
            }
            Err(e) => {
                tracing::warn!(
                    %session_key,
                    %source,
                    "generic webhook start_turn failed: {e:#}"
                );
            }
        }
    }
}

async fn drain_to_log(
    mut rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    session_key: String,
    source: String,
) {
    while let Some(evt) = rx.recv().await {
        match evt {
            AgentEvent::Error(e) => {
                tracing::warn!(%session_key, %source, "agent error: {e}");
            }
            AgentEvent::TurnComplete => {
                tracing::debug!(%session_key, %source, "agent turn complete");
                return;
            }
            _ => {}
        }
    }
    tracing::debug!(%session_key, %source, "agent stream closed");
}

/// Convenience constructor: `Arc<dyn AdmitSink>` ready to drop into
/// [`super::routes::WebhookState`].
pub fn arc_sink() -> Arc<dyn AdmitSink> {
    Arc::new(AgentSink::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_executor::{InboundTurn, SessionExecutor};
    use cortex::ingress::{IngressMessage, IngressSource, MessageBody, PrincipalId};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// Stub executor that records the turn and emits a scripted event
    /// sequence. A counter tracks `start_turn` invocations so the sink
    /// can assert dispatch happened exactly once.
    struct StubExecutor {
        turns: AtomicU32,
        last: Mutex<Option<InboundTurn>>,
        events: Mutex<Option<Vec<AgentEvent>>>,
    }

    #[async_trait]
    impl SessionExecutor for StubExecutor {
        async fn start_turn(
            &self,
            turn: InboundTurn,
        ) -> anyhow::Result<(mpsc::Receiver<AgentEvent>, CancellationToken)> {
            self.turns.fetch_add(1, Ordering::SeqCst);
            *self.last.lock().expect("lock") = Some(turn);
            let (tx, rx) = mpsc::channel(16);
            let evs = self.events.lock().expect("lock").take().unwrap_or_default();
            tokio::spawn(async move {
                for e in evs {
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

    fn admit(text: &str, sender: &str, thread: Option<&str>) -> AdmittedIngress {
        let msg = IngressMessage {
            source: IngressSource::Webhook("test".into()),
            principal: PrincipalId::new(format!("webhook:{sender}")),
            sender_id: sender.into(),
            thread_id: thread.map(str::to_string),
            body: MessageBody::text(text),
            meta: serde_json::Value::Null,
            received_at: 0,
        };
        let gate =
            cortex::ingress::IngressGate::with_noop_audit(100, std::time::Duration::from_secs(60));
        gate.admit(msg).expect("admit")
    }

    #[tokio::test]
    async fn dispatches_through_executor_with_correct_session_key() {
        // Real failure mode: a regression that mis-builds `session_key`
        // (e.g. drops the thread id) would split conversations into
        // separate sessions, losing context.
        let stub = Arc::new(StubExecutor {
            turns: AtomicU32::new(0),
            last: Mutex::new(None),
            events: Mutex::new(Some(vec![AgentEvent::TurnComplete])),
        });
        let sink = AgentSink::with_executor(stub.clone());
        sink.handle(admit("hello", "user-1", Some("thread-7")))
            .await;
        // `turns` and `last` are set synchronously inside `start_turn`
        // *before* the spawn — assertions here are safe today. A short
        // retry loop hardens against a future refactor that defers the
        // recording or moves it into the spawned drain.
        for _ in 0..50 {
            if stub.turns.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(stub.turns.load(Ordering::SeqCst), 1);
        let last = stub.last.lock().expect("lock").clone().expect("turn");
        assert_eq!(last.text, "hello");
        assert_eq!(last.session_key, "user-1:thread-7");
    }

    /// Shared in-memory writer so a scoped subscriber's output is
    /// assertable. `drain_to_log`'s only contract is what it logs and
    /// when it stops, so the log text is the observable.
    #[derive(Clone, Default)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn drain_logs_agent_errors_and_stops_at_turn_complete() {
        // Contract: every AgentEvent::Error before TurnComplete is
        // warned (the only place webhook-turn failures surface), and
        // TurnComplete ends the drain even while the sender stays open.
        let writer = CaptureWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(writer.clone())
            .finish();

        let (tx, rx) = mpsc::channel::<AgentEvent>(8);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        tracing::subscriber::with_default(subscriber, || {
            rt.block_on(async {
                tx.send(AgentEvent::TextDelta("ignored".into()))
                    .await
                    .expect("send");
                tx.send(AgentEvent::Error("llm exploded".into()))
                    .await
                    .expect("send");
                tx.send(AgentEvent::TurnComplete).await.expect("send");
                // `tx` stays alive: only the TurnComplete arm can end the
                // drain, so a deleted arm hangs here and trips the timeout.
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    drain_to_log(rx, "k1".into(), "webhook:test".into()),
                )
                .await
                .expect("drain must return at TurnComplete");
            });
        });
        drop(tx);

        let logs = String::from_utf8(writer.0.lock().expect("lock").clone()).expect("utf8");
        assert!(
            logs.contains("agent error") && logs.contains("llm exploded"),
            "agent errors must be warned with their message: {logs}"
        );
        assert!(
            logs.contains("agent turn complete"),
            "completion must be logged: {logs}"
        );
    }

    #[tokio::test]
    async fn no_executor_logs_and_returns_without_panic() {
        // Real failure mode: dispatch before the daemon has installed
        // its global executor (startup race) must not panic — the route
        // already returned 202 to the upstream service, so a panic here
        // would only crash the worker task.
        let sink = AgentSink::new();
        sink.handle(admit("x", "user-2", None)).await;
        // No assertions beyond not panicking; drained via tracing.
    }

    #[tokio::test]
    async fn start_turn_failure_is_logged_not_propagated() {
        // Real failure mode: a regression that propagated the error
        // would bubble through `AdmitSink::handle`'s `()` return — but
        // the trait signature forbids that, so the only correct behavior
        // is to log and move on. Assert the call returns cleanly even
        // when the executor errors.
        struct ErroringExecutor;
        #[async_trait]
        impl SessionExecutor for ErroringExecutor {
            async fn start_turn(
                &self,
                _turn: InboundTurn,
            ) -> anyhow::Result<(mpsc::Receiver<AgentEvent>, CancellationToken)> {
                anyhow::bail!("simulated executor failure")
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
        let sink = AgentSink::with_executor(Arc::new(ErroringExecutor));
        sink.handle(admit("x", "user-3", None)).await;
    }
}
