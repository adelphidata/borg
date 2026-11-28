//! The wire-log ceiling only removes events. Under a `RUST_LOG` quieter than
//! the ceiling, the capped modules stay as quiet as the operator asked.
//!
//! Single-test integration binary: the subscriber is process-global.

#![allow(clippy::expect_used)]

use std::sync::{Arc, Mutex};

use tracing::{Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

#[derive(Clone, Default)]
struct RecordingLayer {
    seen: Arc<Mutex<Vec<(String, Level)>>>,
}

impl<S: Subscriber> Layer<S> for RecordingLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        self.seen
            .lock()
            .expect("recording mutex")
            .push((meta.target().to_string(), *meta.level()));
    }
}

#[test]
fn the_ceiling_never_makes_a_quiet_filter_louder() {
    let recorder = RecordingLayer::default();
    let seen = Arc::clone(&recorder.seen);
    let filter = cortex::telemetry::log_filter(Some("error"), "info");
    let subscriber = cortex::telemetry::build_subscriber(Vec::new(), filter, Box::new(recorder));
    tracing::subscriber::set_global_default(subscriber).expect("install global subscriber");

    assert_eq!(
        tracing::level_filters::LevelFilter::current(),
        tracing::level_filters::LevelFilter::ERROR,
        "the process-wide maximum level follows RUST_LOG, not the ceiling"
    );

    tracing::info!(target: "tungstenite::protocol", "below the ceiling, above RUST_LOG");
    tracing::warn!(target: "tungstenite", "connection reset");
    tracing::warn!(target: "cortex::agent", "slow turn");
    tracing::error!(target: "tungstenite::protocol", "broken pipe");

    let seen = seen.lock().expect("recording mutex");
    assert_eq!(
        seen.as_slice(),
        &[("tungstenite::protocol".to_string(), Level::ERROR)],
    );
}
