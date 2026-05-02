//! Telemetry gRPC service.

use crate::cost_events::CostEventBus;
use protocols::telemetry::{
    telemetry_server::Telemetry, CostEvent as PbCostEvent, Empty,
};
use std::pin::Pin;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status as TStatus};

/// Telemetry service implementation.
pub struct TelemetrySvc {
    bus: CostEventBus,
}

impl TelemetrySvc {
    /// Construct a telemetry service around the daemon cost-event bus.
    pub fn new(bus: CostEventBus) -> Self {
        Self { bus }
    }
}

#[tonic::async_trait]
impl Telemetry for TelemetrySvc {
    type StreamCostEventsStream =
        Pin<Box<dyn Stream<Item = Result<PbCostEvent, TStatus>> + Send + 'static>>;

    async fn stream_cost_events(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<Self::StreamCostEventsStream>, TStatus> {
        let mut state_rx = self.bus.subscribe_state();
        let bus = self.bus.clone();
        let (tx, rx) = mpsc::channel::<Result<PbCostEvent, TStatus>>(16);

        tokio::spawn(async move {
            loop {
                let Some(inner) = state_rx.borrow().clone() else {
                    if state_rx.changed().await.is_err() {
                        return;
                    }
                    continue;
                };

                let mut sub = inner.tx.subscribe();
                loop {
                    tokio::select! {
                        changed = state_rx.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            break;
                        }
                        recv = sub.recv() => {
                            match recv {
                                Ok(evt) => {
                                    let pb = PbCostEvent {
                                        request_id: evt.request_id,
                                        session_id: evt.session_id,
                                        provider: evt.provider,
                                        model: evt.model,
                                        cost_usd: evt.cost_usd.unwrap_or_default(),
                                        cost_usd_present: evt.cost_usd_present,
                                        cost_source: evt.cost_source,
                                        prompt_tokens: evt.prompt_tokens,
                                        completion_tokens: evt.completion_tokens,
                                        timestamp_unix_ms: evt.timestamp_unix_ms,
                                        kind: evt.kind,
                                    };
                                    if tx.send(Ok(pb)).await.is_err() {
                                        return;
                                    }
                                }
                                Err(broadcast::error::RecvError::Lagged(n)) => {
                                    bus.record_dropped(n);
                                }
                                Err(broadcast::error::RecvError::Closed) => break,
                            }
                        }
                    }
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}
